//! Preferences dialog: API key, defaults, folders and audio devices.

use std::{cell::Cell, fs::create_dir_all, iter::once, path::Path, rc::Rc, sync::mpsc::Sender};

use wxdragon::prelude::*;

use super::{
	announce::Announcer,
	inbox::Inbox,
	services::Services,
	widgets::{self, Form, Status},
};
use crate::{
	api::{ApiClient, ApiError},
	audio::devices,
	config, runtime,
};

const SYSTEM_DEFAULT_DEVICE: &str = "System default";

fn device_choices(devices: Vec<String>) -> Vec<String> {
	once(SYSTEM_DEFAULT_DEVICE.to_string()).chain(devices).collect()
}

fn device_index(choices: &[String], saved: &str) -> usize {
	choices.iter().position(|name| name == saved).unwrap_or(0)
}

fn device_setting(choices: &[String], index: Option<usize>) -> String {
	match index {
		Some(index) if index > 0 => choices.get(index).cloned().unwrap_or_default(),
		_ => String::new(),
	}
}

/// Shows the dialog; returns true when settings were saved. The caller announces the outcome once focus is back.
pub fn show(services: &Rc<Services>) -> bool {
	let dialog = Dialog::builder(&services.frame, "Preferences").build();
	let announcer = Announcer::new(&dialog);
	let form = Form::new(&dialog);

	let has_stored_key = !config::load().api_key.trim().is_empty();
	let (key, key_state, check, remove) = form.group("ElevenLabs account", false, |group| {
		let key = group.password("ElevenLabs &API key");
		key.set_accessibility_description("Leave empty to keep the stored key.");
		let key_state = group.read_only_line(
			"API key stored",
			if has_stored_key {
				"Yes, saved in the settings folder"
			} else if config::load_api_key().is_some() {
				"No, using the ELEVENLABS_API_KEY environment variable"
			} else {
				"No key stored yet"
			},
		);
		let row = widgets::button_row();
		let check = group.button(row, "&Check Key");
		let remove = group.button(row, "&Remove Stored Key");
		group.add_row(row);
		(key, key_state, check, remove)
	});

	let settings = config::load();
	let (models, offline) = {
		let catalog = services.catalog.borrow();
		(catalog.tts_models.clone(), catalog.models_offline)
	};
	let model_names: Vec<String> = models.iter().map(|model| model.name.clone()).collect();
	let (model, format, directory, browse) = form.group("Defaults", false, |group| {
		let model = group.choice("Default &model for Text to Speech", &model_names);
		if let Some(index) = models.iter().position(|model| model.id == settings.default_model) {
			widgets::select_choice(model, index);
		} else if !models.is_empty() {
			widgets::select_choice(model, 0);
		}
		if offline {
			model.set_accessibility_description("The live model list is not loaded; showing the built-in list.");
		}
		let format = group.choice("Default output &format", &widgets::output_format_labels());
		widgets::select_output_format(format, &settings.default_output_format);
		let directory = group.line("Default &output folder", &settings.output_directory);
		let row = widgets::button_row();
		let browse = group.button(row, "&Browse…");
		group.add_row(row);
		(model, format, directory, browse)
	});

	let outputs = device_choices(devices::output_device_names());
	let inputs = device_choices(devices::input_device_names());
	let (output_device, input_device) = form.group("Audio devices", false, |group| {
		let output_device = group.choice("&Playback device", &outputs);
		widgets::select_choice(output_device, device_index(&outputs, &settings.output_device));
		let input_device = group.choice("Recording de&vice", &inputs);
		widgets::select_choice(input_device, device_index(&inputs, &settings.input_device));
		(output_device, input_device)
	});

	let status = form.status(announcer);
	let buttons = widgets::button_row();
	let save = form.button(buttons, "Save");
	let cancel = form.button(buttons, "Cancel");
	form.add_row(buttons);
	let sizer = form.sizer;
	dialog.set_sizer_and_fit(sizer, true);
	dialog.set_affirmative_id(save.get_id());
	dialog.set_escape_id(cancel.get_id());
	save.set_default();

	let (check_sender, _check_inbox) = check_channel(status);
	{
		check.on_click(move |_| {
			let typed = key.get_value();
			let candidate =
				if typed.trim().is_empty() { config::load_api_key() } else { Some(typed.trim().to_string()) };
			let Some(candidate) = candidate else {
				key.set_focus();
				status.say("Enter an API key to check.");
				return;
			};
			status.set("Checking the API key…");
			let api = ApiClient::new(&candidate);
			runtime::spawn_and_send(
				check_sender.clone(),
				async move { api.list_models().await.map(|models| models.len()) },
			);
		});
	}
	{
		let services = services.clone();
		remove.on_click(move |_| {
			if !widgets::confirm(&dialog, "Remove API key", "Remove the stored ElevenLabs API key from this computer?")
			{
				return;
			}
			remove.set_focus();
			match config::delete_api_key() {
				Ok(()) => {
					services.reload_api_key();
					key_state.set_value("No key stored yet");
					status.say("The stored API key was removed.");
				}
				Err(message) => status.say(&message),
			}
		});
	}
	{
		browse.on_click(move |_| {
			if let Some(path) =
				widgets::choose_directory(&dialog, "Choose the default output folder", &directory.get_value())
			{
				directory.set_value(&path.to_string_lossy());
				status.set("Output folder chosen.");
			}
			browse.set_focus();
		});
	}
	cancel.on_click(move |_| dialog.end_modal(ID_CANCEL));
	let saved = Rc::new(Cell::new(false));
	{
		let services = services.clone();
		let saved = saved.clone();
		let models = models.clone();
		save.on_click(move |_| {
			let typed_key = key.get_value().trim().to_string();
			let directory_text = directory.get_value().trim().to_string();
			if !directory_text.is_empty()
				&& !Path::new(&directory_text).is_dir()
				&& let Err(error) = create_dir_all(&directory_text)
			{
				status.say(&format!("The output folder could not be created: {error}"));
				directory.set_focus();
				return;
			}
			let output_device = device_setting(&outputs, widgets::choice_selection(output_device));
			let result = config::update(|settings| {
				if !typed_key.is_empty() {
					settings.api_key.clone_from(&typed_key);
				}
				if let Some(index) = widgets::choice_selection(model)
					&& let Some(chosen) = models.get(index)
				{
					settings.default_model.clone_from(&chosen.id);
				}
				settings.default_output_format = widgets::output_format_id(format).to_string();
				settings.output_directory.clone_from(&directory_text);
				settings.output_device.clone_from(&output_device);
				settings.input_device = device_setting(&inputs, widgets::choice_selection(input_device));
			});
			if let Err(message) = result {
				status.say(&message);
				return;
			}
			services.player.set_device(&output_device);
			saved.set(true);
			dialog.end_modal(ID_OK);
		});
	}
	key.set_focus();
	dialog.show_modal();
	dialog.destroy();
	let saved = saved.get();
	if saved {
		services.reload_api_key();
		services.reload_catalog();
	}
	saved
}

type KeyCheck = Result<usize, ApiError>;

fn check_channel(status: Status) -> (Sender<KeyCheck>, Inbox<KeyCheck>) {
	let inbox = Inbox::new(move |result: KeyCheck| match result {
		Ok(count) => status.say(&format!("The API key works. ElevenLabs lists {count} models for this account.")),
		Err(error) => status.say(&error.message),
	});
	(inbox.sender(), inbox)
}
