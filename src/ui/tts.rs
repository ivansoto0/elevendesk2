//! Text to Speech tab: generate speech, hear it while it streams, then replay or save it.

use std::{cell::RefCell, rc::Rc, sync::mpsc::Sender};

use wxdragon::prelude::*;

use super::{
	inbox::Inbox,
	pipeline,
	services::Services,
	widgets::{self, Clip, ClipControls, Form, Job, Page, PreviewMessage, Status, VoicePreview},
};
use crate::{
	api::{
		ApiClient, ApiError, Voice, VoiceParams,
		models::{self, ModelInfo},
	},
	audio::player::PlayerHandle,
	config, runtime,
};

const FALLBACK_MODEL_ID: &str = "eleven_v3";

enum TtsEvent {
	Progress { generation: u64, seconds: usize },
	Done { generation: u64, result: Result<Vec<u8>, ApiError>, retry: Retry },
	Preview(PreviewMessage),
}

/// What is needed to re-run a failed request on a different model.
#[derive(Clone)]
struct Retry {
	text: String,
	voice_id: String,
	voice_name: String,
	model_id: String,
	params: VoiceParams,
	format: String,
}

struct Inner {
	services: Rc<Services>,
	status: Status,
	text: TextCtrl,
	voice: Choice,
	model: Choice,
	format: Choice,
	stability: Slider,
	similarity: Slider,
	style: Slider,
	speed: Slider,
	preview: VoicePreview<TtsEvent>,
	generate: Button,
	clip: ClipControls,
	panel: Panel,
	models: RefCell<Vec<ModelInfo>>,
	voices: RefCell<Vec<Voice>>,
	job: Job,
	sender: Sender<TtsEvent>,
}

pub struct TtsPanel {
	inner: Rc<Inner>,
	_inbox: Inbox<TtsEvent>,
}

fn voice_labels(voices: &[Voice]) -> Vec<String> {
	voices
		.iter()
		.map(|voice| {
			let duplicated = voices.iter().filter(|other| other.name == voice.name).count() > 1;
			if duplicated && !voice.category.is_empty() {
				format!("{} ({})", voice.name, voice.category)
			} else {
				voice.name.clone()
			}
		})
		.collect()
}

impl TtsPanel {
	pub fn new(parent: Notebook, services: Rc<Services>) -> TtsPanel {
		let panel = Panel::builder(&parent).build();
		let form = Form::new(&panel);
		let text = form.area("&Text to speak", false);
		let (voice, preview, model) = form.group("Voice", false, |group| {
			let voice = group.choice("Voice", &[]);
			let row = widgets::button_row();
			let preview = group.button(row, "Preview &Voice");
			group.add_row(row);
			let model = group.choice("&Model", &[]);
			(voice, preview, model)
		});
		let format =
			form.group("Output", false, |group| group.choice("O&utput format", &widgets::output_format_labels()));
		let (stability, similarity, style, speed) = form.group("Voice settings", false, |group| {
			(
				group.slider("Stab&ility, percent", 0, 100, 50),
				group.slider("Similarity &boost, percent", 0, 100, 75),
				group.slider("Style e&xaggeration, percent", 0, 100, 0),
				group.slider("Spee&d, percent of normal", 70, 120, 100),
			)
		});
		let row = widgets::button_row();
		let generate = form.button(row, "&Generate");
		form.add_row(row);
		let clip = form.group("Generated speech", false, |group| {
			ClipControls::new(group, services.clone(), "generated speech", "elevendesk-speech")
		});
		let status = form.status(services.announcer);
		clip.attach_status(status);
		form.finish();

		let (inner, inbox) = widgets::with_inbox(
			|sender| {
				let voice_preview = VoicePreview::new(
					preview,
					status,
					services.clone(),
					sender.clone(),
					TtsEvent::Preview,
					"voice preview",
				);
				Rc::new(Inner {
					services,
					status,
					text,
					voice,
					model,
					format,
					stability,
					similarity,
					style,
					speed,
					preview: voice_preview,
					generate,
					clip,
					panel,
					models: RefCell::new(Vec::new()),
					voices: RefCell::new(Vec::new()),
					job: Job::default(),
					sender,
				})
			},
			Inner::handle,
		);
		preview.on_click(widgets::handler(&inner, Inner::toggle_preview));
		inner.bind();
		inner.apply_defaults();
		inner.on_catalog_changed();
		TtsPanel { inner, _inbox: inbox }
	}

	pub fn select_voice(&self, voice_id: &str) {
		self.inner.select_voice_by_id(voice_id);
	}
}

