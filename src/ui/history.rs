//! Generation History dialog: play, save or delete earlier ElevenLabs generations.

use std::{
	cell::{Cell, RefCell},
	collections::HashMap,
	rc::Rc,
	sync::mpsc::Sender,
};

use bytes::Bytes;
use wxdragon::prelude::*;

use super::{
	announce::Announcer,
	services::Services,
	widgets::{self, Form, PlayState, Status},
};
use crate::{
	api::{ApiResult, HistoryItem},
	audio::{encode::Playable, player::PlayerEvent},
	runtime,
};

const TEXT_PREVIEW_CHARS: usize = 80;

enum HistoryEvent {
	Loaded(ApiResult<Vec<HistoryItem>>),
	Audio { id: String, result: ApiResult<Bytes> },
	Deleted { id: String, result: ApiResult<()> },
	Player(PlayerEvent),
}

struct State {
	services: Rc<Services>,
	dialog: Dialog,
	status: Status,
	list: ListBox,
	play: Button,
	items: RefCell<Vec<HistoryItem>>,
	cache: RefCell<HashMap<String, Bytes>>,
	/// The download the user is waiting on (item id, save instead of play). Later requests replace it.
	awaiting: RefCell<Option<(String, bool)>>,
	playback: Cell<PlayState>,
	sender: Sender<HistoryEvent>,
}

fn row_text(item: &HistoryItem) -> String {
	let text: String = item.text.chars().take(TEXT_PREVIEW_CHARS).collect();
	format!("{}, {}, {}. {}", widgets::format_date(item.date_unix), item.voice_name, item.model_id, text)
}

pub fn show(services: &Rc<Services>) {
	let dialog = Dialog::builder(&services.frame, "Generation History").build();
	let announcer = Announcer::new(&dialog);
	let form = Form::new(&dialog);
	let list = form.list("Past &generations");
	let (play, stop, save, delete, refresh) = form.group("Selected generation", false, |group| {
		let row = widgets::button_row();
		let play = group.button(row, "&Play");
		let stop = group.button(row, "&Stop");
		let save = group.button(row, "Save &As…");
		let delete = group.button(row, "&Delete");
		let refresh = group.button(row, "&Refresh");
		group.add_row(row);
		(play, stop, save, delete, refresh)
	});
	let status = form.status(announcer);
	let row = widgets::button_row();
	let close = form.button(row, "Close");
	form.add_row(row);
	dialog.set_sizer_and_fit(form.sizer, true);
	dialog.set_size(Size::new(760, 520));
	dialog.set_escape_id(close.get_id());

	let (state, _inbox) = widgets::with_inbox(
		|sender| {
			Rc::new(State {
				services: services.clone(),
				dialog,
				status,
				list,
				play,
				items: RefCell::new(Vec::new()),
				cache: RefCell::new(HashMap::new()),
				awaiting: RefCell::new(None),
				playback: Cell::new(PlayState::Idle),
				sender,
			})
		},
		State::handle,
	);
	state.play.on_click(widgets::handler(&state, State::play_or_pause));
	stop.on_click(widgets::handler(&state, State::stop));
	save.on_click(widgets::handler(&state, State::save));
	delete.on_click(widgets::handler(&state, State::delete));
	refresh.on_click(widgets::handler(&state, State::load));
	close.on_click(move |_| dialog.end_modal(ID_CLOSE));
	state.load();
	state.list.set_focus();
	dialog.show_modal();
	state.services.player.stop();
	dialog.destroy();
}

impl State {
	fn load(&self) {
		let Some(api) = self.services.api_or_say(self.status) else { return };
		self.status.set("Loading history…");
		runtime::spawn_and_send(self.sender.clone(), async move { HistoryEvent::Loaded(api.list_history().await) });
	}

	fn selected(&self) -> Option<HistoryItem> {
		widgets::list_item(self.list, &self.items)
	}

	fn set_playback(&self, playback: PlayState) {
		self.playback.set(playback);
		self.play.set_label(playback.button_label());
	}

	fn play_or_pause(&self) {
		match self.playback.get() {
			PlayState::Playing => self.services.player.pause(),
			PlayState::Paused => self.services.player.resume(),
			PlayState::Idle => self.fetch(false),
		}
	}

	fn stop(&self) {
		if self.playback.get() != PlayState::Idle {
			self.services.player.stop();
			self.set_playback(PlayState::Idle);
			self.status.set("Playback stopped.");
		}
	}

	fn save(&self) {
		self.fetch(true);
	}

