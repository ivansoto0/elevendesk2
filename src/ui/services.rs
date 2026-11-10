//! State shared by every panel and dialog: the API client, the voice/model
//! catalogue, the audio engine and the screen reader announcer.

use std::{cell::RefCell, sync::mpsc::Sender};

use wxdragon::prelude::*;

use super::{announce::Announcer, widgets::Status};
use crate::{
	api::{
		ApiClient, ApiError, Voice,
		models::{self, ModelInfo},
	},
	audio::player::PlayerHandle,
	config, runtime,
};

pub const NO_KEY_MESSAGE: &str = "Add your ElevenLabs API key in Preferences first.";

pub enum AppEvent {
	Voices { result: Result<Vec<Voice>, ApiError>, preferred: Option<String> },
	Models(Result<Vec<ModelInfo>, ApiError>),
}

#[derive(Default)]
pub struct Catalog {
	pub voices: Vec<Voice>,
	pub tts_models: Vec<ModelInfo>,
	pub sts_models: Vec<ModelInfo>,
	/// True when GET /v1/models could not be reached and the built-in list is shown.
	pub models_offline: bool,
}

pub struct Services {
	pub frame: Frame,
	pub announcer: Announcer,
	pub player: PlayerHandle,
	pub catalog: RefCell<Catalog>,
	api: RefCell<Option<ApiClient>>,
	events: Sender<AppEvent>,
}

impl Services {
	pub fn new(frame: Frame, announcer: Announcer, player: PlayerHandle, events: Sender<AppEvent>) -> Services {
		let catalog = Catalog {
			tts_models: models::offline_text_to_speech_models(),
			sts_models: models::offline_voice_conversion_models(),
			models_offline: true,
			..Catalog::default()
		};
		let services =
			Services { frame, announcer, player, catalog: RefCell::new(catalog), api: RefCell::new(None), events };
		services.reload_api_key();
		services
	}

	pub fn reload_api_key(&self) {
		*self.api.borrow_mut() = config::load_api_key().map(|key| ApiClient::new(&key));
	}

	pub fn has_api_key(&self) -> bool {
		self.api.borrow().is_some()
	}

	pub fn api(&self) -> Option<ApiClient> {
		self.api.borrow().clone()
	}

	/// The client, or None after telling the user (through `status`) that a key is needed.
	pub fn api_or_say(&self, status: Status) -> Option<ApiClient> {
		let api = self.api();
		if api.is_none() {
			status.say(NO_KEY_MESSAGE);
		}
		api
	}

	pub fn reload_voices(&self, preferred: Option<String>) {
		let Some(api) = self.api() else { return };
		let events = self.events.clone();
		runtime::spawn(async move {
			let result = api.list_voices().await;
			let _ = events.send(AppEvent::Voices { result, preferred });
		});
	}

	pub fn reload_models(&self) {
		let Some(api) = self.api() else { return };
		let events = self.events.clone();
		runtime::spawn(async move {
			let _ = events.send(AppEvent::Models(api.list_models().await));
		});
	}

	pub fn reload_catalog(&self) {
		self.reload_models();
		self.reload_voices(None);
	}
}