impl Inner {
	fn bind(self: &Rc<Self>) {
		self.generate.on_click(widgets::handler(self, Inner::generate_or_cancel));
		let weak = Rc::downgrade(self);
		let weak_model = weak.clone();
		self.model.on_selection_changed(move |_| {
			if let Some(inner) = weak_model.upgrade() {
				inner.model_changed();
			}
		});
		let weak_voice = weak;
		self.voice.on_selection_changed(move |_| {
			if let Some(inner) = weak_voice.upgrade() {
				inner.remember_voice();
			}
		});
	}

	fn apply_defaults(&self) {
		widgets::select_output_format(self.format, &config::load().default_output_format);
	}

	fn selected_voice(&self) -> Option<Voice> {
		widgets::choice_item(self.voice, &self.voices)
	}

	fn selected_model(&self) -> Option<ModelInfo> {
		widgets::choice_item(self.model, &self.models)
	}

	fn remember_voice(&self) {
		if let Some(voice) = self.selected_voice()
			&& let Err(error) = config::update(|settings| settings.default_voice_id.clone_from(&voice.id))
		{
			self.status.set(&error);
		}
	}

	fn select_voice_by_id(&self, voice_id: &str) -> bool {
		let position = self.voices.borrow().iter().position(|voice| voice.id == voice_id);
		match position {
			Some(index) => {
				widgets::select_choice(self.voice, index);
				true
			}
			None => false,
		}
	}

	fn on_catalog_changed(&self) {
		let (voices, models, offline) = {
			let catalog = self.services.catalog.borrow();
			(catalog.voices.clone(), catalog.tts_models.clone(), catalog.models_offline)
		};
		let previous_voice = self.selected_voice().map(|voice| voice.id);
		let saved = config::load();
		widgets::fill_choice(self.voice, &voice_labels(&voices));
		*self.voices.borrow_mut() = voices;
		let wanted = previous_voice.unwrap_or(saved.default_voice_id);
		if !self.select_voice_by_id(&wanted) && !self.voices.borrow().is_empty() {
			widgets::select_choice(self.voice, 0);
		}

		let previous_model = self.selected_model().map(|model| model.id).unwrap_or(saved.default_model);
		widgets::fill_choice(self.model, &models.iter().map(|model| model.name.clone()).collect::<Vec<_>>());
		*self.models.borrow_mut() = models;
		let models = self.models.borrow();
		if !models.is_empty() {
			let selection = models::resolve_saved(&previous_model, &models);
			widgets::select_choice(self.model, selection.index);
			if let Some(notice) = selection.notice {
				self.status.say(&notice);
			} else if offline {
				self.status.set("Showing the built-in model list. The live list could not be loaded.");
			}
		}
		drop(models);
		self.model_changed();
	}

	/// Enables only the settings the chosen model supports and exposes its details to screen readers.
	fn model_changed(&self) {
		if let Some(model) = self.selected_model() {
			self.style.enable(model.can_use_style);
			let mut details = model.details();
			if !model.can_use_style {
				details.push_str(". Style exaggeration is not available for this model.");
			}
			self.model.set_accessibility_description(&details);
		}
	}

	fn voice_params(&self) -> VoiceParams {
		let supports_style = self.selected_model().is_none_or(|model| model.can_use_style);
		VoiceParams {
			stability: Some(widgets::fraction_from_percent(self.stability.value())),
			similarity_boost: Some(widgets::fraction_from_percent(self.similarity.value())),
			style: supports_style.then(|| widgets::fraction_from_percent(self.style.value())),
			speed: Some(widgets::fraction_from_percent(self.speed.value())),
			use_speaker_boost: None,
		}
	}

	fn generate_or_cancel(&self) {
		if self.job.is_running() {
			self.cancel_generation("Generation cancelled.");
		} else {
			self.start_generation();
		}
	}

	fn start_generation(&self) {
		let text = self.text.get_value();
		if text.trim().is_empty() {
			self.status.say_at(&self.text, "Enter some text to speak first.");
			return;
		}
		let Some(voice) = self.selected_voice() else {
			self.status
				.say_at(&self.voice, "Choose a voice first. If the list is empty, check your API key in Preferences.");
			return;
		};
		let Some(model) = self.selected_model() else {
			self.status.say("Choose a model first.");
			return;
		};
		if let Some(limit) = model.max_characters
			&& text.chars().count() as u64 > limit
		{
			self.status.say_at(
				&self.text,
				&format!("{} accepts up to {limit} characters per request. Shorten the text.", model.name),
			);
			return;
		}
		let format = widgets::output_format_id(self.format).to_string();
		self.remember_voice();
		let retry = Retry {
			text,
			voice_id: voice.id,
			voice_name: voice.name,
			model_id: model.id,
			params: self.voice_params(),
			format,
		};
		self.run(&retry);
	}

