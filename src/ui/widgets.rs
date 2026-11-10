//! Layout helpers and the controls every panel shares.
//!
//! Accessibility rule baked into `Form`: each visible label is created
//! immediately before the control it describes, so Windows (and VoiceOver) pair
//! them as the accessible name, tab order matches reading order, and the `&`
//! mnemonic on the label gives the control an Alt+letter shortcut.

use std::{
	cell::{Cell, RefCell},
	fs,
	path::PathBuf,
	rc::{Rc, Weak},
	sync::mpsc::Sender,
};

use bytes::Bytes;
use chrono::{DateTime, Local, TimeZone};
use tokio::task::JoinHandle;
use wxdragon::{
	id::{ID_OK, ID_YES},
	prelude::*,
};

use super::{announce::Announcer, inbox::Inbox, services::Services};
use crate::{
	api::{ApiClient, ApiResult},
	audio::{
		encode::{self, Playable},
		player::{Listener, PlayerEvent},
	},
	config, runtime,
};

pub fn button_row() -> BoxSizer {
	BoxSizer::builder(Orientation::Horizontal).build()
}

pub const GAP: i32 = 8;
const TEXT_AREA_HEIGHT: i32 = 140;
const LIST_HEIGHT: i32 = 160;

fn flags_fill() -> SizerFlag {
	SizerFlag::Expand | SizerFlag::Left | SizerFlag::Right
}

pub struct Form<'a, W: WxWidget> {
	pub parent: &'a W,
	pub sizer: BoxSizer,
}

