//! Non-secret user settings, stored as JSON in the per-user config directory.

use std::{
	env, fs,
	io::ErrorKind,
	path::{Path, PathBuf},
};

use dirs::document_dir;
use homeport::Homeport;
use serde::{Deserialize, Serialize};
use serde_json::{from_str, to_string_pretty};

const APP_NAME: &str = "ElevenDesk2";
const CONFIG_OVERRIDE_VAR: &str = "ELEVENDESK2_CONFIG_DIR";
const FILE_NAME: &str = "settings.json";
pub const DEFAULT_OUTPUT_FORMAT: &str = "mp3_44100_128";

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
	pub default_model: String,
	pub default_stt_model: String,
	pub default_sts_model: String,
	pub default_voice_id: String,
	pub default_output_format: String,
	pub output_directory: String,
	pub output_device: String,
	pub input_device: String,
	pub sounds_enabled: bool,
	/// ElevenLabs API key, stored in plain text in this file; never logged.
	pub api_key: String,
}

impl Default for Settings {
	fn default() -> Self {
		Settings {
			default_model: String::new(),
			default_stt_model: String::new(),
			default_sts_model: String::new(),
			default_voice_id: String::new(),
			default_output_format: DEFAULT_OUTPUT_FORMAT.to_string(),
			output_directory: document_dir().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default(),
			output_device: String::new(),
			input_device: String::new(),
			sounds_enabled: false,
			api_key: String::new(),
		}
	}
}

/// Where settings and the API key live: next to the exe when portable, %APPDATA%\ElevenDesk2 when installed.
pub fn config_dir() -> Result<PathBuf, String> {
	Homeport::new(APP_NAME)
		.with_config_override(CONFIG_OVERRIDE_VAR)
		.create_config_dir()
		.map_err(|error| format!("The settings folder could not be created: {error}"))
}

fn settings_path() -> Option<PathBuf> {
	config_dir().ok().map(|dir| dir.join(FILE_NAME))
}

/// `Ok(None)` when there is no settings file yet; `Err` when it exists but cannot be parsed.
fn read_from(path: &Path) -> Result<Option<Settings>, String> {
	match fs::read_to_string(path) {
		// Only the position is reported: serde's message can quote the offending value, which may be the API key.
		Ok(text) => from_str(&text)
			.map(Some)
			.map_err(|error| format!("invalid JSON at line {}, column {}", error.line(), error.column())),
		Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
		Err(error) => Err(error.to_string()),
	}
}

/// Written beside the target and renamed over it, so a crash mid-write cannot truncate the settings (and the API key).
fn write_to(path: &Path, settings: &Settings) -> Result<(), String> {
	let text = to_string_pretty(settings).map_err(|error| error.to_string())?;
	let temporary = path.with_extension("json.tmp");
	fs::write(&temporary, text)
		.and_then(|()| fs::rename(&temporary, path))
		.map_err(|error| format!("The settings file could not be saved: {error}"))
}

/// An unreadable file is moved aside before the first write replaces it, so one bad edit cannot silently destroy the API key.
fn update_at(path: &Path, change: impl FnOnce(&mut Settings)) -> Result<(), String> {
	let mut settings = match read_from(path) {
		Ok(settings) => settings.unwrap_or_default(),
		Err(error) => {
			let backup = path.with_extension("json.bad");
			fs::rename(path, &backup).map_err(|rename_error| {
				format!("The settings file is unreadable ({error}) and could not be set aside: {rename_error}")
			})?;
			Settings::default()
		}
	};
	change(&mut settings);
	write_to(path, &settings)
}

pub fn load() -> Settings {
	let Some(path) = settings_path() else {
		return Settings::default();
	};
	read_from(&path)
		.unwrap_or_else(|error| {
			eprintln!("settings file {} is unreadable ({error}); using defaults", path.display());
			None
		})
		.unwrap_or_default()
}

/// Read-modify-write so independent panels never clobber each other's fields.
pub fn update(change: impl FnOnce(&mut Settings)) -> Result<(), String> {
	update_at(&config_dir()?.join(FILE_NAME), change)
}

// Development convenience; a saved key always wins.
const API_KEY_ENV: &str = "ELEVENLABS_API_KEY";

pub fn load_api_key() -> Option<String> {
	let saved = load().api_key;
	let key = if saved.trim().is_empty() { env::var(API_KEY_ENV).ok()? } else { saved };
	let key = key.trim().to_string();
	if key.is_empty() { None } else { Some(key) }
}

pub fn delete_api_key() -> Result<(), String> {
	update(|settings| settings.api_key.clear())
}

#[cfg(test)]
mod tests {
	use std::{env::temp_dir, process::id};

	use super::*;

	fn scratch(name: &str) -> PathBuf {
		let dir = temp_dir().join(format!("elevendesk2-config-{name}-{}", id()));
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir).unwrap();
		dir.join(FILE_NAME)
	}

	#[test]
	fn update_keeps_other_fields_and_leaves_no_temp_file() {
		let path = scratch("update");
		update_at(&path, |settings| settings.api_key = "secret".into()).unwrap();
		update_at(&path, |settings| settings.default_model = "m".into()).unwrap();
		let loaded = read_from(&path).unwrap().unwrap();
		assert_eq!((loaded.api_key.as_str(), loaded.default_model.as_str()), ("secret", "m"));
		assert!(!path.with_extension("json.tmp").exists());
	}

	#[test]
	fn unreadable_file_is_set_aside_not_overwritten() {
		let path = scratch("corrupt");
		fs::write(&path, "{ not json").unwrap();
		assert!(read_from(&path).is_err());
		update_at(&path, |settings| settings.default_voice_id = "v".into()).unwrap();
		assert_eq!(fs::read_to_string(path.with_extension("json.bad")).unwrap(), "{ not json");
		assert_eq!(read_from(&path).unwrap().unwrap().default_voice_id, "v");
	}
}
