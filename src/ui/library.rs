//! Voice Library dialog: search, preview, use, tune, rename and delete voices.

use std::{cell::RefCell, fmt::Write as _, rc::Rc, sync::mpsc::Sender};

use wxdragon::prelude::*;

use super::{
	announce::Announcer,
	services::Services,
	widgets::{self, Form, PreviewMessage, Status, VoicePreview},
};
use crate::{
	api::{ApiResult, Voice, VoiceParams},
	runtime,
};

const DESCRIPTION_PREVIEW_CHARS: usize = 90;

enum LibraryEvent {
	Settings { voice: Voice, result: ApiResult<VoiceParams> },
	SettingsSaved(ApiResult<()>),
	Edited { id: String, name: String, description: String, result: ApiResult<()> },
	Deleted { id: String, result: ApiResult<()> },
	Preview(PreviewMessage),
}

struct State {
	services: Rc<Services>,
	dialog: Dialog,
	status: Status,
	search: TextCtrl,
	list: ListBox,
	use_voice: Button,
	preview: VoicePreview<LibraryEvent>,
	settings: Button,
	edit: Button,
	delete: Button,
	voices: RefCell<Vec<Voice>>,
	/// Indices into `voices` of the rows currently listed.
	shown: RefCell<Vec<usize>>,
	sender: Sender<LibraryEvent>,
	chosen: RefCell<Option<String>>,
}

fn row_text(voice: &Voice) -> String {
	let mut text = voice.name.clone();
	if !voice.category.is_empty() {
		write!(text, ", {}", voice.category).expect("writing to a String cannot fail");
	}
	if !voice.description.is_empty() {
		let shortened: String = voice.description.chars().take(DESCRIPTION_PREVIEW_CHARS).collect();
		write!(text, ". {shortened}").expect("writing to a String cannot fail");
	}
	text
}

/// Returns the voice the user chose with "Use Voice". The caller applies it after this window
/// has closed, so the resulting announcement is not swallowed by the dialog closing.
pub fn show(services: &Rc<Services>) -> Option<String> {
	let dialog = Dialog::builder(&services.frame, "Voice Library").build();
	let announcer = Announcer::new(&dialog);
	let form = Form::new(&dialog);
	let (search, list) = form.group("Find a voice", true, |group| {
		let search = group.line("&Search voices by name, category or description", "");
		let list = group.list("Voice &list");
		(search, list)
	});
	let (use_voice, preview, settings, edit, delete) = form.group("Selected voice", false, |group| {
		let row = widgets::button_row();
		let use_voice = group.button(row, "&Use Voice");
		let preview = group.button(row, "Preview &Voice");
		let settings = group.button(row, "Voice Se&ttings…");
		let edit = group.button(row, "&Edit…");
		let delete = group.button(row, "&Delete");
		group.add_row(row);
		(use_voice, preview, settings, edit, delete)
	});
	let status = form.status(announcer);
	let row = widgets::button_row();
	let close = form.button(row, "Close");
	form.add_row(row);
	dialog.set_sizer_and_fit(form.sizer, true);
	dialog.set_size(Size::new(720, 560));
	dialog.set_escape_id(close.get_id());

	let (state, _inbox) = widgets::with_inbox(
		|sender| {
			let voice_preview =
				VoicePreview::new(preview, status, services.clone(), sender.clone(), LibraryEvent::Preview, "preview");
			Rc::new(State {
				services: services.clone(),
				dialog,
				status,
				search,
				list,
				use_voice,
				preview: voice_preview,
				settings,
				edit,
				delete,
				voices: RefCell::new(services.catalog.borrow().voices.clone()),
				shown: RefCell::new(Vec::new()),
				sender,
				chosen: RefCell::new(None),
			})
		},
		State::handle,
	);
	state.search.on_text_changed(widgets::handler(&state, State::refilter));
	state.list.on_selection_changed(widgets::handler(&state, State::update_buttons));
	state.use_voice.on_click(widgets::handler(&state, State::use_selected));
	preview.on_click(widgets::handler(&state, State::toggle_preview));
	state.settings.on_click(widgets::handler(&state, State::open_settings));
	state.edit.on_click(widgets::handler(&state, State::edit_selected));
	state.delete.on_click(widgets::handler(&state, State::delete_selected));
	close.on_click(move |_| dialog.end_modal(ID_CLOSE));
	state.refilter();
	state.search.set_focus();
	if state.voices.borrow().is_empty() {
		state
			.status
			.set("No voices are loaded. Check your API key in Preferences, then use Refresh Voices and Models.");
	} else {
		state.status.set(&format!("{} voices.", state.voices.borrow().len()));
	}
	dialog.show_modal();
	state.services.player.stop();
	dialog.destroy();

	state.chosen.borrow_mut().take()
}

