use std::{
	error,
	fmt::{self, Display, Formatter, Write as _},
	io,
};

use elevenlabs_rs::error::Error as ElevenLabsError;
use reqwest::Error as ReqwestError;
use serde_json::{Value, from_str};

/// Error type for everything the app-owned client returns. The message is
/// written for a person to read (and for a screen reader to speak).
#[derive(Debug, Clone)]
pub struct ApiError {
	pub message: String,
	pub status: Option<u16>,
}

impl ApiError {
	pub fn new(message: impl Into<String>) -> ApiError {
		ApiError { message: message.into(), status: None }
	}

	pub fn unexpected_response() -> ApiError {
		ApiError::new("ElevenLabs returned a response the app did not understand.")
	}

	pub fn timed_out() -> ApiError {
		ApiError::new("ElevenLabs did not respond in time. Try again.")
	}

	pub fn from_status(status: u16, body: &str, request_id: Option<&str>) -> ApiError {
		let detail = from_str::<Value>(body).ok().and_then(|value| detail_message(&value));
		let hint = match status {
			401 => Some("ElevenLabs rejected the API key. Check it in Preferences."),
			402 | 403 => Some("This account does not have access to that feature."),
			429 => Some("ElevenLabs is rate limiting requests. Wait a moment and try again."),
			500..=599 => Some("ElevenLabs reported a server problem."),
			_ => None,
		};
		let mut message = match (detail, hint) {
			(Some(detail), Some(hint)) => format!("{hint} {detail}"),
			(Some(detail), None) => detail,
			(None, Some(hint)) => hint.to_string(),
			(None, None) => format!("The ElevenLabs request failed with HTTP status {status}."),
		};
		if let Some(id) = request_id {
			write!(message, " Request ID: {id}.").expect("writing to a String cannot fail");
		}
		ApiError { message, status: Some(status) }
	}
}

impl Display for ApiError {
	fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
		formatter.write_str(&self.message)
	}
}

impl error::Error for ApiError {}

/// ElevenLabs errors come as {"detail": {"message": ..}}, {"detail": "text"}
/// or a validation list {"detail": [{"msg": ..}]}.
pub fn detail_message(value: &Value) -> Option<String> {
	let detail = value.get("detail").unwrap_or(value);
	match detail {
		Value::String(text) => Some(text.clone()),
		Value::Object(map) => map.get("message").and_then(Value::as_str).map(str::to_string),
		Value::Array(items) => {
			let messages: Vec<&str> = items.iter().filter_map(|item| item.get("msg").and_then(Value::as_str)).collect();
			if messages.is_empty() { None } else { Some(messages.join("; ")) }
		}
		_ => None,
	}
}

impl From<ElevenLabsError> for ApiError {
	fn from(error: ElevenLabsError) -> ApiError {
		match error {
			ElevenLabsError::ApiError(api) => {
				ApiError::from_status(api.status.as_u16(), &api.body, api.request_id.as_deref())
			}
			ElevenLabsError::ReqwestError(error) => ApiError::from(error),
			other => ApiError::new(other.to_string()),
		}
	}
}

impl From<ReqwestError> for ApiError {
	fn from(error: ReqwestError) -> ApiError {
		if error.is_timeout() || error.is_connect() {
			return ApiError::new("Could not reach ElevenLabs. Check your internet connection and try again.");
		}
		match error.status() {
			Some(status) => ApiError::from_status(status.as_u16(), "", None),
			None => ApiError::new(format!("The network request failed: {}", error.without_url())),
		}
	}
}

impl From<io::Error> for ApiError {
	fn from(error: io::Error) -> ApiError {
		ApiError::new(format!("A file could not be read: {error}"))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn formats_detail_variants() {
		let object = ApiError::from_status(422, r#"{"detail":{"status":"x","message":"Bad voice"}}"#, Some("abc"));
		assert_eq!(object.message, "Bad voice Request ID: abc.");
		let list = ApiError::from_status(422, r#"{"detail":[{"msg":"too short"},{"msg":"missing"}]}"#, None);
		assert_eq!(list.message, "too short; missing");
		let unauthorized = ApiError::from_status(401, "not json", None);
		assert!(unauthorized.message.contains("Preferences"));
		assert_eq!(unauthorized.status, Some(401));
	}
}
