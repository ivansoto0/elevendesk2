//! App-owned ElevenLabs client.
//!
//! Division of labour (see README, "Evaluating elevenlabs_rs"):
//! - the crate's typed endpoint is used for streaming text to speech;
//! - the crate's `raw()` request API covers every other JSON/multipart call,
//!   parsed leniently as `serde_json::Value` so new response fields or models
//!   cannot break deserialisation (the crate's typed `Model` struct would);
//! - a thin reqwest call covers streaming speech to speech, which `raw()` cannot stream.
//!
//! Nothing here logs the API key, and it is only ever sent to api.elevenlabs.io.

mod error;
pub mod models;

use std::{
	fmt::Write,
	future::Future,
	path::{Path, PathBuf},
	pin::Pin,
	time::Duration,
};

use bytes::Bytes;
use elevenlabs_rs::{
	ElevenLabsClient, Method, OutputFormat, RawRequestBuilder, VoiceSettings,
	endpoints::genai::tts::{TextToSpeechBody, TextToSpeechQuery, TextToSpeechStream},
	multipart,
};
pub use error::ApiError;
use futures_util::{Stream, StreamExt};
pub use models::ModelInfo;
use reqwest::Client as HttpClient;
use serde_json::{Map, Value, json};
use tokio::{fs::read as read_file, time::timeout};

pub type ApiResult<T> = Result<T, ApiError>;
pub type AudioStream = Pin<Box<dyn Stream<Item = ApiResult<Bytes>> + Send>>;

/// Percent-encodes an ID for use as one URL path segment, so an odd ID cannot add path parts or a query.
fn segment(id: &str) -> String {
	let mut encoded = String::with_capacity(id.len());
	for byte in id.bytes() {
		if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
			encoded.push(char::from(byte));
		} else {
			write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
		}
	}
	encoded
}

