//! Local audio conversion. Generation always streams 24 kHz mono 16-bit PCM
//! (so playback can start early); the format the user picked is produced here,
//! exactly as the original ElevenDesk did.

use std::io::Cursor;

use hound::{SampleFormat, WavSpec, WavWriter};
use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, MonoPcm, Quality, max_required_buffer_size};

pub const STREAM_RATE: u32 = 24_000;
const MP3_RATE: u32 = 44_100;
const ULAW_RATE: u32 = 8_000;

pub struct OutputFormat {
	pub id: &'static str,
	pub label: &'static str,
}

/// First entry is the default. Older ElevenDesk choices are all still here.
pub const OUTPUT_FORMATS: &[OutputFormat] = &[
	OutputFormat { id: "mp3_44100_128", label: "MP3, 128 kbps" },
	OutputFormat { id: "mp3_44100_192", label: "MP3, 192 kbps" },
	OutputFormat { id: "wav_24000", label: "WAV, 24 kHz" },
	OutputFormat { id: "pcm_44100", label: "Raw PCM, 44.1 kHz" },
	OutputFormat { id: "ulaw_8000", label: "u-law, 8 kHz" },
];

pub fn pcm_from_le_bytes(bytes: &[u8]) -> Vec<i16> {
	bytes.chunks_exact(2).map(|pair| i16::from_le_bytes([pair[0], pair[1]])).collect()
}

// reason: sample-index and interpolation maths on audio lengths far below 2^52.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]
pub fn resample(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
	if from == 0 || to == 0 {
		return Vec::new();
	}
	if from == to || samples.is_empty() {
		return samples.to_vec();
	}
	let output_len = (samples.len() as u64 * u64::from(to) / u64::from(from)) as usize;
	let step = f64::from(from) / f64::from(to);
	(0..output_len)
		.map(|index| {
			let position = index as f64 * step;
			let base = position as usize;
			let fraction = position - base as f64;
			let first = f64::from(samples[base.min(samples.len() - 1)]);
			let second = f64::from(samples[(base + 1).min(samples.len() - 1)]);
			(first + (second - first) * fraction).round() as i16
		})
		.collect()
}

const ULAW_BIAS: i32 = 0x84;
const ULAW_CLIP: i32 = 32_635;

// reason: the bit-packed u-law byte is built in an i32 and always fits in a u8.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ulaw_encode_sample(sample: i16) -> u8 {
	let mut value = i32::from(sample);
	let sign = if value < 0 {
		value = -value;
		0x80
	} else {
		0
	};
	let value = value.min(ULAW_CLIP) + ULAW_BIAS;
	let mut exponent = 7;
	let mut mask = 0x4000;
	while exponent > 0 && value & mask == 0 {
		exponent -= 1;
		mask >>= 1;
	}
	let mantissa = (value >> (exponent + 3)) & 0x0F;
	!(sign | ((exponent << 4) | mantissa)) as u8
}

// reason: decoded u-law magnitudes are at most 32124, which fits in an i16.
#[allow(clippy::cast_possible_truncation)]
fn ulaw_decode_sample(byte: u8) -> i16 {
	let byte = !byte;
	let exponent = i32::from((byte >> 4) & 0x07);
	let mantissa = i32::from(byte & 0x0F);
	let magnitude = (((mantissa << 3) + ULAW_BIAS) << exponent) - ULAW_BIAS;
	if byte & 0x80 != 0 { -magnitude as i16 } else { magnitude as i16 }
}

fn pcm_to_le_bytes(samples: &[i16]) -> Vec<u8> {
	samples.iter().flat_map(|sample| sample.to_le_bytes()).collect()
}

fn encode_wav(samples: &[i16], rate: u32) -> Result<Vec<u8>, String> {
	let spec = WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: SampleFormat::Int };
	let mut buffer = Cursor::new(Vec::new());
	{
		let mut writer = WavWriter::new(&mut buffer, spec).map_err(|error| error.to_string())?;
		for sample in samples {
			writer.write_sample(*sample).map_err(|error| error.to_string())?;
		}
		writer.finalize().map_err(|error| error.to_string())?;
	}
	Ok(buffer.into_inner())
}

fn encode_mp3(samples: &[i16], kbps: u32) -> Result<Vec<u8>, String> {
	let bitrate = match kbps {
		192 => Bitrate::Kbps192,
		_ => Bitrate::Kbps128,
	};
	let mp3_error = |what: &str| format!("The MP3 encoder failed ({what}).");
	let mut builder = Builder::new().ok_or_else(|| mp3_error("start"))?;
	builder.set_num_channels(1).map_err(|_| mp3_error("channels"))?;
	builder.set_sample_rate(MP3_RATE).map_err(|_| mp3_error("sample rate"))?;
	builder.set_brate(bitrate).map_err(|_| mp3_error("bit rate"))?;
	builder.set_quality(Quality::Good).map_err(|_| mp3_error("quality"))?;
	let mut encoder = builder.build().map_err(|_| mp3_error("setup"))?;
	let resampled = resample(samples, STREAM_RATE, MP3_RATE);
	let mut out = Vec::with_capacity(max_required_buffer_size(resampled.len()));
	let written = encoder.encode(MonoPcm(&resampled), out.spare_capacity_mut()).map_err(|_| mp3_error("encode"))?;
	// SAFETY: the encoder initialised exactly `written` bytes of the spare capacity.
	unsafe { out.set_len(written) };
	out.reserve(7200);
	let flushed = encoder.flush::<FlushNoGap>(out.spare_capacity_mut()).map_err(|_| mp3_error("flush"))?;
	// SAFETY: as above, for the flushed bytes.
	unsafe { out.set_len(out.len() + flushed) };
	Ok(out)
}