impl<'a, W: WxWidget> Form<'a, W> {
	pub fn new(parent: &'a W) -> Form<'a, W> {
		Form { parent, sizer: BoxSizer::builder(Orientation::Vertical).build() }
	}

	pub fn label(&self, text: &str) -> StaticText {
		let label = StaticText::builder(self.parent).with_label(text).build();
		self.sizer.add(&label, 0, flags_fill() | SizerFlag::Top, GAP);
		label
	}

	fn add_control(&self, control: &impl WxWidget, grow: bool) {
		self.sizer.add(control, i32::from(grow), flags_fill(), GAP);
	}

	pub fn line(&self, label: &str, value: &str) -> TextCtrl {
		self.label(label);
		let control = TextCtrl::builder(self.parent).with_value(value).build();
		self.add_control(&control, false);
		control
	}

	/// Multi-line text field that takes the spare vertical space.
	pub fn area(&self, label: &str, read_only: bool) -> TextCtrl {
		self.label(label);
		let mut style = TextCtrlStyle::MultiLine | TextCtrlStyle::WordWrap;
		if read_only {
			style |= TextCtrlStyle::ReadOnly;
		}
		let control =
			TextCtrl::builder(self.parent).with_style(style).with_size(Size::new(-1, TEXT_AREA_HEIGHT)).build();
		self.add_control(&control, true);
		control
	}

	pub fn read_only_line(&self, label: &str, value: &str) -> TextCtrl {
		self.label(label);
		let control = TextCtrl::builder(self.parent).with_value(value).with_style(TextCtrlStyle::ReadOnly).build();
		self.add_control(&control, false);
		control
	}

	pub fn password(&self, label: &str) -> TextCtrl {
		self.label(label);
		let control = TextCtrl::builder(self.parent).with_style(TextCtrlStyle::Password).build();
		self.add_control(&control, false);
		control
	}

	pub fn choice(&self, label: &str, items: &[String]) -> Choice {
		self.label(label);
		let control = Choice::builder(self.parent).with_choices(items.to_vec()).build();
		self.add_control(&control, false);
		control
	}

	/// Native slider with an integer range. The composite spin control was avoided on purpose: its inner
	/// edit field is exposed to screen readers under its current value instead of the label.
	pub fn slider(&self, label: &str, min: i32, max: i32, value: i32) -> Slider {
		self.label(label);
		let control = Slider::builder(self.parent).with_min_value(min).with_max_value(max).with_value(value).build();
		control.set_accessibility_label(&label.replace('&', ""));
		self.add_control(&control, false);
		control
	}

	pub fn list(&self, label: &str) -> ListBox {
		self.label(label);
		let control = ListBox::builder(self.parent).with_size(Size::new(-1, LIST_HEIGHT)).build();
		self.add_control(&control, true);
		control
	}

	pub fn check(&self, label: &str, value: bool) -> CheckBox {
		let control = CheckBox::builder(self.parent).with_label(label).with_value(value).build();
		self.sizer.add(&control, 0, flags_fill() | SizerFlag::Top, GAP);
		control
	}

	/// Creates a button and appends it to `row`; use `button_row` for the row itself.
	pub fn button(&self, row: BoxSizer, label: &str) -> Button {
		// The visible label is the accessible name (keeps Voice Control and toggling labels in sync).
		let button = Button::builder(self.parent).with_label(label).build();
		row.add(&button, 0, SizerFlag::Right, GAP);
		button
	}

	pub fn add_row(&self, row: BoxSizer) {
		self.sizer.add_sizer(&row, 0, flags_fill() | SizerFlag::Top | SizerFlag::Bottom, GAP);
	}

	pub fn status(&self, announcer: Announcer) -> Status {
		let field = self.read_only_line("Status", "Ready");
		Status { field, announcer }
	}

	/// A labelled group (a native group box, which screen readers announce when focus enters it).
	/// Everything inside must be created through the inner form so it is parented to the box.
	/// `grow` lets the group take spare vertical space.
	pub fn group<R>(&self, title: &str, grow: bool, build: impl FnOnce(&Form<StaticBox>) -> R) -> R {
		let frame = StaticBox::builder(self.parent).with_label(title).build();
		let inner = Form::new(&frame);
		let result = build(&inner);
		let outline = StaticBoxSizerBuilder::new_with_box(&frame, Orientation::Vertical).build();
		outline.add_sizer(&inner.sizer, 1, SizerFlag::Expand, 0);
		self.sizer.add_sizer(&outline, i32::from(grow), flags_fill() | SizerFlag::Top, GAP);
		result
	}

	pub fn finish(self) {
		self.parent.set_sizer(self.sizer, true);
	}
}

/// A read-only, focusable status line. Screen reader users can Tab to it (or press
/// F6) to read the current state; `say` also announces through the live region.
#[derive(Clone, Copy)]
pub struct Status {
	pub field: TextCtrl,
	announcer: Announcer,
}

impl Status {
	/// Visible/readable state without interrupting speech (progress, idle text).
	pub fn set(&self, message: &str) {
		// Rewriting a focused read-only field makes a screen reader re-read it, so skip no-ops.
		if self.field.get_value() != message {
			self.field.set_value(message);
		}
	}

	/// State change the user should hear now.
	pub fn say(&self, message: &str) {
		self.set(message);
		// If the user is reading the status field, its own change event already speaks.
		if !self.field.has_focus() {
			self.announcer.say(message);
		}
	}