impl State {
	fn selected(&self) -> Option<Voice> {
		let index = widgets::list_item(self.list, &self.shown)?;
		self.voices.borrow().get(index).cloned()
	}

	fn refilter(&self) {
		let previous = self.selected().map(|voice| voice.id);
		let needle = self.search.get_value().trim().to_lowercase();
		let voices = self.voices.borrow();
		let shown: Vec<usize> = voices
			.iter()
			.enumerate()
			.filter(|(_, voice)| {
				needle.is_empty()
					|| [&voice.name, &voice.category, &voice.description]
						.iter()
						.any(|field| field.to_lowercase().contains(&needle))
			})
			.map(|(index, _)| index)
			.collect();
		widgets::fill_list(self.list, &shown.iter().map(|index| row_text(&voices[*index])).collect::<Vec<_>>());
		let keep = previous.and_then(|id| shown.iter().position(|index| voices[*index].id == id)).unwrap_or(0);
		widgets::select_row_within(self.list, keep, shown.len());
		let summary = format!("{} of {} voices.", shown.len(), voices.len());
		let no_match = shown.is_empty() && !needle.is_empty();
		*self.shown.borrow_mut() = shown;
		drop(voices);
		if no_match {
			self.status.say("No voices match.");
		} else {
			self.status.set(&summary);
		}
		self.update_buttons();
	}

	fn update_buttons(&self) {
		let voice = self.selected();
		let owned = voice.as_ref().is_some_and(|voice| voice.is_owner);
		let previewable = self.preview.is_active() || voice.as_ref().is_some_and(|voice| voice.preview_url.is_some());
		widgets::set_enabled(self.use_voice, voice.is_some(), &self.list);
		widgets::set_enabled(self.settings, voice.is_some(), &self.list);
		self.preview.set_enabled(previewable, &self.list);
		widgets::set_enabled(self.edit, owned, &self.list);
		widgets::set_enabled(self.delete, owned, &self.list);
	}

	fn use_selected(&self) {
		if let Some(voice) = self.selected() {
			*self.chosen.borrow_mut() = Some(voice.id.clone());
			self.dialog.end_modal(ID_OK);
		}
	}

	fn toggle_preview(&self) {
		if !self.preview.stop() {
			let Some(url) = self.selected().and_then(|voice| voice.preview_url) else {
				self.status.say("This voice has no preview sample.");
				return;
			};
			let Some(api) = self.services.api_or_say(self.status) else { return };
			self.preview.start(api, url);
		}
		self.update_buttons();
	}

	fn open_settings(&self) {
		let Some(voice) = self.selected() else { return };
		let Some(api) = self.services.api_or_say(self.status) else { return };
		self.status.set("Loading voice settings…");
		let id = voice.id.clone();
		runtime::spawn_and_send(self.sender.clone(), async move {
			let result = api.voice_settings(&id).await;
			LibraryEvent::Settings { voice, result }
		});
	}

	fn edit_selected(&self) {
		let Some(voice) = self.selected() else { return };
		let Some((name, description)) = edit_dialog(self.dialog, &voice) else {
			self.edit.set_focus();
			return;
		};
		let Some(api) = self.services.api_or_say(self.status) else { return };
		self.status.set("Saving the voice…");
		let id = voice.id.clone();
		runtime::spawn_and_send(self.sender.clone(), async move {
			let result = api.edit_voice(&id, &name, &description).await;
			LibraryEvent::Edited { id, name, description, result }
		});
	}

	fn delete_selected(&self) {
		let Some(voice) = self.selected() else { return };
		if !widgets::confirm(
			&self.dialog,
			"Delete voice",
			&format!("Delete the voice \"{}\"? This cannot be undone.", voice.name),
		) {
			self.list.set_focus();
			return;
		}
		let Some(api) = self.services.api_or_say(self.status) else { return };
		self.status.set("Deleting the voice…");
		let id = voice.id.clone();
		runtime::spawn_and_send(self.sender.clone(), async move {
			let result = api.delete_voice(&id).await;
			LibraryEvent::Deleted { id, result }
		});
	}

