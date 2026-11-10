mod announce;
mod clone;
mod design;
mod dub_languages;
mod dubbing;
mod history;
mod inbox;
mod library;
mod pipeline;
mod prefs;
mod pronunciation;
mod services;
mod sfx;
mod sts;
mod stt;
mod tts;
mod widgets;

use std::{
	cell::{Cell, RefCell},
	rc::Rc,
};

use announce::Announcer;
use inbox::Pump;
use services::{AppEvent, Services};
use widgets::Page;
use wxdragon::{event::event_data::KeyEventData, main as run_app, prelude::*};

use crate::{
	api::models::{self, ModelInfo},
	audio::{player::PlayerHandle, recorder},
	config,
};

const APP_NAME: &str = "ElevenDesk 2";

/// Every menu command. The label (with its accelerator) lives here once, so the
/// menus, the dispatcher and the keyboard help dialog can never disagree.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Action {
	Primary,
	Save,
	OpenFile,
	Exit,
	PlayPause,
	Stop,
	Tab(usize),
	GoToStatus,
	RepeatAnnouncement,
	VoiceLibrary,
	CloneVoice,
	History,
	Pronunciation,
	Refresh,
	Preferences,
	Shortcuts,
	About,
}

const TAB_TITLES: [&str; 6] =
	["Text to Speech", "Speech to Text", "Speech to Speech", "Sound Effects", "Dubbing", "Voice Design"];

struct MenuEntry {
	action: Action,
	label: &'static str,
	help: &'static str,
}

const fn entry(action: Action, label: &'static str, help: &'static str) -> MenuEntry {
	MenuEntry { action, label, help }
}

const FILE_MENU: &[MenuEntry] = &[
	entry(
		Action::Primary,
		"&Run Current Tab Action\tCtrl+Enter",
		"Generate, transcribe, convert or start, depending on the tab",
	),
	entry(Action::Save, "&Save Last Output…\tCtrl+S", "Save the audio or transcript from the current tab"),
	entry(Action::OpenFile, "&Open Audio File…\tCtrl+O", "Choose an audio or video file for the current tab"),
	entry(Action::Exit, "E&xit", "Close ElevenDesk 2"),
];
const PLAYBACK_MENU: &[MenuEntry] = &[
	entry(Action::PlayPause, "&Play or Pause\tCtrl+P", "Play, pause or resume the current tab's audio"),
	entry(Action::Stop, "&Stop\tCtrl+.", "Stop playback, or cancel a running generation"),
];
const GO_MENU: &[MenuEntry] = &[
	entry(Action::Tab(0), "Text to Speech\tCtrl+1", "Switch tab"),
	entry(Action::Tab(1), "Speech to Text\tCtrl+2", "Switch tab"),
	entry(Action::Tab(2), "Speech to Speech\tCtrl+3", "Switch tab"),
	entry(Action::Tab(3), "Sound Effects\tCtrl+4", "Switch tab"),
	entry(Action::Tab(4), "Dubbing\tCtrl+5", "Switch tab"),
	entry(Action::Tab(5), "Voice Design\tCtrl+6", "Switch tab"),
	entry(Action::GoToStatus, "Status Field\tF6", "Move focus to the current tab's status field"),
	entry(
		Action::RepeatAnnouncement,
		"Repeat Last Announcement\tCtrl+Shift+L",
		"Have the screen reader say the last status message again",
	),
];
const TOOLS_MENU: &[MenuEntry] = &[
	entry(Action::VoiceLibrary, "&Voice Library…", "Browse, preview, edit and delete voices"),
	entry(Action::CloneVoice, "&Clone Voice…", "Create an instant voice clone from audio files"),
	entry(Action::History, "Generation &History…", "Play, save or delete past generations"),
	entry(Action::Pronunciation, "&Pronunciation Dictionaries…", "List pronunciation dictionaries"),
	entry(Action::Refresh, "&Refresh Voices and Models\tCtrl+R", "Reload the voice and model lists from ElevenLabs"),
];
const SETTINGS_MENU: &[MenuEntry] =
	&[entry(Action::Preferences, "&Preferences…\tCtrl+,", "API key, defaults, folders and audio devices")];