	/// Focus first, then speak: a focus change after the announcement would cut it off.
	pub fn say_at(&self, control: &impl WxWidget, message: &str) {
		control.set_focus();
		self.say(message);
	}
}

pub fn show_message(parent: &dyn WxWidget, title: &str, message: &str) {
	MessageDialog::builder(parent, message, title)
		.with_style(MessageDialogStyle::OK | MessageDialogStyle::IconInformation | MessageDialogStyle::Centre)
		.build()
		.show_modal();
}

/// Yes/No question; "No" is the default so Enter never confirms something destructive.
pub fn confirm(parent: &dyn WxWidget, title: &str, message: &str) -> bool {
	MessageDialog::builder(parent, message, title)
		.with_style(
			MessageDialogStyle::YesNo
				| MessageDialogStyle::NoDefault
				| MessageDialogStyle::IconQuestion
				| MessageDialogStyle::Centre,
		)
		.build()
		.show_modal()
		== ID_YES
}

pub const AUDIO_FILTER: &str = "Audio files|*.mp3;*.wav;*.m4a;*.flac;*.ogg;*.opus;*.aac;*.wma|All files|*.*";
pub const AUDIO_VIDEO_FILTER: &str = "Audio and video files|*.mp3;*.wav;*.m4a;*.flac;*.ogg;*.opus;*.aac;*.wma;*.mp4;*.mov;*.avi;*.mkv;*.webm|All files|*.*";

pub fn open_files(parent: &dyn WxWidget, title: &str, filter: &str, multiple: bool) -> Vec<PathBuf> {
	let mut style = FileDialogStyle::Open | FileDialogStyle::FileMustExist;
	if multiple {
		style |= FileDialogStyle::Multiple;
	}
	let dialog = FileDialog::builder(parent)
		.with_message(title)
		.with_default_dir(&config::load().output_directory)
		.with_wildcard(filter)
		.with_style(style)
		.build();
	if dialog.show_modal() != ID_OK {
		return Vec::new();
	}
	dialog.get_paths().into_iter().map(PathBuf::from).collect()
}

pub fn save_file(parent: &dyn WxWidget, title: &str, default_name: &str, filter: &str) -> Option<PathBuf> {
	let dialog = FileDialog::builder(parent)
		.with_message(title)
		.with_default_dir(&config::load().output_directory)
		.with_default_file(default_name)
		.with_wildcard(filter)
		.with_style(FileDialogStyle::Save | FileDialogStyle::OverwritePrompt)
		.build();
	if dialog.show_modal() != ID_OK {
		return None;
	}
	dialog.get_path().map(PathBuf::from)
}

/// Asks where to save and writes `bytes`; returns the message to announce, or None if the user cancelled.
pub fn save_bytes(
	parent: &dyn WxWidget,
	title: &str,
	default_name: &str,
	filter: &str,
	bytes: &[u8],
) -> Option<String> {
	let path = save_file(parent, title, default_name, filter)?;
	Some(match fs::write(&path, bytes) {
		Ok(()) => format!("Saved to {}.", path.display()),
		Err(error) => format!("The file could not be saved: {error}"),
	})
}

/// Replaces characters Windows forbids in file names and the trailing dots and spaces it silently strips.
pub fn safe_file_name(name: &str) -> String {
	let cleaned: String =
		name.chars().map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '-' } else { c }).collect();
	cleaned.trim().trim_end_matches(['.', ' ']).to_string()
}

pub fn choose_directory(parent: &dyn WxWidget, title: &str, start: &str) -> Option<PathBuf> {
	let dialog = DirDialog::builder(parent, title, start).build();
	if dialog.show_modal() != ID_OK {
		return None;
	}
	dialog.get_path().map(PathBuf::from)
}

/// Slider positions are whole percents; the API wants fractions.
// reason: slider positions are small integers (0..=200), exact in f32.
#[allow(clippy::cast_precision_loss)]
pub fn fraction_from_percent(percent: i32) -> f32 {
	percent as f32 / 100.0
}

// reason: API fractions are 0..=2, so the rounded percent always fits in an i32.
#[allow(clippy::cast_possible_truncation)]
pub fn percent_from_fraction(fraction: f32) -> i32 {
	(fraction * 100.0).round() as i32
}

pub fn choice_selection(choice: Choice) -> Option<usize> {
	choice.get_selection().map(|index| index as usize)
}

pub fn output_format_labels() -> Vec<String> {
	encode::OUTPUT_FORMATS.iter().map(|format| format.label.to_string()).collect()
}

/// Selects the format with this id in a choice filled from `output_format_labels`; unknown ids select the default.
pub fn select_output_format(choice: Choice, id: &str) {
	select_choice(choice, encode::OUTPUT_FORMATS.iter().position(|format| format.id == id).unwrap_or(0));
}