	fn handle(&self, event: LibraryEvent) {
		match event {
			LibraryEvent::Settings { voice, result: Ok(params) } => {
				self.status.set("Ready");
				if let Some(updated) = settings_dialog(self.dialog, &voice.name, &params) {
					let Some(api) = self.services.api_or_say(self.status) else { return };
					self.status.set("Saving voice settings…");
					let id = voice.id.clone();
					runtime::spawn_and_send(self.sender.clone(), async move {
						LibraryEvent::SettingsSaved(api.update_voice_settings(&id, &updated).await)
					});
				}
				self.settings.set_focus();
			}
			LibraryEvent::Settings { result: Err(error), .. }
			| LibraryEvent::SettingsSaved(Err(error))
			| LibraryEvent::Edited { result: Err(error), .. }
			| LibraryEvent::Deleted { result: Err(error), .. } => self.status.say(&error.message),
			LibraryEvent::SettingsSaved(Ok(())) => self.status.say("Voice settings saved."),
			LibraryEvent::Edited { id, name, description, result: Ok(()) } => {
				for voice in self.voices.borrow_mut().iter_mut().filter(|voice| voice.id == id) {
					voice.name.clone_from(&name);
					voice.description.clone_from(&description);
				}
				self.services.reload_voices(None);
				self.refilter();
				self.edit.set_focus();
				self.status.say(&format!("Voice renamed to {name}."));
			}
			LibraryEvent::Deleted { id, result: Ok(()) } => {
				self.voices.borrow_mut().retain(|voice| voice.id != id);
				self.services.reload_voices(None);
				self.refilter();
				self.list.set_focus();
				self.status.say("Voice deleted.");
			}
			LibraryEvent::Preview(message) => {
				self.preview.handle(message);
				self.update_buttons();
			}
		}
	}
}

fn edit_dialog(parent: Dialog, voice: &Voice) -> Option<(String, String)> {
	let dialog = Dialog::builder(&parent, "Edit voice").build();
	let announcer = Announcer::new(&dialog);
	let form = Form::new(&dialog);
	let name = form.line("Voice &name", &voice.name);
	let description = form.area("&Description", false);
	description.set_value(&voice.description);
	let status = form.status(announcer);
	let row = widgets::button_row();
	let save = form.button(row, "Save");
	let cancel = form.button(row, "Cancel");
	form.add_row(row);
	dialog.set_sizer_and_fit(form.sizer, true);
	dialog.set_affirmative_id(save.get_id());
	dialog.set_escape_id(cancel.get_id());
	save.set_default();
	let result: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));
	{
		let result = result.clone();
		save.on_click(move |_| {
			let new_name = name.get_value().trim().to_string();
			if new_name.is_empty() {
				name.set_focus();
				status.say("The voice needs a name.");
				return;
			}
			*result.borrow_mut() = Some((new_name, description.get_value().trim().to_string()));
			dialog.end_modal(ID_OK);
		});
	}
	cancel.on_click(move |_| dialog.end_modal(ID_CANCEL));
	name.set_focus();
	dialog.show_modal();
	dialog.destroy();

	result.borrow_mut().take()
}

/// Only the settings the API returned for this voice are shown.
fn settings_dialog(parent: Dialog, voice_name: &str, params: &VoiceParams) -> Option<VoiceParams> {
	let dialog = Dialog::builder(&parent, &format!("Voice settings: {voice_name}")).build();
	let form = Form::new(&dialog);
	let slider = |label: &str, value: Option<f32>, min: i32, max: i32| {
		value.map(|value| form.slider(label, min, max, widgets::percent_from_fraction(value)))
	};
	let stability = slider("&Stability, percent", params.stability, 0, 100);
	let similarity = slider("Similarity &boost, percent", params.similarity_boost, 0, 100);
	let style = slider("Style e&xaggeration, percent", params.style, 0, 100);
	let speed = slider("S&peed, percent of normal", params.speed, 70, 120);
	let boost = params.use_speaker_boost.map(|value| form.check("Speaker bo&ost", value));
	let row = widgets::button_row();
	let save = form.button(row, "Save");
	let cancel = form.button(row, "Cancel");
	form.add_row(row);
	dialog.set_sizer_and_fit(form.sizer, true);
	dialog.set_affirmative_id(save.get_id());
	dialog.set_escape_id(cancel.get_id());
	save.set_default();
	let result: Rc<RefCell<Option<VoiceParams>>> = Rc::new(RefCell::new(None));
	{
		let result = result.clone();
		save.on_click(move |_| {
			*result.borrow_mut() = Some(VoiceParams {
				stability: stability.map(|control| widgets::fraction_from_percent(control.value())),
				similarity_boost: similarity.map(|control| widgets::fraction_from_percent(control.value())),
				style: style.map(|control| widgets::fraction_from_percent(control.value())),
				speed: speed.map(|control| widgets::fraction_from_percent(control.value())),
				use_speaker_boost: boost.map(|control| control.is_checked()),
			});
			dialog.end_modal(ID_OK);
		});
	}
	cancel.on_click(move |_| dialog.end_modal(ID_CANCEL));
	if let Some(first) = stability.or(similarity).or(style).or(speed) {
		first.set_focus();
	}
	dialog.show_modal();
	dialog.destroy();

	result.borrow_mut().take()
}