const BASE_URL: &str = "https://api.elevenlabs.io";
pub const SOUND_EFFECTS_MODEL: &str = "eleven_text_to_sound_v2";
pub const DUBBING_MODEL: &str = "dubbing_v2";
const PAGE_SIZE: u32 = 100;
// Upper bound on paging loops so a misbehaving cursor can never spin forever.
const MAX_PAGES: usize = 50;
// elevenlabs_rs builds its own reqwest client without timeouts, so every call into it is bounded here.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
// Uploads and server-side generation (speech to text, sound effects) legitimately take minutes.
const LONG_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
// Longest silence tolerated on the speech to speech response before giving up.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct Voice {
	pub id: String,
	pub name: String,
	pub category: String,
	pub description: String,
	pub preview_url: Option<String>,
	pub is_owner: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceParams {
	pub stability: Option<f32>,
	pub similarity_boost: Option<f32>,
	pub style: Option<f32>,
	pub speed: Option<f32>,
	pub use_speaker_boost: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct HistoryItem {
	pub id: String,
	pub date_unix: i64,
	pub voice_name: String,
	pub model_id: String,
	pub text: String,
}

#[derive(Debug, Clone)]
pub struct PronunciationDictionary {
	pub name: String,
	pub description: String,
}

#[derive(Debug, Clone)]
pub struct DubProject {
	pub id: String,
	pub name: String,
	pub status: String,
	pub source_language: String,
	pub created_at: String,
	pub error: String,
}

#[derive(Debug, Clone)]
pub struct DubLanguage {
	pub id: String,
	pub target_language: String,
	pub status: String,
	pub output_url: String,
	pub error: String,
}

#[derive(Clone)]
pub struct ApiClient {
	eleven: ElevenLabsClient,
	http: HttpClient,
	key: String,
	#[cfg(test)]
	base_url: Option<String>,
}

fn text(value: &Value, key: &str) -> String {
	value.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn optional_text(value: &Value, key: &str) -> Option<String> {
	value.get(key).and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string)
}

fn file_part(bytes: Vec<u8>, file_name: &str) -> multipart::Part {
	multipart::Part::bytes(bytes).file_name(file_name.to_string())
}

async fn within<T, E: Into<ApiError>>(limit: Duration, call: impl Future<Output = Result<T, E>>) -> ApiResult<T> {
	timeout(limit, call).await.map_err(|_| ApiError::timed_out())?.map_err(Into::into)
}

pub fn file_name_of(path: &Path) -> String {
	path.file_name().map_or_else(|| "audio".to_string(), |name| name.to_string_lossy().into_owned())
}

impl VoiceParams {
	fn to_json(&self) -> Value {
		let mut map = Map::new();
		for (key, number) in [
			("stability", self.stability),
			("similarity_boost", self.similarity_boost),
			("style", self.style),
			("speed", self.speed),
		] {
			if let Some(number) = number {
				map.insert(key.to_string(), json!(number));
			}
		}
		if let Some(boost) = self.use_speaker_boost {
			map.insert("use_speaker_boost".to_string(), json!(boost));
		}
		Value::Object(map)
	}

	// reason: the API sends f64; the voice sliders are f32 in the 0..=2 range.
	#[allow(clippy::cast_possible_truncation)]
	fn from_json(value: &Value) -> VoiceParams {
		let number = |key: &str| value.get(key).and_then(Value::as_f64).map(|number| number as f32);
		VoiceParams {
			stability: number("stability"),
			similarity_boost: number("similarity_boost"),
			style: number("style"),
			speed: number("speed"),
			use_speaker_boost: value.get("use_speaker_boost").and_then(Value::as_bool),
		}
	}
}

impl ApiClient {
	pub fn new(api_key: &str) -> ApiClient {
		ApiClient {
			eleven: ElevenLabsClient::new(api_key),
			http: HttpClient::builder()
				.connect_timeout(CONNECT_TIMEOUT)
				.read_timeout(READ_TIMEOUT)
				.build()
				.expect("failed to initialise the HTTP client"),
			key: api_key.to_string(),
			#[cfg(test)]
			base_url: None,
		}
	}

	/// Every crate `raw()` request goes through here so tests can point the client at a local mock.
	fn raw(&self, method: Method, path: impl Into<String>) -> RawRequestBuilder<'_> {
		let request = self.eleven.raw(method, path);
		#[cfg(test)]
		if let Some(base) = &self.base_url {
			return request.with_base_url(base.clone());
		}
		request
	}

	async fn json(
		&self,
		method: Method,
		path: &str,
		query: &[(&str, String)],
		body: Option<Value>,
	) -> ApiResult<Value> {
		let mut request = self.raw(method, path);
		for (name, value) in query {
			request = request.query(*name, value);
		}
		if let Some(body) = body {
			request = request.json(&body)?;
		}
		within(REQUEST_TIMEOUT, request.send_json::<Value>()).await
	}

	async fn multipart_json(&self, method: Method, path: &str, form: multipart::Form) -> ApiResult<Value> {
		within(LONG_REQUEST_TIMEOUT, self.raw(method, path).multipart(form).send_json::<Value>()).await
	}

	async fn ok(&self, method: Method, path: &str) -> ApiResult<()> {
		within(REQUEST_TIMEOUT, self.raw(method, path).send_bytes()).await?;
		Ok(())
	}

	pub async fn list_models(&self) -> ApiResult<Vec<ModelInfo>> {
		let body = self.json(Method::GET, "v1/models", &[], None).await?;
		Ok(models::parse_models(&body))
	}

	pub async fn list_voices(&self) -> ApiResult<Vec<Voice>> {
		let mut voices = Vec::new();
		let mut token: Option<String> = None;
		for _ in 0..MAX_PAGES {
			let mut query = vec![("page_size", PAGE_SIZE.to_string())];
			if let Some(token) = &token {
				query.push(("next_page_token", token.clone()));
			}
			let page = self.json(Method::GET, "v2/voices", &query, None).await?;
			for item in page.get("voices").and_then(Value::as_array).into_iter().flatten() {
				let Some(id) = optional_text(item, "voice_id") else { continue };
				voices.push(Voice {
					id,
					name: text(item, "name"),
					category: text(item, "category"),
					description: text(item, "description"),
					preview_url: optional_text(item, "preview_url"),
					is_owner: item.get("is_owner").and_then(Value::as_bool).unwrap_or(false),
				});
			}
			let has_more = page.get("has_more").and_then(Value::as_bool).unwrap_or(false);
			token = optional_text(&page, "next_page_token");
			if !has_more || token.is_none() {
				break;
			}
		}
		voices.sort_by_key(|voice| voice.name.to_lowercase());
		Ok(voices)
	}

	pub async fn voice_settings(&self, voice_id: &str) -> ApiResult<VoiceParams> {
		let body = self.json(Method::GET, &format!("v1/voices/{}/settings", segment(voice_id)), &[], None).await?;
		Ok(VoiceParams::from_json(&body))
	}

	pub async fn update_voice_settings(&self, voice_id: &str, params: &VoiceParams) -> ApiResult<()> {
		self.json(Method::POST, &format!("v1/voices/{}/settings/edit", segment(voice_id)), &[], Some(params.to_json()))
			.await?;
		Ok(())
	}

	pub async fn edit_voice(&self, voice_id: &str, name: &str, description: &str) -> ApiResult<()> {
		let form = multipart::Form::new().text("name", name.to_string()).text("description", description.to_string());
		self.multipart_json(Method::POST, &format!("v1/voices/{}/edit", segment(voice_id)), form).await?;
		Ok(())
	}

	pub async fn delete_voice(&self, voice_id: &str) -> ApiResult<()> {
		self.ok(Method::DELETE, &format!("v1/voices/{}", segment(voice_id))).await
	}

	/// Instant voice clone from local audio files. Dropping the future cancels the upload.
	pub async fn clone_voice(&self, name: &str, description: &str, paths: &[PathBuf]) -> ApiResult<String> {
		let mut form = multipart::Form::new().text("name", name.to_string()).text("labels", "{}");
		if !description.is_empty() {
			form = form.text("description", description.to_string());
		}
		for path in paths {
			let bytes = read_file(path).await?;
			form = form.part("files", file_part(bytes, &file_name_of(path)));
		}
		let body = self.multipart_json(Method::POST, "v1/voices/add", form).await?;
		optional_text(&body, "voice_id").ok_or_else(ApiError::unexpected_response)
	}

	/// Designs a voice from a description and saves the first preview to the account.
	pub async fn design_voice(&self, description: &str, name: &str) -> ApiResult<(String, String)> {
		let previews = self
			.json(
				Method::POST,
				"v1/text-to-voice/create-previews",
				&[],
				Some(json!({ "voice_description": description, "auto_generate_text": true })),
			)
			.await?;
		let generated_id = previews
			.get("previews")
			.and_then(Value::as_array)
			.and_then(|items| items.first())
			.and_then(|preview| optional_text(preview, "generated_voice_id"))
			.ok_or_else(ApiError::unexpected_response)?;
		let created = self
			.json(
				Method::POST,
				"v1/text-to-voice",
				&[],
				Some(
					json!({ "voice_name": name, "voice_description": description, "generated_voice_id": generated_id }),
				),
			)
			.await?;
		let voice_id = optional_text(&created, "voice_id").ok_or_else(ApiError::unexpected_response)?;
		Ok((voice_id, optional_text(&created, "name").unwrap_or_else(|| name.to_string())))
	}

	/// Streams 24 kHz mono 16-bit PCM so playback can start before generation ends.
	pub async fn text_to_speech_stream(
		&self,
		text: &str,
		voice_id: &str,
		model_id: &str,
		params: &VoiceParams,
	) -> ApiResult<AudioStream> {
		let settings = VoiceSettings {
			stability: params.stability,
			similarity_boost: params.similarity_boost,
			style: params.style,
			speed: params.speed,
			use_speaker_boost: params.use_speaker_boost,
		};
		let body = TextToSpeechBody::new(text).with_model_id(model_id).with_voice_settings(settings);
		let endpoint = TextToSpeechStream::new(voice_id, body)
			.with_query(TextToSpeechQuery::default().with_output_format(OutputFormat::Pcm24000Hz));
		let stream = within(LONG_REQUEST_TIMEOUT, self.eleven.hit(endpoint)).await?;
		Ok(Box::pin(stream.map(|chunk| chunk.map_err(ApiError::from))))
	}

	pub async fn speech_to_speech_stream(
		&self,
		audio: Vec<u8>,
		file_name: &str,
		voice_id: &str,
		model_id: &str,
	) -> ApiResult<AudioStream> {
		let form =
			multipart::Form::new().text("model_id", model_id.to_string()).part("audio", file_part(audio, file_name));
		let response = self
			.http
			.post(format!("{BASE_URL}/v1/speech-to-speech/{}/stream?output_format=pcm_24000", segment(voice_id)))
			.header("xi-api-key", &self.key)
			.multipart(form)
			.send()
			.await?;
		let status = response.status();
		if !status.is_success() {
			let request_id =
				response.headers().get("request-id").and_then(|value| value.to_str().ok()).map(str::to_string);
			let body = response.text().await.unwrap_or_default();
			return Err(ApiError::from_status(status.as_u16(), &body, request_id.as_deref()));
		}
		Ok(Box::pin(response.bytes_stream().map(|chunk| chunk.map_err(ApiError::from))))
	}

	pub async fn speech_to_text(&self, audio: Vec<u8>, file_name: &str, model_id: &str) -> ApiResult<String> {
		let form =
			multipart::Form::new().text("model_id", model_id.to_string()).part("file", file_part(audio, file_name));
		let body = self.multipart_json(Method::POST, "v1/speech-to-text", form).await?;
		body.get("text").and_then(Value::as_str).map(str::to_string).ok_or_else(ApiError::unexpected_response)
	}

	pub async fn sound_effect(
		&self,
		prompt: &str,
		duration_seconds: f64,
		looping: bool,
		prompt_influence: f64,
	) -> ApiResult<Bytes> {
		let body = json!({
			"text": prompt,
			"duration_seconds": duration_seconds,
			"loop": looping,
			"prompt_influence": prompt_influence,
			"model_id": SOUND_EFFECTS_MODEL,
		});
		let request =
			self.raw(Method::POST, "v1/sound-generation").query("output_format", "mp3_44100_128").json(&body)?;
		within(LONG_REQUEST_TIMEOUT, request.send_bytes()).await
	}

	pub async fn list_history(&self) -> ApiResult<Vec<HistoryItem>> {
		let body = self.json(Method::GET, "v1/history", &[("page_size", PAGE_SIZE.to_string())], None).await?;
		Ok(body
			.get("history")
			.and_then(Value::as_array)
			.into_iter()
			.flatten()
			.filter_map(|item| {
				Some(HistoryItem {
					id: optional_text(item, "history_item_id")?,
					date_unix: item.get("date_unix").and_then(Value::as_i64).unwrap_or(0),
					voice_name: text(item, "voice_name"),
					model_id: text(item, "model_id"),
					text: text(item, "text"),
				})
			})
			.collect())
	}

	pub async fn history_audio(&self, id: &str) -> ApiResult<Bytes> {
		within(DOWNLOAD_TIMEOUT, self.raw(Method::GET, format!("v1/history/{}/audio", segment(id))).send_bytes()).await
	}

	pub async fn delete_history_item(&self, id: &str) -> ApiResult<()> {
		self.ok(Method::DELETE, &format!("v1/history/{}", segment(id))).await
	}

	pub async fn list_pronunciation_dictionaries(&self) -> ApiResult<Vec<PronunciationDictionary>> {
		let body = self
			.json(Method::GET, "v1/pronunciation-dictionaries/", &[("page_size", PAGE_SIZE.to_string())], None)
			.await?;
		Ok(body
			.get("pronunciation_dictionaries")
			.and_then(Value::as_array)
			.into_iter()
			.flatten()
			.map(|item| PronunciationDictionary { name: text(item, "name"), description: text(item, "description") })
			.collect())
	}

	pub async fn create_dub_project(
		&self,
		path: &Path,
		name: &str,
		source_language: Option<&str>,
	) -> ApiResult<String> {
		let bytes = read_file(path).await?;
		let mut form = multipart::Form::new()
			.text("reference", name.to_string())
			.text("model_id", DUBBING_MODEL)
			.part("file", file_part(bytes, &file_name_of(path)));
		if let Some(language) = source_language {
			form = form.text("source_language", language.to_string());
		}
		let body = self.multipart_json(Method::POST, "v1/dubbing/project", form).await?;
		optional_text(&body, "project_id").ok_or_else(ApiError::unexpected_response)
	}

	pub async fn dub_project(&self, project_id: &str) -> ApiResult<DubProject> {
		let body = self.json(Method::GET, &format!("v1/dubbing/project/{}", segment(project_id)), &[], None).await?;
		Ok(parse_dub_project(&body))
	}

	pub async fn list_dub_projects(&self) -> ApiResult<Vec<DubProject>> {
		let mut projects = Vec::new();
		let mut cursor: Option<String> = None;
		for _ in 0..MAX_PAGES {
			let mut query = vec![("page_size", PAGE_SIZE.to_string())];
			if let Some(cursor) = &cursor {
				query.push(("cursor", cursor.clone()));
			}
			let page = self.json(Method::GET, "v1/dubbing/project", &query, None).await?;
			projects
				.extend(page.get("projects").and_then(Value::as_array).into_iter().flatten().map(parse_dub_project));
			cursor = optional_text(&page, "next_cursor");
			if cursor.is_none() {
				break;
			}
		}
		Ok(projects)
	}

	pub async fn delete_dub_project(&self, project_id: &str) -> ApiResult<()> {
		self.ok(Method::DELETE, &format!("v1/dubbing/project/{}", segment(project_id))).await
	}

	pub async fn create_dub_language(&self, project_id: &str, target_language: &str) -> ApiResult<String> {
		let body = self
			.json(
				Method::POST,
				&format!("v1/dubbing/project/{}/language", segment(project_id)),
				&[],
				Some(json!({ "target_language": target_language })),
			)
			.await?;
		optional_text(&body, "language_id").ok_or_else(ApiError::unexpected_response)
	}

	pub async fn dub_language(&self, project_id: &str, language_id: &str) -> ApiResult<DubLanguage> {
		let body = self
			.json(
				Method::GET,
				&format!("v1/dubbing/project/{}/language/{}", segment(project_id), segment(language_id)),
				&[],
				None,
			)
			.await?;
		Ok(parse_dub_language(&body))
	}

	pub async fn list_dub_languages(&self, project_id: &str) -> ApiResult<Vec<DubLanguage>> {
		let mut languages = Vec::new();
		let mut cursor: Option<String> = None;
		for _ in 0..MAX_PAGES {
			let mut query = vec![("page_size", PAGE_SIZE.to_string())];
			if let Some(cursor) = &cursor {
				query.push(("cursor", cursor.clone()));
			}
			let page = self
				.json(Method::GET, &format!("v1/dubbing/project/{}/language", segment(project_id)), &query, None)
				.await?;
			languages
				.extend(page.get("languages").and_then(Value::as_array).into_iter().flatten().map(parse_dub_language));
			cursor = optional_text(&page, "next_cursor");
			if cursor.is_none() {
				break;
			}
		}
		Ok(languages)
	}

	/// Downloads a finished dub or a voice preview. These are pre-signed public
	/// URLs, so the API key is deliberately not attached.
	pub async fn download_public(&self, url: &str) -> ApiResult<Bytes> {
		if url.is_empty() {
			return Err(ApiError::unexpected_response());
		}
		let response = self.http.get(url).timeout(DOWNLOAD_TIMEOUT).send().await?.error_for_status()?;
		Ok(response.bytes().await?)
	}
}