const HELP_MENU: &[MenuEntry] = &[
	entry(Action::Shortcuts, "&Keyboard Shortcuts", "List the keyboard shortcuts"),
	entry(Action::About, "&About ElevenDesk 2", "Version information"),
];

const MENUS: &[(&str, &[MenuEntry])] = &[
	("&File", FILE_MENU),
	("Pla&yback", PLAYBACK_MENU),
	("G&o", GO_MENU),
	("Too&ls", TOOLS_MENU),
	("S&ettings", SETTINGS_MENU),
	("&Help", HELP_MENU),
];

fn menu_id(action: Action) -> i32 {
	let mut index = 0;
	for (_, entries) in MENUS {
		for entry in *entries {
			if entry.action == action {
				return ID_HIGHEST + 1 + index;
			}
			index += 1;
		}
	}
	unreachable!("every action is listed in MENUS")
}

fn action_for_id(id: i32) -> Option<Action> {
	MENUS.iter().flat_map(|(_, entries)| entries.iter()).map(|entry| entry.action).find(|action| menu_id(*action) == id)
}

fn build_menu_bar() -> MenuBar {
	let mut bar = MenuBar::builder();
	for (title, entries) in MENUS {
		let mut menu = Menu::builder();
		for entry in *entries {
			menu = menu.append_item(menu_id(entry.action), entry.label, entry.help);
		}
		bar = bar.append(menu.build(), title);
	}
	bar.build()
}

struct AppState {
	services: Rc<Services>,
	notebook: Notebook,
	pages: Vec<Rc<dyn Page>>,
	tts: Rc<tts::TtsPanel>,
	startup_done: Cell<bool>,
	/// Spoken summary of a voice reload only when the user asked for the reload.
	announce_voices: Cell<bool>,
}

impl AppState {
	fn current_page(&self) -> Option<&Rc<dyn Page>> {
		let index = self.notebook.selection();
		self.pages.get(usize::try_from(index).ok()?)
	}

	fn on_app_event(&self, event: AppEvent) {
		match event {
			AppEvent::Models(Ok(list)) => {
				self.apply_models(&list);
			}
			AppEvent::Models(Err(error)) => {
				self.services.catalog.borrow_mut().models_offline = true;
				self.services
					.announcer
					.say(&format!("Could not load the model list. Using the built-in list. {}", error.message));
			}
			AppEvent::Voices { result: Ok(voices), preferred } => {
				let count = voices.len();
				self.services.catalog.borrow_mut().voices = voices;
				self.notify_pages();
				if let Some(id) = preferred {
					self.tts.select_voice(&id);
				}
				if self.announce_voices.replace(false) {
					self.services.announcer.say(&format!("Voice list loaded: {count} voices."));
				}
			}
			AppEvent::Voices { result: Err(error), .. } => {
				self.services.announcer.say(&format!("Could not load voices. {}", error.message));
			}
		}
	}

	fn apply_models(&self, list: &[ModelInfo]) {
		let tts_models = models::text_to_speech_models(list);
		let sts_models = models::voice_conversion_models(list);
		{
			let mut catalog = self.services.catalog.borrow_mut();
			catalog.models_offline = tts_models.is_empty();
			if !tts_models.is_empty() {
				catalog.tts_models = tts_models;
			}
			if !sts_models.is_empty() {
				catalog.sts_models = sts_models;
			}
		}
		self.notify_pages();
	}

	fn notify_pages(&self) {
		for page in &self.pages {
			page.on_catalog_changed();
		}
	}

