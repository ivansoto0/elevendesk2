//! Shared worker-side steps for features that stream 24 kHz PCM from ElevenLabs
//! (text to speech, speech to speech): feed the speaker as chunks arrive, then
//! convert the whole clip to the format the user chose.

use std::time::Duration;

use futures_util::StreamExt;
use tokio::{task::spawn_blocking, time::timeout};

use crate::{
	api::{ApiError, ApiResult, AudioStream},
	audio::{encode, player::PlayerHandle},
};

const PROGRESS_STEP_SECONDS: usize = 2;
// A stream that goes quiet for this long is treated as dead rather than waited on forever.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);

/// Plays chunks as they arrive and returns the complete clip. `on_seconds` is
/// called every couple of seconds of received audio (never per chunk) so progress
/// reporting cannot flood the screen reader or the GUI queue.
pub async fn pump_stream(
	mut stream: AudioStream,
	player: &PlayerHandle,
	mut on_seconds: impl FnMut(usize),
) -> ApiResult<Vec<i16>> {
	let mut pcm: Vec<i16> = Vec::new();
	let mut odd_byte: Option<u8> = None;
	let mut reported = 0usize;
	while let Some(chunk) = timeout(CHUNK_TIMEOUT, stream.next()).await.map_err(|_| ApiError::timed_out())? {
		let mut bytes = chunk?.to_vec();
		if let Some(byte) = odd_byte.take() {
			bytes.insert(0, byte);
		}
		if bytes.len() % 2 == 1 {
			odd_byte = bytes.pop();
		}
		let samples = encode::pcm_from_le_bytes(&bytes);
		player.feed(samples.clone());
		pcm.extend(samples);
		let seconds = pcm.len() / encode::STREAM_RATE as usize;
		if seconds >= reported + PROGRESS_STEP_SECONDS {
			reported = seconds;
			on_seconds(seconds);
		}
	}
	player.end_stream();
	if pcm.is_empty() {
		return Err(ApiError::unexpected_response());
	}
	Ok(pcm)
}

pub async fn encode_clip(pcm: Vec<i16>, format: String) -> ApiResult<Vec<u8>> {
	spawn_blocking(move || encode::encode_stream_pcm(&pcm, &format))
		.await
		.map_err(|error| ApiError::new(format!("Audio conversion stopped unexpectedly: {error}")))?
		.map_err(ApiError::new)
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use futures_util::stream::iter;

	use super::*;

	fn stream_of(chunks: &[&[u8]]) -> AudioStream {
		let items: Vec<ApiResult<Bytes>> = chunks.iter().map(|chunk| Ok(Bytes::copy_from_slice(chunk))).collect();
		Box::pin(iter(items))
	}

	#[tokio::test]
	async fn samples_split_across_chunks_are_rejoined() {
		let player = PlayerHandle::spawn(String::new());
		let stream = stream_of(&[&[1, 0, 2], &[0, 3], &[0]]);
		let pcm = pump_stream(stream, &player, |_| {}).await.unwrap();
		assert_eq!(pcm, [1, 2, 3]);
	}

	#[tokio::test]
	async fn an_empty_stream_is_an_error() {
		let player = PlayerHandle::spawn(String::new());
		assert!(pump_stream(stream_of(&[]), &player, |_| {}).await.is_err());
		assert!(pump_stream(stream_of(&[&[5]]), &player, |_| {}).await.is_err());
	}
}
