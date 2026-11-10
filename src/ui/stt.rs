//! Speech to Text tab: pick an audio file, transcribe it, copy or save the transcript.

use std::{cell::RefCell, fs, path::PathBuf, rc::Rc, sync::mpsc::Sender};

use tokio::fs::read as read_file;
use wxdragon::{clipboard::Clipboard, prelude::*};

use super::{
	services::Services,
	widgets::{self, Form, Job, Page, Status},
};
use crate::{
	api::{
		ApiError, file_name_of,
		models::{self, ModelInfo},
	},
	config, runtime,
};

enum SttEvent {
	Done { job: u64, result: Result<String, ApiError> },
}

struct Inner {
	services: Rc<Services>,
	panel: Panel,
	status: Status,
	open: Button,
	file_name: TextCtrl,
	model: Choice,
	transcribe: Button,
	transcript: TextCtrl,
	copy: Button,
	save: Button,
	models: Vec<ModelInfo>,
	path: RefCell<Option<PathBuf>>,
	job: Job,
	sender: Sender<SttEvent>,
}

pub struct SttPanel {
	inner: Rc<Inner>,
	_inbox: super::inbox::Inbox<SttEvent>,
}

impl SttPanel {
	pub fn new(parent: Notebook, services: Rc<Services>) -> SttPanel {
		let panel = Panel::builder(&parent).build();
		let form = Form::new(&panel);
		let (open, file_name) = form.group("Source", false, |group| {
			let row = widgets::button_row();
			let open = group.button(row, "Choose &Audio File…");
			group.add_row(row);
			let file_name = group.read_only_line("Selected file", "No file selected");
			(open, file_name)
		});
		let models = models::speech_to_text_models();
		let names: Vec<String> = models.iter().map(|model| model.name.clone()).collect();
		let saved = config::load().default_stt_model;
		let (model, transcribe) = form.group("Options", false, |group| {
			let model = group.choice("&Model", &names);
			widgets::select_choice(model, models.iter().position(|model| model.id == saved).unwrap_or(0));
			let row = widgets::button_row();
			let transcribe = group.button(row, "&Transcribe");
			group.add_row(row);
			(model, transcribe)
		});
		transcribe.enable(false);
		let (transcript, copy, save) = form.group("Transcript", true, |group| {
			let transcript = group.area("T&ranscript", true);
			let row = widgets::button_row();
			let copy = group.button(row, "&Copy");
			let save = group.button(row, "&Save Transcript…");
			group.add_row(row);
			(transcript, copy, save)
		});
		copy.enable(false);
		save.enable(false);
		let status = form.status(services.announcer);
		form.finish();

		let (inner, inbox) = widgets::with_inbox(
			|sender| {
				Rc::new(Inner {
					services,
					panel,
					status,
					open,
					file_name,
					model,
					transcribe,
					transcript,
					copy,
					save,
					models,
					path: RefCell::new(None),
					job: Job::default(),
					sender,
				})
			},
			Inner::handle,
		);
		open.on_click(widgets::handler(&inner, Inner::choose_file));
		inner.transcribe.on_click(widgets::handler(&inner, Inner::transcribe_or_cancel));
		inner.copy.on_click(widgets::handler(&inner, Inner::copy_transcript));
		inner.save.on_click(widgets::handler(&inner, Inner::save_transcript));
		inner.model.on_selection_changed({
			let inner = Rc::downgrade(&inner);
			move |_| {
				if let Some(inner) = inner.upgrade() {
					inner.remember_model();
				}
			}
		});
		SttPanel { inner, _inbox: inbox }
	}
}

impl Inner {
	fn choose_file(&self) {
		let files = widgets::open_files(
			&self.services.frame,
			"Choose an audio file to transcribe",
			widgets::AUDIO_FILTER,
			false,
		);
		match files.into_iter().next() {
			Some(path) => {
				self.set_file(path);
				self.transcribe.set_focus();
			}
			None => self.open.set_focus(),
		}
	}

	fn set_file(&self, path: PathBuf) {
		self.file_name.set_value(&path.to_string_lossy());
		self.status.set(&format!("Selected {}.", file_name_of(&path)));
		*self.path.borrow_mut() = Some(path);
		self.transcribe.enable(true);
	}