pub fn output_format_id(choice: Choice) -> &'static str {
	encode::OUTPUT_FORMATS[choice_selection(choice).unwrap_or(0)].id
}

pub fn list_selection(list: ListBox) -> Option<usize> {
	list.get_selection().map(|index| index as usize)
}

/// The item behind the selected choice entry; `items` must be in the same order as the entries.
pub fn choice_item<T: Clone>(choice: Choice, items: &RefCell<Vec<T>>) -> Option<T> {
	items.borrow().get(choice_selection(choice)?).cloned()
}

/// The item behind the selected list row; `items` must be in the same order as the rows.
pub fn list_item<T: Clone>(list: ListBox, items: &RefCell<Vec<T>>) -> Option<T> {
	items.borrow().get(list_selection(list)?).cloned()
}

/// Selects row `index`, or the last row if the list got shorter; does nothing for an empty list.
pub fn select_row_within(list: ListBox, index: usize, len: usize) {
	if len > 0 {
		list.set_selection(u32::try_from(index.min(len - 1)).unwrap_or(u32::MAX), true);
	}
}

pub fn select_choice(choice: Choice, index: usize) {
	choice.set_selection(u32::try_from(index).unwrap_or(u32::MAX));
}

pub fn fill_choice(choice: Choice, items: &[String]) {
	choice.clear();
	for item in items {
		choice.append(item);
	}
}

pub fn fill_list(list: ListBox, items: &[String]) {
	list.clear();
	for item in items {
		list.append(item);
	}
}

#[derive(Clone)]
pub struct Clip {
	pub bytes: Vec<u8>,
	/// Output format id ("mp3_44100_128", "flac", ...), used for decoding and file naming.
	pub format: String,
}

impl Clip {
	pub fn extension(&self) -> &str {
		if self.format.starts_with("mp3") {
			"mp3"
		} else if self.format.starts_with("wav") {
			"wav"
		} else if self.format.starts_with("pcm") {
			"pcm"
		} else if self.format.starts_with("ulaw") {
			"ulaw"
		} else {
			&self.format
		}
	}

	pub fn playable(&self) -> Playable {
		encode::playable_from_output(self.bytes.clone(), &self.format)
	}
}

#[derive(Clone, Copy, PartialEq)]
pub enum PlayState {
	Idle,
	Playing,
	Paused,
}

impl PlayState {
	pub fn after(event: &PlayerEvent) -> PlayState {
		match event {
			PlayerEvent::Started | PlayerEvent::Resumed => PlayState::Playing,
			PlayerEvent::Paused => PlayState::Paused,
			PlayerEvent::Finished | PlayerEvent::Stopped | PlayerEvent::Error(_) => PlayState::Idle,
		}
	}

	/// Quiet status text for an event; errors are spoken by the caller instead.
	pub fn note(event: &PlayerEvent) -> Option<&'static str> {
		match event {
			PlayerEvent::Started | PlayerEvent::Resumed => Some("Playing."),
			PlayerEvent::Paused => Some("Paused."),
			PlayerEvent::Finished => Some("Playback finished."),
			PlayerEvent::Stopped | PlayerEvent::Error(_) => None,
		}
	}

	pub fn button_label(self) -> &'static str {
		match self {
			PlayState::Idle => "&Play",
			PlayState::Playing => "&Pause",
			PlayState::Paused => "Resume &playback",
		}
	}
}

struct ClipInner {
	play: Button,
	stop: Button,
	save: Button,
	noun: String,
	default_name: String,
	clip: RefCell<Option<Clip>>,
	state: Cell<PlayState>,
	services: Rc<Services>,
	status: Cell<Option<Status>>,
}

/// Play / Pause / Stop / Save for the most recent clip a panel produced.
/// Panels that stream audio while generating hand `listener()` to the player.
pub struct ClipControls {
	inner: Rc<ClipInner>,
	inbox: Inbox<PlayerEvent>,
}