	fn dispatch(&self, action: Action) {
		let page = self.current_page().cloned();
		let tab = usize::try_from(self.notebook.selection()).unwrap_or(0);
		if !tab_supports(action, tab) {
			self.services.announcer.say(&format!("Not available on the {} tab.", TAB_TITLES[tab]));
			return;
		}
		match action {
			Action::Primary => page.iter().for_each(|page| page.primary_action()),
			Action::Save => page.iter().for_each(|page| page.save()),
			Action::OpenFile => page.iter().for_each(|page| page.open_file()),
			Action::Exit => self.services.frame.close(false),
			Action::PlayPause => page.iter().for_each(|page| page.toggle_play()),
			Action::Stop => page.iter().for_each(|page| page.stop()),
			Action::Tab(index) => {
				self.notebook.set_selection(index);
				if let Some(page) = self.pages.get(index) {
					page.focus_first();
				}
			}
			Action::GoToStatus => page.iter().for_each(|page| page.status_field().set_focus()),
			Action::RepeatAnnouncement => self.services.announcer.repeat_last(),
			Action::Refresh => {
				if self.services.has_api_key() {
					self.announce_voices.set(true);
					self.services.announcer.say("Refreshing voices and models.");
					self.services.reload_catalog();
				} else {
					self.services.announcer.say(services::NO_KEY_MESSAGE);
				}
			}
			Action::Preferences => {
				let saved = prefs::show(&self.services);
				self.after_preferences(saved);
			}
			Action::Shortcuts => {
				widgets::show_message(&self.services.frame, "Keyboard shortcuts", &shortcut_help());
				self.restore_focus();
			}
			Action::About => {
				widgets::show_message(
					&self.services.frame,
					"About ElevenDesk 2",
					&format!(
						"{APP_NAME} {}\nAn accessible desktop client for ElevenLabs. Not affiliated with or endorsed by ElevenLabs.",
						env!("CARGO_PKG_VERSION")
					),
				);
				self.restore_focus();
			}
			Action::VoiceLibrary => {
				if let Some(voice_id) = library::show(&self.services) {
					let use_voice = use_voice_callback(self.services.clone(), self.tts.clone(), self.notebook);
					use_voice(&voice_id);
					return;
				}
				self.restore_focus();
			}
			Action::CloneVoice => {
				let cloned = clone::show(&self.services);
				self.restore_focus();
				if let Some(voice_id) = cloned {
					self.services.reload_voices(Some(voice_id));
					self.services.announcer.say("Voice cloned.");
				}
			}
			Action::History => {
				history::show(&self.services);
				self.restore_focus();
			}
			Action::Pronunciation => {
				pronunciation::show(&self.services);
				self.restore_focus();
			}
		}
	}

	/// Focus first, then speak, so the focus change does not cut the message off.
	fn after_preferences(&self, saved: bool) {
		self.restore_focus();
		if saved {
			self.services.announcer.say(if self.services.has_api_key() {
				"Preferences saved."
			} else {
				"Preferences saved. No API key is set."
			});
		}
	}

	/// After a dialog closes, put focus back on the real content of the current tab.
	fn restore_focus(&self) {
		if let Some(page) = self.current_page() {
			page.focus_first();
		}
	}
}

fn shortcut_help() -> String {
	let mut lines = Vec::new();
	for (title, entries) in MENUS {
		for entry in *entries {
			if let Some((name, keys)) = entry.label.split_once('\t') {
				lines.push(format!("{keys}: {} ({})", name.replace(['&', '…'], ""), title.replace('&', "")));
			}
		}
	}
	lines.join("\n")
}

pub fn run() {
	if let Err(error) = run_app(build) {
		eprintln!("{APP_NAME} failed to start: {error}");
	}
}

