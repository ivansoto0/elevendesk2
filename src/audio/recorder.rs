//! Microphone capture to a temporary 16-bit mono WAV file, used by Speech to Speech.
//! cpal streams are not `Send`, so a worker thread owns the stream for its whole life.

use std::{
	env, fs,
	path::PathBuf,
	process,
	sync::{
		Arc, Mutex,
		mpsc::{self, Sender},
	},
	thread,
};

use hound::{SampleFormat as WavSampleFormat, WavSpec, WavWriter};
use rodio::{
	DeviceTrait,
	cpal::{
		SampleFormat, Stream, default_host,
		traits::{HostTrait, StreamTrait},
	},
};

pub struct Recorder {
	stop: Sender<()>,
	finished: mpsc::Receiver<Result<PathBuf, String>>,
}

type SharedSamples = Arc<Mutex<Vec<f32>>>;

fn push_mono<T: Copy>(shared: &SharedSamples, data: &[T], channels: usize, to_float: impl Fn(T) -> f32) {
	if let Ok(mut samples) = shared.lock() {
		for frame in data.chunks(channels.max(1)) {
			samples.push(
				frame.iter().map(|sample| to_float(*sample)).sum::<f32>()
					/ f32::from(u16::try_from(frame.len()).unwrap_or(u16::MAX)),
			);
		}
	}
}

impl Recorder {
	/// Starts capturing from the named input device (empty name means the system default).
	pub fn start(device_name: &str) -> Result<Recorder, String> {
		let (stop, stop_receiver) = mpsc::channel::<()>();
		let (ready_sender, ready_receiver) = mpsc::channel::<Result<(), String>>();
		let (finished_sender, finished) = mpsc::channel();
		let device_name = device_name.to_string();
		thread::Builder::new()
			.name("elevendesk-recorder".into())
			.spawn(move || {
				let samples: SharedSamples = Arc::new(Mutex::new(Vec::new()));
				match open_stream(&device_name, &samples) {
					Ok((stream, rate)) => {
						let _ = ready_sender.send(Ok(()));
						let _ = stop_receiver.recv();
						drop(stream);
						let captured = samples.lock().map(|samples| samples.clone()).unwrap_or_default();
						let _ = finished_sender.send(write_wav(&captured, rate));
					}
					Err(message) => {
						let _ = ready_sender.send(Err(message));
					}
				}
			})
			.map_err(|error| format!("Recording could not be started: {error}"))?;
		ready_receiver.recv().map_err(|_| "Recording could not be started.".to_string())??;
		Ok(Recorder { stop, finished })
	}

	pub fn stop(self) -> Result<PathBuf, String> {
		let _ = self.stop.send(());
		self.finished.recv().map_err(|_| "The recording was lost.".to_string())?
	}
}

fn open_stream(device_name: &str, samples: &SharedSamples) -> Result<(Stream, u32), String> {
	let host = default_host();
	let device = if device_name.is_empty() {
		host.default_input_device()
	} else {
		host.input_devices()
			.ok()
			.and_then(|mut devices| devices.find(|device| device.description().is_ok_and(|d| d.name() == device_name)))
	}
	.ok_or("The selected input device is not available.")?;
	let supported =
		device.default_input_config().map_err(|error| format!("Recording could not be started: {error}"))?;
	let channels = supported.channels() as usize;
	let rate = supported.sample_rate();
	let config = supported.config();
	let shared = Arc::clone(samples);
	let on_error = |error| eprintln!("recording stream error: {error}");
	let stream = match supported.sample_format() {
		SampleFormat::F32 => device.build_input_stream(
			config,
			move |data: &[f32], _| push_mono(&shared, data, channels, |s| s),
			on_error,
			None,
		),
		SampleFormat::I16 => device.build_input_stream(
			config,
			move |data: &[i16], _| push_mono(&shared, data, channels, |s| f32::from(s) / 32_768.0),
			on_error,
			None,
		),
		SampleFormat::U16 => device.build_input_stream(
			config,
			move |data: &[u16], _| push_mono(&shared, data, channels, |s| (f32::from(s) - 32_768.0) / 32_768.0),
			on_error,
			None,
		),
		other => return Err(format!("The input device uses an unsupported sample format ({other:?}).")),
	}
	.map_err(|error| format!("Recording could not be started: {error}"))?;
	stream.play().map_err(|error| format!("Recording could not be started: {error}"))?;
	Ok((stream, rate))
}

/// Removes the temporary recording left behind by `write_wav`, if any.
pub fn remove_temp_recording() {
	let _ = fs::remove_file(recording_path());
}

fn recording_path() -> PathBuf {
	env::temp_dir().join(format!("elevendesk2-recording-{}.wav", process::id()))
}

// Clamped float to 16-bit sample conversion is the intended quantisation.
#[allow(clippy::cast_possible_truncation)]
fn write_wav(samples: &[f32], rate: u32) -> Result<PathBuf, String> {
	if samples.is_empty() {
		return Err("Nothing was recorded.".to_string());
	}
	let path = recording_path();
	let spec = WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: WavSampleFormat::Int };
	let mut writer =
		WavWriter::create(&path, spec).map_err(|error| format!("The recording could not be saved: {error}"))?;
	for sample in samples {
		writer
			.write_sample((sample.clamp(-1.0, 1.0) * 32_767.0) as i16)
			.map_err(|error| format!("The recording could not be saved: {error}"))?;
	}
	writer.finalize().map_err(|error| format!("The recording could not be saved: {error}"))?;
	Ok(path)
}