impl ClipControls {
	pub fn new(form: &Form<impl WxWidget>, services: Rc<Services>, noun: &str, default_name: &str) -> ClipControls {
		let row = button_row();
		let play = form.button(row, "&Play");
		let stop = form.button(row, "&Stop");
		let save = form.button(row, "Save &As…");
		form.add_row(row);
		play.enable(false);
		stop.enable(false);
		save.enable(false);
		let inner = Rc::new(ClipInner {
			play,
			stop,
			save,
			noun: noun.to_string(),
			default_name: default_name.to_string(),
			clip: RefCell::new(None),
			state: Cell::new(PlayState::Idle),
			services,
			status: Cell::new(None),
		});
		let weak = Rc::downgrade(&inner);
		let inbox = Inbox::new(move |event| {
			if let Some(inner) = weak.upgrade() {
				inner.on_player_event(&event);
			}
		});
		let controls = ClipControls { inner, inbox };
		controls.bind();
		controls.inner.refresh_buttons();
		controls
	}

	fn bind(&self) {
		let weak: Weak<ClipInner> = Rc::downgrade(&self.inner);
		let sender = self.inbox.sender();
		self.inner.play.on_click({
			let weak = weak.clone();
			let sender = sender.clone();
			move |_| {
				if let Some(inner) = weak.upgrade() {
					inner.toggle_play(&sender);
				}
			}
		});
		self.inner.stop.on_click({
			let weak = weak.clone();
			move |_| {
				if let Some(inner) = weak.upgrade() {
					inner.stop_by_user();
				}
			}
		});
		self.inner.save.on_click(move |_| {
			if let Some(inner) = weak.upgrade() {
				inner.save();
			}
		});
	}

	/// Called once the panel's status field exists (it is created after the buttons so Tab order ends on it).
	pub fn attach_status(&self, status: Status) {
		self.inner.status.set(Some(status));
	}

	pub fn set_clip(&self, clip: Option<Clip>) {
		let has_clip = clip.is_some();
		*self.inner.clip.borrow_mut() = clip;
		self.inner.set_enabled(self.inner.save, has_clip);
		self.inner.refresh_buttons();
	}

	/// Listener for audio this panel streams itself (text/speech to speech).
	pub fn listener(&self) -> Listener {
		let sender = self.inbox.sender();
		Box::new(move |event| {
			let _ = sender.send(event);
		})
	}

	pub fn toggle_play(&self) {
		self.inner.toggle_play(&self.inbox.sender());
	}

	pub fn stop(&self) {
		self.inner.stop_by_user();
	}

	pub fn save(&self) {
		self.inner.save();
	}
}

impl ClipInner {
	fn say(&self, message: &str) {
		if let Some(status) = self.status.get() {
			status.say(message);
		}
	}

	fn note(&self, message: &str) {
		if let Some(status) = self.status.get() {
			status.set(message);
		}
	}

	fn refresh_buttons(&self) {
		let name = match self.state.get() {
			PlayState::Idle => format!("Play {}", self.noun),
			PlayState::Playing => format!("Pause {}", self.noun),
			PlayState::Paused => format!("Resume {}", self.noun),
		};
		self.play.set_label(self.state.get().button_label());
		self.play.set_accessibility_label(&name);
		let can_play = self.state.get() != PlayState::Idle || self.clip.borrow().is_some();
		set_enabled(self.stop, self.state.get() != PlayState::Idle, &self.play);
		self.set_enabled(self.play, can_play);
	}

	/// Disables without stranding focus: falls back to the status field, which exists once the panel is built.
	fn set_enabled(&self, button: Button, enabled: bool) {
		match self.status.get() {
			Some(status) => set_enabled(button, enabled, &status.field),
			None => button.enable(enabled),
		}
	}