fn parse_dub_project(value: &Value) -> DubProject {
	DubProject {
		id: text(value, "project_id"),
		name: text(value, "reference"),
		status: text(value, "status"),
		source_language: text(value, "source_language"),
		created_at: value
			.get("created_at")
			.map(|created| created.as_str().map_or_else(|| created.to_string(), str::to_string))
			.unwrap_or_default(),
		error: value.get("error").map(|error| text(error, "error")).unwrap_or_default(),
	}
}

fn parse_dub_language(value: &Value) -> DubLanguage {
	DubLanguage {
		id: text(value, "language_id"),
		target_language: text(value, "target_language"),
		status: text(value, "status"),
		output_url: value.get("outputs").map(|outputs| text(outputs, "lossless_audio")).unwrap_or_default(),
		error: value.get("error").map(|error| text(error, "error")).unwrap_or_default(),
	}
}

#[cfg(test)]
mod tests {
	use std::future::pending;

	use super::*;

	#[test]
	fn voice_params_skip_unset_fields() {
		let params = VoiceParams { stability: Some(0.5), speed: Some(1.0), ..Default::default() };
		let value = params.to_json();
		assert_eq!(value.as_object().unwrap().len(), 2);
		assert!(value.get("style").is_none());
	}

	#[tokio::test]
	async fn stalled_calls_time_out_instead_of_hanging() {
		let stalled = pending::<Result<(), ApiError>>();
		let error = within(Duration::from_millis(20), stalled).await.unwrap_err();
		assert!(error.message.contains("did not respond"));
	}