	fn run(&self, retry: &Retry) {
		let Some(api) = self.services.api_or_say(self.status) else { return };
		self.stop_generation();
		self.preview.stop();
		let generation = self.job.begin();
		self.clip.set_clip(None);
		self.generate.set_label("Cancel &generation");
		self.status.say("Generating speech…");
		self.services.player.begin_stream(self.clip.listener());
		let sender = self.sender.clone();
		let player = self.services.player.clone();
		let job = retry.clone();
		let handle = runtime::spawn(async move {
			let result = stream_speech(&api, &player, &job, &sender, generation).await;
			let _ = sender.send(TtsEvent::Done { generation, result, retry: job });
		});
		self.job.attach(handle);
	}

	fn stop_generation(&self) {
		if self.job.cancel() {
			self.services.player.stop();
		}
	}

	fn cancel_generation(&self, message: &str) {
		self.stop_generation();
		self.reset_generate_button();
		self.status.say(message);
	}

	fn reset_generate_button(&self) {
		self.generate.set_label("&Generate");
	}

	fn handle(&self, event: TtsEvent) {
		match event {
			TtsEvent::Progress { generation, seconds } if self.job.is_current(generation) => {
				self.status.set(&format!("Generating… {seconds} seconds of audio received."));
			}
			TtsEvent::Done { generation, result, retry } if self.job.finish(generation) => {
				self.finish(result, retry);
			}
			TtsEvent::Preview(message) => self.preview.handle(message),
			_ => {}
		}
	}

	fn finish(&self, result: Result<Vec<u8>, ApiError>, retry: Retry) {
		self.reset_generate_button();
		match result {
			Ok(bytes) => {
				self.clip.set_clip(Some(Clip { bytes, format: retry.format }));
				self.status.say("Speech ready.");
			}
			Err(error) => {
				self.services.player.stop();
				self.status.say(&error.message);
				self.offer_fallback_model(&error, retry);
			}
		}
	}

	/// Some older or cloned voices fail on newer models with HTTP 500; offer a model that works.
	fn offer_fallback_model(&self, error: &ApiError, retry: Retry) {
		if error.status != Some(500) || retry.model_id == FALLBACK_MODEL_ID {
			return;
		}
		let Some(fallback) = self.models.borrow().iter().find(|model| model.id == FALLBACK_MODEL_ID).cloned() else {
			return;
		};
		let question = format!(
			"ElevenLabs could not generate speech for {} with {}. Some older or cloned voices do not work with every model.\n\nSwitch to {} and try again?",
			retry.voice_name, retry.model_id, fallback.name
		);
		if widgets::confirm(&self.panel, "Voice and model compatibility", &question) {
			if let Some(index) = self.models.borrow().iter().position(|model| model.id == FALLBACK_MODEL_ID) {
				widgets::select_choice(self.model, index);
				self.model_changed();
			}
			self.run(&Retry { model_id: FALLBACK_MODEL_ID.to_string(), ..retry });
		} else {
			self.generate.set_focus();
		}
	}

	fn toggle_preview(&self) {
		if self.preview.stop() {
			return;
		}
		let Some(voice) = self.selected_voice() else {
			self.status.say("Choose a voice to preview.");
			return;
		};
		let Some(url) = voice.preview_url else {
			self.status.say("This voice has no preview sample.");
			return;
		};
		let Some(api) = self.services.api_or_say(self.status) else { return };
		self.preview.start(api, url);
	}
}

async fn stream_speech(
	api: &ApiClient,
	player: &PlayerHandle,
	job: &Retry,
	sender: &Sender<TtsEvent>,
	generation: u64,
) -> Result<Vec<u8>, ApiError> {
	let stream = api.text_to_speech_stream(&job.text, &job.voice_id, &job.model_id, &job.params).await?;
	let progress = sender.clone();
	let pcm = pipeline::pump_stream(stream, player, move |seconds| {
		let _ = progress.send(TtsEvent::Progress { generation, seconds });
	})
	.await?;
	pipeline::encode_clip(pcm, job.format.clone()).await
}

impl Page for TtsPanel {
	fn panel(&self) -> Panel {
		self.inner.panel
	}

	fn focus_first(&self) {
		self.inner.text.set_focus();
	}

	fn status_field(&self) -> TextCtrl {
		self.inner.status.field
	}

	fn primary_action(&self) {
		self.inner.generate_or_cancel();
	}

	fn toggle_play(&self) {
		self.inner.clip.toggle_play();
	}

	fn stop(&self) {
		if self.inner.preview.stop() {
			return;
		}
		if self.inner.job.is_running() {
			self.inner.cancel_generation("Generation cancelled.");
		} else {
			self.inner.clip.stop();
		}
	}

	fn save(&self) {
		self.inner.clip.save();
	}

	fn on_catalog_changed(&self) {
		self.inner.on_catalog_changed();
	}
}
