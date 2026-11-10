//! Speech to Speech tab: convert a recording or audio file into another voice.

use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::mpsc::Sender, thread};

use tokio::fs::read as read_file;
use wxdragon::prelude::*;

use super::{
	pipeline,
	services::Services,
	widgets::{self, Clip, ClipControls, Form, Job, Page, Status},
};
use crate::{
	api::{
		ApiError, Voice, file_name_of,
		models::{self, ModelInfo},
	},
	audio::recorder::Recorder,
	config, runtime,
};

enum StsEvent {
	Progress { job: u64, seconds: usize },
	Done { job: u64, format: String, result: Result<Vec<u8>, ApiError> },
	Recorded(Result<PathBuf, String>),
}

struct Inner {
	services: Rc<Services>,
	panel: Panel,
	status: Status,
	open: Button,
	record: Button,
	file_name: TextCtrl,
	voice: Choice,
	model: Choice,
	format: Choice,
	convert: Button,
	clip: ClipControls,
	voices: RefCell<Vec<Voice>>,
	models: RefCell<Vec<ModelInfo>>,
	path: RefCell<Option<PathBuf>>,
	recorder: RefCell<Option<Recorder>>,
	job: Job,
	sender: Sender<StsEvent>,
}

pub struct StsPanel {
	inner: Rc<Inner>,
	_inbox: super::inbox::Inbox<StsEvent>,
}

impl StsPanel {
	pub fn new(parent: Notebook, services: Rc<Services>) -> StsPanel {
		let panel = Panel::builder(&parent).build();
		let form = Form::new(&panel);
		let (open, record, file_name) = form.group("Source audio", false, |group| {
			let row = widgets::button_row();
			let open = group.button(row, "&Choose Audio File…");
			let record = group.button(row, "&Record");
			group.add_row(row);
			let file_name = group.read_only_line("Selected file", "No file selected");
			(open, record, file_name)
		});
		let (voice, model, format, convert) = form.group("Conversion", false, |group| {
			let voice = group.choice("&Target voice", &[]);
			let model = group.choice("&Model", &[]);
			let format = group.choice("O&utput format", &widgets::output_format_labels());
			let row = widgets::button_row();
			let convert = group.button(row, "Con&vert");
			group.add_row(row);
			(voice, model, format, convert)
		});
		convert.enable(false);
		let clip = form.group("Converted audio", false, |group| {
			ClipControls::new(group, services.clone(), "converted audio", "elevendesk-converted")
		});
		let status = form.status(services.announcer);
		clip.attach_status(status);
		form.finish();
		let saved = config::load();
		widgets::select_output_format(format, &saved.default_output_format);

		let (inner, inbox) = widgets::with_inbox(
			|sender| {
				Rc::new(Inner {
					services,
					panel,
					status,
					open,
					record,
					file_name,
					voice,
					model,
					format,
					convert,
					clip,
					voices: RefCell::new(Vec::new()),
					models: RefCell::new(Vec::new()),
					path: RefCell::new(None),
					recorder: RefCell::new(None),
					job: Job::default(),
					sender,
				})
			},
			Inner::handle,
		);
		open.on_click(widgets::handler(&inner, Inner::choose_file));
		inner.record.on_click(widgets::handler(&inner, Inner::toggle_recording));
		inner.convert.on_click(widgets::handler(&inner, Inner::convert_or_cancel));
		inner.on_catalog_changed();
		StsPanel { inner, _inbox: inbox }
	}
}

impl Inner {
	fn on_catalog_changed(&self) {
		let (voices, models) = {
			let catalog = self.services.catalog.borrow();
			(catalog.voices.clone(), catalog.sts_models.clone())
		};
		let previous = widgets::choice_item(self.voice, &self.voices).map(|voice| voice.id);
		let wanted = previous.unwrap_or_else(|| config::load().default_voice_id);
		widgets::fill_choice(self.voice, &voices.iter().map(|voice| voice.name.clone()).collect::<Vec<_>>());
		let position =
			voices.iter().position(|voice| voice.id == wanted).or(if voices.is_empty() { None } else { Some(0) });
		if let Some(index) = position {
			widgets::select_choice(self.voice, index);
		}
		*self.voices.borrow_mut() = voices;

		let previous_model = widgets::choice_item(self.model, &self.models).map(|model| model.id);
		widgets::fill_choice(self.model, &models.iter().map(|model| model.name.clone()).collect::<Vec<_>>());
		if !models.is_empty() {
			let selection =
				models::resolve_saved(&previous_model.unwrap_or_else(|| config::load().default_sts_model), &models);
			widgets::select_choice(self.model, selection.index);
			if let Some(notice) = selection.notice {
				self.status.say(&notice);
			}
		}
		*self.models.borrow_mut() = models;
	}

	fn choose_file(&self) {
		if self.recorder.borrow().is_some() {
			self.status.say("Stop recording before choosing a file.");
			return;
		}
		let files = widgets::open_files(&self.services.frame, "Choose audio to convert", widgets::AUDIO_FILTER, false);
		match files.into_iter().next() {
			Some(path) => {
				self.set_file(path, "Selected");
				self.convert.set_focus();
			}
			None => self.open.set_focus(),
		}
	}

