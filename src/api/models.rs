//! Model catalogue from GET /v1/models. Model IDs are plain strings and every
//! capability comes from the API's own fields, so new models (Eleven v4 and
//! friends) show up without a code change.

use serde_json::Value;

// reason: the four flags mirror independent capability fields of the ElevenLabs model object.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
	pub id: String,
	pub name: String,
	pub description: String,
	pub can_do_text_to_speech: bool,
	pub can_do_voice_conversion: bool,
	pub can_use_style: bool,
	pub can_use_speaker_boost: bool,
	pub max_characters: Option<u64>,
	pub language_count: usize,
}

impl ModelInfo {
	// reason: the character limit is a positive whole number the API serialises as a JSON number.
	#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
	pub fn from_json(value: &Value) -> Option<ModelInfo> {
		let id = value.get("model_id")?.as_str()?.to_string();
		let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
		let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
		let max_characters = value
			.get("maximum_text_length_per_request")
			.and_then(Value::as_f64)
			.filter(|limit| *limit > 0.0)
			.map(|limit| limit as u64);
		Some(ModelInfo {
			name: Some(text("name")).filter(|name| !name.is_empty()).unwrap_or_else(|| id.clone()),
			description: text("description"),
			can_do_text_to_speech: flag("can_do_text_to_speech"),
			can_do_voice_conversion: flag("can_do_voice_conversion"),
			can_use_style: flag("can_use_style"),
			can_use_speaker_boost: flag("can_use_speaker_boost"),
			max_characters,
			language_count: value.get("languages").and_then(Value::as_array).map_or(0, Vec::len),
			id,
		})
	}

	/// One-line summary for the read-only "model details" field.
	pub fn details(&self) -> String {
		let mut parts = vec![self.id.clone()];
		if !self.description.is_empty() {
			parts.push(self.description.clone());
		}
		if let Some(limit) = self.max_characters {
			parts.push(format!("up to {limit} characters per request"));
		}
		if self.language_count > 0 {
			parts.push(format!("{} languages", self.language_count));
		}
		parts.join(". ")
	}
}

pub fn parse_models(body: &Value) -> Vec<ModelInfo> {
	body.as_array().map(|items| items.iter().filter_map(ModelInfo::from_json).collect()).unwrap_or_default()
}

pub fn text_to_speech_models(models: &[ModelInfo]) -> Vec<ModelInfo> {
	models.iter().filter(|model| model.can_do_text_to_speech).cloned().collect()
}

pub fn voice_conversion_models(models: &[ModelInfo]) -> Vec<ModelInfo> {
	models.iter().filter(|model| model.can_do_voice_conversion).cloned().collect()
}

/// Used only when GET /v1/models cannot be reached. These are the IDs the
/// original ElevenDesk offered; the panel says plainly that the list is offline.
pub fn offline_text_to_speech_models() -> Vec<ModelInfo> {
	[
		("eleven_multilingual_v2", "Eleven Multilingual v2"),
		("eleven_flash_v2_5", "Eleven Flash v2.5"),
		("eleven_turbo_v2_5", "Eleven Turbo v2.5"),
		("eleven_v3", "Eleven v3"),
	]
	.into_iter()
	.map(|(id, name)| offline_model(id, name, true, false))
	.collect()
}

pub fn offline_voice_conversion_models() -> Vec<ModelInfo> {
	[
		("eleven_multilingual_sts_v2", "Eleven Multilingual v2 (speech to speech)"),
		("eleven_english_sts_v2", "Eleven English v2 (speech to speech)"),
	]
	.into_iter()
	.map(|(id, name)| offline_model(id, name, false, true))
	.collect()
}

fn offline_model(id: &str, name: &str, tts: bool, sts: bool) -> ModelInfo {
	ModelInfo {
		id: id.to_string(),
		name: name.to_string(),
		description: String::new(),
		can_do_text_to_speech: tts,
		can_do_voice_conversion: sts,
		can_use_style: true,
		can_use_speaker_boost: true,
		max_characters: None,
		language_count: 0,
	}
}

/// Speech to text models are not part of /v1/models, so the list is fixed.
pub fn speech_to_text_models() -> Vec<ModelInfo> {
	[("scribe_v2", "Scribe v2"), ("scribe_v1", "Scribe v1")]
		.into_iter()
		.map(|(id, name)| offline_model(id, name, false, false))
		.collect()
}

pub struct Selection {
	pub index: usize,
	/// Set when the saved model could not be honoured.
	pub notice: Option<String>,
}

/// Picks the saved model if the account still offers it, otherwise the first
/// available one with an explanatory notice.
pub fn resolve_saved(saved: &str, models: &[ModelInfo]) -> Selection {
	if saved.is_empty() || models.is_empty() {
		return Selection { index: 0, notice: None };
	}
	match models.iter().position(|model| model.id == saved) {
		Some(index) => Selection { index, notice: None },
		None => Selection {
			index: 0,
			notice: Some(format!(
				"Your saved model {saved} is no longer available for this account. Using {} instead.",
				models[0].name
			)),
		},
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[test]
	fn parses_sparse_and_unknown_models() {
		let body = json!([
			{"model_id": "eleven_v4", "name": "Eleven v4", "can_do_text_to_speech": true, "can_use_style": true, "future_field": 1},
			{"model_id": "eleven_multilingual_sts_v2", "can_do_voice_conversion": true},
			{"name": "no id, skipped"}
		]);
		let models = parse_models(&body);
		assert_eq!(models.len(), 2);
		assert_eq!(models[0].id, "eleven_v4");
		assert!(models[0].can_use_style && !models[0].can_use_speaker_boost);
		assert_eq!(models[1].name, "eleven_multilingual_sts_v2");
		assert_eq!(text_to_speech_models(&models).len(), 1);
		assert_eq!(voice_conversion_models(&models).len(), 1);
	}

	#[test]
	fn missing_saved_model_reports_a_notice() {
		let models = offline_text_to_speech_models();
		let kept = resolve_saved("eleven_v3", &models);
		assert!(kept.notice.is_none());
		assert_eq!(models[kept.index].id, "eleven_v3");
		let gone = resolve_saved("eleven_retired", &models);
		assert_eq!(gone.index, 0);
		assert!(gone.notice.unwrap().contains("eleven_retired"));
		assert!(resolve_saved("", &models).notice.is_none());
		assert!(resolve_saved("eleven_retired", &[]).notice.is_none(), "an empty list has nothing to fall back to");
	}
}