	fn remember_model(&self) {
		if let Some(model) = widgets::choice_selection(self.model).and_then(|index| self.models.get(index))
			&& let Err(error) = config::update(|settings| settings.default_stt_model.clone_from(&model.id))
		{
			self.status.set(&error);
		}
	}

	fn transcribe_or_cancel(&self) {
		if self.job.is_running() {
			self.cancel("Transcription cancelled.");
			return;
		}
		let Some(path) = self.path.borrow().clone() else {
			self.status.say("Choose an audio file first.");
			return;
		};
		let Some(api) = self.services.api_or_say(self.status) else { return };
		let model_id = widgets::choice_selection(self.model)
			.and_then(|index| self.models.get(index))
			.map(|model| model.id.clone())
			.unwrap_or_default();
		let job = self.job.begin();
		self.transcribe.set_label("Cancel &transcription");
		self.status.say("Transcribing…");
		let sender = self.sender.clone();
		let handle = runtime::spawn(async move {
			let result = async {
				let bytes = read_file(&path).await?;
				api.speech_to_text(bytes, &file_name_of(&path), &model_id).await
			}
			.await;
			let _ = sender.send(SttEvent::Done { job, result });
		});
		self.job.attach(handle);
	}

	fn cancel(&self, message: &str) {
		self.job.cancel();
		self.reset_button();
		self.status.say(message);
	}

	fn reset_button(&self) {
		self.transcribe.set_label("&Transcribe");
	}

	fn handle(&self, event: SttEvent) {
		let SttEvent::Done { job, result } = event;
		if !self.job.finish(job) {
			return;
		}
		let button_had_focus = self.transcribe.has_focus();
		self.reset_button();
		match result {
			Ok(text) => {
				let words = text.split_whitespace().count();
				self.transcript.set_value(&text);
				self.copy.enable(!text.is_empty());
				self.save.enable(!text.is_empty());
				// Only follow the result with focus if the user is still waiting on the button.
				if button_had_focus {
					self.transcript.set_focus();
				}
				self.status.say(&format!("Transcription complete: {words} words."));
			}
			Err(error) => self.status.say(&error.message),
		}
	}

	fn copy_transcript(&self) {
		if Clipboard::get().set_text(&self.transcript.get_value()) {
			self.status.say("Transcript copied to the clipboard.");
		} else {
			self.status.say("The clipboard could not be opened.");
		}
	}

	fn save_transcript(&self) {
		let text = self.transcript.get_value();
		if text.is_empty() {
			self.status.say("There is no transcript to save yet.");
			return;
		}
		let stem = self
			.path
			.borrow()
			.as_ref()
			.and_then(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
			.unwrap_or_else(|| "transcript".into());
		let save_had_focus = self.save.has_focus();
		let saved = widgets::save_file(
			&self.services.frame,
			"Save transcript",
			&format!("{stem}.txt"),
			"Text files (*.txt)|*.txt|All files|*.*",
		)
		.map(|path| match fs::write(&path, text) {
			Ok(()) => format!("Transcript saved to {}.", path.display()),
			Err(error) => format!("The transcript could not be saved: {error}"),
		});
		if save_had_focus {
			self.save.set_focus();
		}
		if let Some(message) = saved {
			self.status.say(&message);
		}
	}
}

impl Page for SttPanel {
	fn panel(&self) -> Panel {
		self.inner.panel
	}

	fn focus_first(&self) {
		// Transcribe is disabled until a file is chosen, and a disabled control cannot take focus.
		if self.inner.transcribe.is_enabled() {
			self.inner.transcribe.set_focus();
		} else {
			self.inner.open.set_focus();
		}
	}

	fn status_field(&self) -> TextCtrl {
		self.inner.status.field
	}

	fn primary_action(&self) {
		self.inner.transcribe_or_cancel();
	}

	fn stop(&self) {
		if self.inner.job.is_running() {
			self.inner.cancel("Transcription cancelled.");
		}
	}

	fn save(&self) {
		self.inner.save_transcript();
	}

	fn open_file(&self) {
		self.inner.choose_file();
	}
}