	fn toggle_play(&self, sender: &Sender<PlayerEvent>) {
		match self.state.get() {
			PlayState::Playing => self.services.player.pause(),
			PlayState::Paused => self.services.player.resume(),
			PlayState::Idle => {
				let Some(clip) = self.clip.borrow().clone() else {
					self.say(&format!("There is no {} to play yet.", self.noun));
					return;
				};
				let sender = sender.clone();
				self.services.player.play_clip(
					clip.playable(),
					Box::new(move |event| {
						let _ = sender.send(event);
					}),
				);
				self.note(&format!("Loading {}…", self.noun));
			}
		}
	}

	fn stop_by_user(&self) {
		if self.state.get() != PlayState::Idle {
			self.services.player.stop();
			self.state.set(PlayState::Idle);
			self.refresh_buttons();
			self.note("Playback stopped.");
		}
	}

	fn on_player_event(&self, event: &PlayerEvent) {
		self.state.set(PlayState::after(event));
		if let PlayerEvent::Error(message) = event {
			self.say(message);
		} else if let Some(note) = PlayState::note(event) {
			self.note(note);
		}
		self.refresh_buttons();
	}

	fn save(&self) {
		let button_had_focus = self.save.has_focus();
		let Some(clip) = self.clip.borrow().clone() else {
			self.say(&format!("There is no {} to save yet.", self.noun));
			return;
		};
		let default_name = format!("{}.{}", self.default_name, clip.extension());
		let filter = format!(
			"{} (*.{})|*.{}|All files|*.*",
			clip.extension().to_uppercase(),
			clip.extension(),
			clip.extension()
		);
		let message =
			save_bytes(&self.services.frame, &format!("Save {}", self.noun), &default_name, &filter, &clip.bytes);
		if button_had_focus {
			self.save.set_focus();
		}
		if let Some(message) = message {
			self.say(&message);
		}
	}
}

/// What the main window needs from each tab so menus and shortcuts behave the
/// same everywhere. Every method has a harmless default.
pub trait Page {
	fn panel(&self) -> Panel;
	fn focus_first(&self);
	fn status_field(&self) -> TextCtrl;
	/// Ctrl+Enter: generate / transcribe / convert / start, whichever the tab does.
	fn primary_action(&self) {}
	fn toggle_play(&self) {}
	fn stop(&self) {}
	fn save(&self) {}
	fn open_file(&self) {}
	fn on_catalog_changed(&self) {}
	/// Enter pressed while a list on this tab has focus. Return true if handled.
	fn on_enter(&self) -> bool {
		false
	}
	/// Delete pressed while a list on this tab has focus. Return true if handled.
	fn on_delete(&self) -> bool {
		false
	}
}

/// Builds an event callback that calls `action` on `state` while it is still alive.
/// Weak so widget callbacks (owned by wx) never keep a panel's state alive forever.
pub fn handler<T: 'static, E>(state: &Rc<T>, action: fn(&T)) -> impl FnMut(E) + 'static {
	let weak = Rc::downgrade(state);
	move |_event: E| {
		if let Some(state) = weak.upgrade() {
			action(&state);
		}
	}
}

/// Builds a panel's shared state together with the inbox that feeds it worker results.
pub fn with_inbox<S: 'static, M: Send + 'static>(
	build: impl FnOnce(Sender<M>) -> Rc<S>,
	handle: fn(&S, M),
) -> (Rc<S>, Inbox<M>) {
	let slot: Rc<RefCell<Weak<S>>> = Rc::new(RefCell::new(Weak::new()));
	let handler_slot = slot.clone();
	let inbox = Inbox::new(move |message| {
		if let Some(state) = handler_slot.borrow().upgrade() {
			handle(&state, message);
		}
	});
	let state = build(inbox.sender());
	*slot.borrow_mut() = Rc::downgrade(&state);
	(state, inbox)
}