fn build(app: App) {
	let frame = Frame::builder().with_title(APP_NAME).with_size(Size::new(900, 760)).build();
	app.set_top_window(&frame);
	frame.set_min_size(Size::new(640, 480));
	frame.set_menu_bar(build_menu_bar());
	let root = Panel::builder(&frame).build();
	let announcer = Announcer::new(&root);
	let notebook = Notebook::builder(&root).build();
	let player = PlayerHandle::spawn(config::load().output_device);

	let (state, app_inbox) = widgets::with_inbox(
		|app_events| {
			let services = Rc::new(Services::new(frame, announcer, player, app_events));
			let tts = Rc::new(tts::TtsPanel::new(notebook, services.clone()));
			let use_voice = use_voice_callback(services.clone(), tts.clone(), notebook);
			let pages: Vec<Rc<dyn Page>> = vec![
				tts.clone(),
				Rc::new(stt::SttPanel::new(notebook, services.clone())),
				Rc::new(sts::StsPanel::new(notebook, services.clone())),
				Rc::new(sfx::SfxPanel::new(notebook, services.clone())),
				Rc::new(dubbing::DubbingPanel::new(notebook, services.clone())),
				Rc::new(design::DesignPanel::new(notebook, services.clone(), use_voice)),
			];
			for (index, page) in pages.iter().enumerate() {
				notebook.add_page(&page.panel(), TAB_TITLES[index], index == 0, None);
			}
			Rc::new(AppState {
				services,
				notebook,
				pages,
				tts,
				startup_done: Cell::new(false),
				announce_voices: Cell::new(false),
			})
		},
		AppState::on_app_event,
	);

	let layout = BoxSizer::builder(Orientation::Vertical).build();
	layout.add(&notebook, 1, SizerFlag::Expand | SizerFlag::All, 4);
	root.set_sizer(layout, true);

	{
		let state = state.clone();
		frame.on_menu(move |event| {
			if let Some(action) = action_for_id(event.get_id()) {
				state.dispatch(action);
			}
		});
	}

	// Return and Delete on a list never reach the list itself on Windows (dialog-key processing eats them),
	// so they are routed from the frame before the control sees them.
	{
		let state = state.clone();
		frame.bind_internal(EventType::CHAR_HOOK, move |event| {
			let key = KeyEventData::new(event);
			let plain = !key.control_down() && !key.alt_down() && !key.meta_down() && !key.shift_down();
			let handled = plain
				&& match (key.get_key_code(), state.current_page()) {
					(Some(WXK_RETURN | WXK_NUMPAD_ENTER), Some(page)) => page.on_enter(),
					(Some(WXK_DELETE | WXK_NUMPAD_DELETE), Some(page)) => page.on_delete(),
					_ => false,
				};
			key.skip(!handled);
		});
	}

	let pump = Pump::start(&frame);
	let pump = Rc::new(RefCell::new(Some(pump)));
	{
		let state = state.clone();
		let keep_alive = RefCell::new(Some(app_inbox));
		frame.on_close(move |event| {
			// Stop callbacks and workers before native child controls are destroyed.
			if let Some(pump) = pump.borrow_mut().take() {
				pump.stop();
			}
			state.services.player.stop();
			recorder::remove_temp_recording();
			keep_alive.borrow_mut().take();
			event.skip(true);
		});
	}

	frame.center_on_screen();
	frame.show(true);
	state.pages[0].focus_first();
	state.startup();
}

impl AppState {
	/// First-run flow: no key yet means Preferences opens straight away.
	fn startup(&self) {
		if self.startup_done.replace(true) {
			return;
		}
		if self.services.has_api_key() {
			self.announce_voices.set(true);
			self.services.reload_catalog();
		} else {
			let saved = prefs::show(&self.services);
			self.after_preferences(saved);
		}
	}
}

/// "Use this voice" from the library or Voice Design: reload the list, select the voice in Text to Speech, and go there.
fn use_voice_callback(services: Rc<Services>, tts: Rc<tts::TtsPanel>, notebook: Notebook) -> Rc<dyn Fn(&str)> {
	Rc::new(move |voice_id: &str| {
		tts.select_voice(voice_id);
		services.reload_voices(Some(voice_id.to_string()));
		notebook.set_selection(0);
		tts.focus_first();
		services.announcer.say("Voice selected in Text to Speech.");
	})
}

/// Tab-specific commands: a key that does nothing on this tab says so instead of staying silent.
fn tab_supports(action: Action, tab: usize) -> bool {
	const TTS: usize = 0;
	const STT: usize = 1;
	const STS: usize = 2;
	const SFX: usize = 3;
	const DUBBING: usize = 4;
	match action {
		Action::Save => tab <= DUBBING,
		Action::OpenFile => matches!(tab, STT | STS | DUBBING),
		Action::PlayPause => matches!(tab, TTS | STS | SFX | DUBBING),
		_ => true,
	}
}