/// Converts streamed 24 kHz PCM into the chosen output format's file bytes.
pub fn encode_stream_pcm(samples: &[i16], format_id: &str) -> Result<Vec<u8>, String> {
	if samples.is_empty() {
		return Err("There is no audio to convert.".to_string());
	}
	match format_id {
		"mp3_44100_128" => encode_mp3(samples, 128),
		"mp3_44100_192" => encode_mp3(samples, 192),
		"wav_24000" => encode_wav(samples, STREAM_RATE),
		"pcm_44100" => Ok(pcm_to_le_bytes(&resample(samples, STREAM_RATE, MP3_RATE))),
		"ulaw_8000" => Ok(resample(samples, STREAM_RATE, ULAW_RATE).into_iter().map(ulaw_encode_sample).collect()),
		other => Err(format!("The output format {other} is not supported.")),
	}
}

/// How a stored clip must be turned into sound.
pub enum Playable {
	/// A container rodio can decode (mp3, wav, flac, ...).
	Encoded(Vec<u8>),
	Pcm {
		samples: Vec<i16>,
		rate: u32,
	},
}

pub fn playable_from_output(bytes: Vec<u8>, format_id: &str) -> Playable {
	match format_id {
		"pcm_44100" => Playable::Pcm { samples: pcm_from_le_bytes(&bytes), rate: MP3_RATE },
		"ulaw_8000" => Playable::Pcm { samples: bytes.into_iter().map(ulaw_decode_sample).collect(), rate: ULAW_RATE },
		_ => Playable::Encoded(bytes),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// reason: synthetic test tone, small indices and a value bounded by 12000.
	#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
	fn tone(len: usize) -> Vec<i16> {
		(0..len).map(|i| ((i as f32 / 20.0).sin() * 12_000.0) as i16).collect()
	}

	#[test]
	fn resample_changes_length_proportionally() {
		assert_eq!(resample(&tone(2400), 24_000, 8_000).len(), 800);
		assert_eq!(resample(&tone(2400), 24_000, 44_100).len(), 4410);
		assert_eq!(resample(&tone(10), 24_000, 24_000), tone(10));
	}

	#[test]
	fn resample_with_a_zero_rate_is_empty_not_a_panic() {
		assert!(resample(&tone(100), 0, 8_000).is_empty());
		assert!(resample(&tone(100), 24_000, 0).is_empty());
	}

	#[test]
	fn odd_byte_counts_drop_the_dangling_byte() {
		assert_eq!(pcm_from_le_bytes(&[1, 0, 2, 0, 9]), [1, 2]);
		assert!(pcm_from_le_bytes(&[7]).is_empty());
	}

	#[test]
	fn empty_audio_is_rejected_in_every_format() {
		for format in OUTPUT_FORMATS {
			assert!(encode_stream_pcm(&[], format.id).is_err(), "{}", format.id);
		}
	}

	#[test]
	fn ulaw_round_trip_is_close() {
		for sample in [-30_000i16, -1000, -8, 0, 8, 1000, 30_000] {
			let decoded = i32::from(ulaw_decode_sample(ulaw_encode_sample(sample)));
			assert!((decoded - i32::from(sample)).abs() <= i32::from(sample).abs() / 15 + 16, "{sample} -> {decoded}");
		}
	}

	#[test]
	fn wav_and_mp3_are_valid_containers() {
		let wav = encode_stream_pcm(&tone(24_000), "wav_24000").unwrap();
		assert_eq!(&wav[..4], b"RIFF");
		let mp3 = encode_stream_pcm(&tone(24_000), "mp3_44100_128").unwrap();
		assert!(mp3.len() > 1000);
		assert!(encode_stream_pcm(&tone(10), "nope").is_err());
	}
}

#[cfg(test)]
mod decode_tests {
	use rodio::{Decoder, Source};

	use super::*;

	// reason: synthetic test tone, small indices and a value bounded by 10000.
	#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
	fn tone(len: usize) -> Vec<i16> {
		(0..len).map(|i| ((i as f32 / 12.0).sin() * 10_000.0) as i16).collect()
	}

	#[test]
	// reason: converting a few tens of thousands of decoded samples to seconds.
	#[allow(clippy::cast_precision_loss)]
	fn encoded_containers_decode_with_the_playback_engine() {
		for (format, minimum_seconds) in [("wav_24000", 0.9), ("mp3_44100_128", 0.9), ("mp3_44100_192", 0.9)] {
			let bytes = encode_stream_pcm(&tone(24_000), format).unwrap();
			let decoder = Decoder::try_from(Cursor::new(bytes)).unwrap_or_else(|error| panic!("{format}: {error}"));
			let rate = decoder.sample_rate().get() as f32;
			let channels = f32::from(decoder.channels().get());
			let seconds = decoder.count() as f32 / rate / channels;
			assert!(seconds >= minimum_seconds && seconds < 1.3, "{format} decoded to {seconds}s");
		}
	}

	#[test]
	fn raw_formats_become_playable_pcm() {
		let ulaw = encode_stream_pcm(&tone(2400), "ulaw_8000").unwrap();
		match playable_from_output(ulaw, "ulaw_8000") {
			Playable::Pcm { samples, rate } => {
				assert_eq!(rate, 8000);
				assert_eq!(samples.len(), 800);
			}
			Playable::Encoded(_) => panic!("ulaw must decode to PCM"),
		}
		assert!(matches!(playable_from_output(vec![0; 4], "flac"), Playable::Encoded(_)));
	}
}