/// A panel's single cancellable background run. Worker events carry the id they were started with, so the
/// late result of a cancelled or superseded run is ignored.
#[derive(Default)]
pub struct Job {
	id: Cell<u64>,
	task: RefCell<Option<JoinHandle<()>>>,
}

impl Job {
	pub fn is_running(&self) -> bool {
		self.task.borrow().is_some()
	}

	/// Cancels any previous run and returns the id the new run's events must carry.
	pub fn begin(&self) -> u64 {
		self.cancel();
		self.id.get()
	}

	pub fn attach(&self, task: JoinHandle<()>) {
		*self.task.borrow_mut() = Some(task);
	}

	/// Aborts the run and invalidates its id. Returns whether a run was actually in progress.
	pub fn cancel(&self) -> bool {
		let task = self.task.borrow_mut().take();
		if let Some(task) = &task {
			task.abort();
		}
		self.id.set(self.id.get() + 1);
		task.is_some()
	}

	pub fn is_current(&self, id: u64) -> bool {
		id == self.id.get()
	}

	/// Call when the run's final event arrives; false means it belongs to a cancelled run and must be dropped.
	pub fn finish(&self, id: u64) -> bool {
		let current = self.is_current(id);
		if current {
			*self.task.borrow_mut() = None;
		}
		current
	}
}

/// What a `VoicePreview` needs delivered back from its download and from the player.
pub enum PreviewMessage {
	Ready { request: u64, result: ApiResult<Bytes> },
	Player { request: u64, event: PlayerEvent },
}

/// Download-and-play of a voice's sample behind one toggle button, shared by the Text to Speech tab and the
/// Voice Library. The owner wraps `PreviewMessage` into its own event type and feeds it back to `handle`.
pub struct VoicePreview<E> {
	button: Button,
	status: Status,
	services: Rc<Services>,
	sender: Sender<E>,
	wrap: fn(PreviewMessage) -> E,
	name: &'static str,
	active: Cell<bool>,
	request: Cell<u64>,
}

impl<E: Send + 'static> VoicePreview<E> {
	/// `name` is lower case ("voice preview"); it is capitalised where a sentence starts with it.
	pub fn new(
		button: Button,
		status: Status,
		services: Rc<Services>,
		sender: Sender<E>,
		wrap: fn(PreviewMessage) -> E,
		name: &'static str,
	) -> VoicePreview<E> {
		VoicePreview { button, status, services, sender, wrap, name, active: Cell::new(false), request: Cell::new(0) }
	}

	pub fn is_active(&self) -> bool {
		self.active.get()
	}

	pub fn set_enabled(&self, enabled: bool, fallback: &impl WxWidget) {
		set_enabled(self.button, enabled, fallback);
	}

	pub fn start(&self, api: ApiClient, url: String) {
		let request = self.request.get() + 1;
		self.request.set(request);
		self.active.set(true);
		self.button.set_label("Stop &Voice preview");
		self.status.set(&format!("Loading {}…", self.name));
		let wrap = self.wrap;
		runtime::spawn_and_send(self.sender.clone(), async move {
			wrap(PreviewMessage::Ready { request, result: api.download_public(&url).await })
		});
	}

	/// User-requested stop. Returns false when nothing was previewing.
	pub fn stop(&self) -> bool {
		if !self.active.get() {
			return false;
		}
		// Late events of the stopped request must not touch a preview started right afterwards.
		self.request.set(self.request.get() + 1);
		self.services.player.stop();
		self.end(&format!("{} stopped.", capitalised(self.name)), false);
		true
	}

	pub fn handle(&self, message: PreviewMessage) {
		match message {
			PreviewMessage::Ready { request, result } if self.is_live(request) => match result {
				Ok(bytes) => self.play(request, &bytes),
				Err(error) => self.end(&format!("The {} could not be loaded. {}", self.name, error.message), true),
			},
			PreviewMessage::Player { request, event } if self.is_live(request) => match event {
				PlayerEvent::Started => self.status.set(&format!("Playing {}.", self.name)),
				PlayerEvent::Finished => self.end(&format!("{} finished.", capitalised(self.name)), false),
				PlayerEvent::Stopped => {
					self.active.set(false);
					self.button.set_label("Preview &Voice");
				}
				PlayerEvent::Error(message) => self.end(&message, true),
				PlayerEvent::Paused | PlayerEvent::Resumed => {}
			},
			_ => {}
		}
	}

	fn is_live(&self, request: u64) -> bool {
		self.active.get() && request == self.request.get()
	}

	fn play(&self, request: u64, bytes: &Bytes) {
		let (sender, wrap) = (self.sender.clone(), self.wrap);
		self.services.player.play_clip(
			Playable::Encoded(bytes.to_vec()),
			Box::new(move |event| {
				let _ = sender.send(wrap(PreviewMessage::Player { request, event }));
			}),
		);
	}

	/// Only failures are announced; the button label already tells the user the preview ended.
	fn end(&self, message: &str, is_error: bool) {
		self.active.set(false);
		self.button.set_label("Preview &Voice");
		if is_error {
			self.status.say(message);
		} else {
			self.status.set(message);
		}
	}
}