	fn set_file(&self, path: PathBuf, verb: &str) {
		self.file_name.set_value(&path.to_string_lossy());
		self.status.set(&format!("{verb} {}.", file_name_of(&path)));
		*self.path.borrow_mut() = Some(path);
		self.convert.enable(true);
	}

	fn toggle_recording(&self) {
		if self.recorder.borrow().is_some() {
			self.stop_recording();
		} else {
			self.start_recording();
		}
	}

	fn start_recording(&self) {
		if self.job.is_running() {
			self.status.say("Wait for the conversion to finish, or cancel it, before recording.");
			return;
		}
		self.services.player.stop();
		match Recorder::start(&config::load().input_device) {
			Ok(recorder) => {
				*self.recorder.borrow_mut() = Some(recorder);
				self.record.set_label("Stop &recording");
				self.status.say("Recording.");
			}
			Err(message) => self.status.say(&message),
		}
	}

	fn stop_recording(&self) {
		let Some(recorder) = self.recorder.borrow_mut().take() else { return };
		self.record.set_label("&Record");
		self.status.set("Saving the recording…");
		let sender = self.sender.clone();
		// Writing the WAV file happens off the GUI thread.
		thread::spawn(move || {
			let _ = sender.send(StsEvent::Recorded(recorder.stop()));
		});
	}

	fn convert_or_cancel(&self) {
		if self.job.is_running() {
			self.cancel("Conversion cancelled.");
			return;
		}
		if self.recorder.borrow().is_some() {
			self.status.say("Stop recording before converting.");
			return;
		}
		let Some(path) = self.path.borrow().clone() else {
			self.status.say("Choose or record audio first.");
			return;
		};
		let Some(voice) = widgets::choice_item(self.voice, &self.voices) else {
			self.status.say_at(&self.voice, "Choose a target voice first.");
			return;
		};
		let Some(model) = widgets::choice_item(self.model, &self.models) else {
			self.status.say("Choose a model first.");
			return;
		};
		let Some(api) = self.services.api_or_say(self.status) else { return };
		let format = widgets::output_format_id(self.format).to_string();
		let job = self.job.begin();
		self.clip.set_clip(None);
		self.convert.set_label("Cancel con&version");
		widgets::set_enabled(self.record, false, &self.convert);
		self.status.say("Converting audio…");
		self.services.player.begin_stream(self.clip.listener());
		let (sender, player) = (self.sender.clone(), self.services.player.clone());
		let handle = runtime::spawn(async move {
			let result = async {
				let bytes = read_file(&path).await?;
				let stream = api.speech_to_speech_stream(bytes, &file_name_of(&path), &voice.id, &model.id).await?;
				let progress = sender.clone();
				let pcm = pipeline::pump_stream(stream, &player, move |seconds| {
					let _ = progress.send(StsEvent::Progress { job, seconds });
				})
				.await?;
				pipeline::encode_clip(pcm, format.clone()).await
			}
			.await;
			let _ = sender.send(StsEvent::Done { job, format, result });
		});
		self.job.attach(handle);
	}

	fn cancel(&self, message: &str) {
		if self.job.cancel() {
			self.services.player.stop();
		}
		self.reset_buttons();
		self.status.say(message);
	}

	fn reset_buttons(&self) {
		self.convert.set_label("Con&vert");
		self.record.enable(true);
	}

	fn handle(&self, event: StsEvent) {
		match event {
			StsEvent::Progress { job, seconds } if self.job.is_current(job) => {
				self.status.set(&format!("Converting… {seconds} seconds of audio received."));
			}
			StsEvent::Done { job, format, result } if self.job.finish(job) => {
				self.reset_buttons();
				match result {
					Ok(bytes) => {
						self.clip.set_clip(Some(Clip { bytes, format }));
						self.status.say("Conversion complete.");
					}
					Err(error) => {
						self.services.player.stop();
						self.status.say(&error.message);
					}
				}
			}
			StsEvent::Recorded(Ok(path)) => {
				let record_had_focus = self.record.has_focus();
				self.set_file(path, "Recorded");
				if record_had_focus {
					self.convert.set_focus();
				}
				self.status.say("Recording ready.");
			}
			StsEvent::Recorded(Err(message)) => self.status.say(&message),
			_ => {}
		}
	}
}

impl Page for StsPanel {
	fn panel(&self) -> Panel {
		self.inner.panel
	}

	fn focus_first(&self) {
		if self.inner.convert.is_enabled() {
			self.inner.convert.set_focus();
		} else {
			self.inner.open.set_focus();
		}
	}

	fn status_field(&self) -> TextCtrl {
		self.inner.status.field
	}

	fn primary_action(&self) {
		self.inner.convert_or_cancel();
	}

	fn toggle_play(&self) {
		self.inner.clip.toggle_play();
	}

	fn stop(&self) {
		if self.inner.job.is_running() {
			self.inner.cancel("Conversion cancelled.");
		} else {
			self.inner.clip.stop();
		}
	}

	fn save(&self) {
		self.inner.clip.save();
	}

	fn open_file(&self) {
		self.inner.choose_file();
	}

	fn on_catalog_changed(&self) {
		self.inner.on_catalog_changed();
	}
}