	fn fetch(&self, save: bool) {
		let Some(item) = self.selected() else {
			self.status.say("Select a generation first.");
			return;
		};
		if let Some(bytes) = self.cache.borrow().get(&item.id).cloned() {
			self.use_audio(&item, &bytes, save);
			return;
		}
		if self.awaiting.borrow().as_ref() == Some(&(item.id.clone(), save)) {
			return;
		}
		let Some(api) = self.services.api_or_say(self.status) else { return };
		self.status.set("Downloading the audio…");
		*self.awaiting.borrow_mut() = Some((item.id.clone(), save));
		let id = item.id.clone();
		runtime::spawn_and_send(self.sender.clone(), async move {
			let result = api.history_audio(&id).await;
			HistoryEvent::Audio { id, result }
		});
	}

	/// Clears and returns the wanted action if the user is still waiting on this item.
	fn take_awaiting(&self, id: &str) -> Option<bool> {
		let mut awaiting = self.awaiting.borrow_mut();
		match awaiting.as_ref() {
			Some((wanted, save)) if wanted == id => {
				let save = *save;
				*awaiting = None;
				Some(save)
			}
			_ => None,
		}
	}

	fn use_audio(&self, item: &HistoryItem, bytes: &Bytes, save: bool) {
		if save {
			let stem = widgets::safe_file_name(&format!(
				"{} {}",
				item.voice_name,
				item.id.chars().take(6).collect::<String>()
			));
			let saved = widgets::save_bytes(
				&self.dialog,
				"Save generation",
				&format!("{stem}.mp3"),
				"MP3 audio (*.mp3)|*.mp3|All files|*.*",
				bytes,
			);
			if let Some(message) = saved {
				self.status.say(&message);
			}
			return;
		}
		let sender = self.sender.clone();
		self.services.player.play_clip(
			Playable::Encoded(bytes.to_vec()),
			Box::new(move |event| {
				let _ = sender.send(HistoryEvent::Player(event));
			}),
		);
	}

	fn delete(&self) {
		let Some(item) = self.selected() else {
			self.status.say("Select a generation first.");
			return;
		};
		if !widgets::confirm(
			&self.dialog,
			"Delete generation",
			"Delete this generation from your ElevenLabs history? This cannot be undone.",
		) {
			self.list.set_focus();
			return;
		}
		let Some(api) = self.services.api_or_say(self.status) else { return };
		self.status.set("Deleting…");
		let id = item.id.clone();
		runtime::spawn_and_send(self.sender.clone(), async move {
			let result = api.delete_history_item(&id).await;
			HistoryEvent::Deleted { id, result }
		});
	}

	fn handle(&self, event: HistoryEvent) {
		match event {
			HistoryEvent::Loaded(Ok(items)) => {
				widgets::fill_list(self.list, &items.iter().map(row_text).collect::<Vec<_>>());
				widgets::select_row_within(self.list, 0, items.len());
				if items.is_empty() {
					self.status.say("Your history is empty.");
				} else {
					self.status.set(&format!("{} generations in your history.", items.len()));
				}
				*self.items.borrow_mut() = items;
			}
			HistoryEvent::Loaded(Err(error)) | HistoryEvent::Deleted { result: Err(error), .. } => {
				self.status.say(&error.message);
			}
			HistoryEvent::Audio { id, result: Ok(bytes) } => {
				let save = self.take_awaiting(&id);
				let item = self.items.borrow().iter().find(|item| item.id == id).cloned();
				if let (Some(save), Some(item)) = (save, item) {
					self.use_audio(&item, &bytes, save);
				}
				self.cache.borrow_mut().insert(id, bytes);
			}
			HistoryEvent::Audio { id, result: Err(error) } => {
				if self.take_awaiting(&id).is_some() {
					self.status.say(&format!("The audio is not available. {}", error.message));
				}
			}
			HistoryEvent::Deleted { id, result: Ok(()) } => {
				let selected = widgets::list_selection(self.list).unwrap_or(0);
				self.items.borrow_mut().retain(|item| item.id != id);
				let items = self.items.borrow();
				widgets::fill_list(self.list, &items.iter().map(row_text).collect::<Vec<_>>());
				widgets::select_row_within(self.list, selected, items.len());
				drop(items);
				self.list.set_focus();
				self.status.say("Generation deleted.");
			}
			HistoryEvent::Player(event) => {
				self.set_playback(PlayState::after(&event));
				if let PlayerEvent::Error(message) = &event {
					self.status.say(message);
				} else if let Some(note) = PlayState::note(&event) {
					self.status.set(note);
				}
			}
		}
	}
}