	#[tokio::test]
	async fn downloading_without_a_url_is_an_error_not_a_request() {
		let error = ApiClient::new("test-key").download_public("").await.unwrap_err();
		assert!(error.message.contains("did not understand"));
	}

	#[test]
	fn parses_dub_language_without_outputs() {
		let language = parse_dub_language(&json!({"language_id": "l1", "target_language": "es", "status": "pending"}));
		assert_eq!(language.id, "l1");
		assert_eq!(language.output_url, "");
	}
}

/// Exercises the real request-building code against a local mock server.
#[cfg(test)]
mod mock_tests {
	use std::{
		env, fs,
		sync::{Arc, Mutex},
	};

	use tokio::{
		io::{AsyncReadExt, AsyncWriteExt},
		net::TcpListener,
		spawn,
	};

	use super::*;

	type Seen = Arc<Mutex<Vec<(String, String, bool)>>>;

	/// Answers every request with `reply(path)`; records (method, path, had_api_key_header).
	async fn mock_server(reply: fn(&str) -> (u16, &'static str)) -> (String, Seen) {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let base = format!("http://{}", listener.local_addr().unwrap());
		let seen: Seen = Arc::new(Mutex::new(Vec::new()));
		let record = seen.clone();
		spawn(async move {
			loop {
				let Ok((mut socket, _)) = listener.accept().await else { return };
				let record = record.clone();
				spawn(async move {
					let mut buffer = vec![0u8; 65536];
					let read = socket.read(&mut buffer).await.unwrap_or(0);
					let head = String::from_utf8_lossy(&buffer[..read]).to_string();
					let mut first = head.lines().next().unwrap_or_default().split_whitespace();
					let (method, target) =
						(first.next().unwrap_or_default().to_string(), first.next().unwrap_or_default().to_string());
					let has_key = head.to_lowercase().contains("xi-api-key: test-key");
					record.lock().unwrap().push((method, target.clone(), has_key));
					let (status, body) = reply(target.split('?').next().unwrap_or_default());
					let response = format!(
						"HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
						body.len()
					);
					let _ = socket.write_all(response.as_bytes()).await;
				});
			}
		});
		(base, seen)
	}

	fn client(base: &str) -> ApiClient {
		ApiClient { base_url: Some(base.to_string()), ..ApiClient::new("test-key") }
	}

	#[tokio::test]
	async fn models_voices_and_history_parse_and_send_the_key() {
		let (base, seen) = mock_server(|path| match path {
			"/v1/models" => (200, r#"[{"model_id":"eleven_v4","name":"Eleven v4","can_do_text_to_speech":true,"can_use_style":true,"new_field":{}}]"#),
			"/v2/voices" => (200, r#"{"voices":[{"voice_id":"b","name":"Bob","category":"premade","is_owner":false},{"voice_id":"a","name":"alice","preview_url":"http://x/p.mp3","is_owner":true}],"has_more":false}"#),
			"/v1/history" => (200, r#"{"history":[{"history_item_id":"h1","date_unix":1700000000,"voice_name":"Bob","model_id":"m","text":"hi"}]}"#),
			"/v1/pronunciation-dictionaries/" => (200, r#"{"pronunciation_dictionaries":[{"id":"d1","name":"Names"}]}"#),
			_ => (404, r#"{"detail":{"status":"not_found","message":"nope"}}"#),
		})
		.await;
		let api = client(&base);
		let models = api.list_models().await.unwrap();
		assert_eq!(models[0].id, "eleven_v4");
		let voices = api.list_voices().await.unwrap();
		assert_eq!(
			voices.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
			["alice", "Bob"],
			"sorted case-insensitively"
		);
		assert!(voices[0].is_owner && voices[0].preview_url.is_some());
		assert_eq!(api.list_history().await.unwrap()[0].id, "h1");
		assert_eq!(api.list_pronunciation_dictionaries().await.unwrap()[0].name, "Names");
		let missing = api.voice_settings("zzz").await.unwrap_err();
		assert_eq!(missing.status, Some(404));
		assert!(missing.message.contains("nope"));
		assert!(seen.lock().unwrap().iter().all(|(_, _, has_key)| *has_key), "xi-api-key header on every request");
	}

	#[tokio::test]
	async fn dubbing_project_flow_uses_the_project_api() {
		let (base, seen) = mock_server(|path| match path {
			"/v1/dubbing/project" => (200, r#"{"project_id":"p1","status":"preparing"}"#),
			"/v1/dubbing/project/p1" => (200, r#"{"project_id":"p1","reference":"Clip","status":"ready","source_language":"en"}"#),
			"/v1/dubbing/project/p1/language" => (200, r#"{"language_id":"l1","status":"pending","languages":[{"language_id":"l1","target_language":"es","status":"completed","outputs":{"lossless_audio":"http://x/a.flac"}}]}"#),
			"/v1/dubbing/project/p1/language/l1" => (200, r#"{"status":"completed","outputs":{"lossless_audio":"http://x/a.flac"}}"#),
			_ => (404, "{}"),
		})
		.await;
		let api = client(&base);
		let file = env::temp_dir().join("elevendesk2-mock-test.mp3");
		fs::write(&file, b"audio").unwrap();
		assert_eq!(api.create_dub_project(&file, "Clip", Some("en")).await.unwrap(), "p1");
		assert_eq!(api.dub_project("p1").await.unwrap().status, "ready");
		assert_eq!(api.create_dub_language("p1", "es").await.unwrap(), "l1");
		assert_eq!(api.dub_language("p1", "l1").await.unwrap().output_url, "http://x/a.flac");
		assert_eq!(api.list_dub_languages("p1").await.unwrap()[0].target_language, "es");
		let calls: Vec<String> = seen.lock().unwrap().iter().map(|(m, p, _)| format!("{m} {p}")).collect();
		assert!(calls.contains(&"POST /v1/dubbing/project".to_string()), "{calls:?}");
		assert!(calls.contains(&"POST /v1/dubbing/project/p1/language".to_string()), "{calls:?}");
	}
}

#[cfg(test)]
mod segment_tests {
	use super::segment;

	#[test]
	fn ids_are_encoded_as_a_single_path_segment() {
		assert_eq!(segment("abc-123_X.~"), "abc-123_X.~");
		assert_eq!(segment("a/b?c#d e"), "a%2Fb%3Fc%23d%20e");
		assert_eq!(segment("../x"), "..%2Fx");
	}
}