fn capitalised(text: &str) -> String {
	let mut chars = text.chars();
	chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// "Monday, September 1, 2025" for a unix timestamp, in the user's time zone.
pub fn format_date(unix: i64) -> String {
	Local.timestamp_opt(unix, 0).single().map(|date| date.format("%A, %B %-d, %Y").to_string()).unwrap_or_default()
}

/// Accepts either a unix timestamp or an ISO-8601 string (the dubbing API's `created_at`).
pub fn format_api_date(raw: &str) -> String {
	if let Ok(unix) = raw.trim().parse::<i64>() {
		return format_date(unix);
	}
	if let Ok(parsed) = DateTime::parse_from_rfc3339(raw.trim().trim_matches('"')) {
		return format_date(parsed.timestamp());
	}
	raw.trim().trim_matches('"').chars().take(10).collect()
}

/// Disables a button without stranding keyboard focus on it: Windows drops focus
/// from a disabled control and the screen reader then reads an arbitrary window.
pub fn set_enabled(button: Button, enabled: bool, fallback: &impl WxWidget) {
	if !enabled && button.has_focus() {
		fallback.set_focus();
	}
	button.enable(enabled);
}

#[cfg(test)]
mod tests {
	use std::future::pending;

	use super::*;

	#[test]
	fn file_names_lose_windows_forbidden_characters() {
		assert_eq!(safe_file_name("a:b/c\\d*e?f\"g<h>i|j"), "a-b-c-d-e-f-g-h-i-j");
		assert_eq!(safe_file_name("tab	here"), "tab-here");
		assert_eq!(safe_file_name("  Voice name. . "), "Voice name");
		assert_eq!(safe_file_name("Ünïcode ok"), "Ünïcode ok");
	}

	#[test]
	fn a_superseded_job_cannot_finish() {
		let job = Job::default();
		let first = job.begin();
		job.attach(runtime::spawn(pending::<()>()));
		assert!(job.is_running());
		let second = job.begin();
		assert!(!job.is_running(), "beginning a run cancels the previous task");
		assert!(!job.finish(first));
		job.attach(runtime::spawn(pending::<()>()));
		assert!(job.finish(second) && !job.is_running());
		assert!(!job.cancel());
	}

	#[test]
	fn player_events_map_to_play_state() {
		assert!(PlayState::after(&PlayerEvent::Resumed) == PlayState::Playing);
		assert!(PlayState::after(&PlayerEvent::Error("x".into())) == PlayState::Idle);
		assert_eq!(PlayState::note(&PlayerEvent::Finished), Some("Playback finished."));
		assert_eq!(PlayState::note(&PlayerEvent::Stopped), None);
		assert_eq!(capitalised("voice preview"), "Voice preview");
	}
}
