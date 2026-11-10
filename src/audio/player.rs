//! Playback engine. One dedicated thread owns the audio device and a rodio
//! `Player`, so decoding, buffering and device I/O never touch the GUI thread.
//! The GUI talks to it through a cheap, cloneable `PlayerHandle`.
//!
//! A "session" is one clip or one live stream. Starting a new session, or
//! stopping, ends the previous one with a `Stopped` event so whichever panel
//! owned it can reset its buttons.

use std::{
	io::Cursor,
	mem,
	num::NonZero,
	sync::mpsc::{self, RecvTimeoutError, Sender},
	thread,
	time::Duration,
};

use rodio::{
	Decoder, DeviceSinkBuilder, DeviceTrait, MixerDeviceSink, Player,
	buffer::SamplesBuffer,
	cpal::{default_host, traits::HostTrait},
};

use super::encode::{Playable, STREAM_RATE};

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
	Started,
	Paused,
	Resumed,
	/// Reached the end of the audio.
	Finished,
	/// Ended early: stopped by the user or replaced by another session.
	Stopped,
	Error(String),
}

type AppendToPlayer = Box<dyn FnOnce(&Player)>;
pub type Listener = Box<dyn Fn(PlayerEvent) + Send + 'static>;

enum Command {
	Clip(Playable, Listener),
	BeginStream(Listener),
	Feed(Vec<i16>),
	EndStream,
	Pause,
	Resume,
	Stop,
	SetDevice(String),
}

#[derive(Clone)]
pub struct PlayerHandle {
	commands: Sender<Command>,
}

impl PlayerHandle {
	pub fn spawn(output_device: String) -> PlayerHandle {
		let (commands, receiver) = mpsc::channel();
		thread::Builder::new()
			.name("elevendesk-audio".into())
			.spawn(move || Engine::new(output_device).run(&receiver))
			.expect("failed to start the audio thread");
		PlayerHandle { commands }
	}

	fn send(&self, command: Command) {
		// The engine thread only exits when every handle is gone.
		let _ = self.commands.send(command);
	}

	pub fn play_clip(&self, playable: Playable, listener: Listener) {
		self.send(Command::Clip(playable, listener));
	}

	pub fn begin_stream(&self, listener: Listener) {
		self.send(Command::BeginStream(listener));
	}

	pub fn feed(&self, samples: Vec<i16>) {
		self.send(Command::Feed(samples));
	}

	pub fn end_stream(&self) {
		self.send(Command::EndStream);
	}

	pub fn pause(&self) {
		self.send(Command::Pause);
	}

	pub fn resume(&self) {
		self.send(Command::Resume);
	}

	pub fn stop(&self) {
		self.send(Command::Stop);
	}

	pub fn set_device(&self, name: &str) {
		self.send(Command::SetDevice(name.to_string()));
	}
}

struct Session {
	listener: Listener,
	paused: bool,
	stream: Option<StreamState>,
}

struct StreamState {
	buffered: Vec<i16>,
	started: bool,
	ended: bool,
}

impl Session {
	fn notify(&self, event: PlayerEvent) {
		(self.listener)(event);
	}

	fn flush_stream(&mut self, player: &Player) {
		let Some(stream) = self.stream.as_mut() else { return };
		let chunk = mem::take(&mut stream.buffered);
		if chunk.is_empty() {
			return;
		}
		player.append(mono_buffer(to_float(&chunk), STREAM_RATE));
		if !stream.started {
			stream.started = true;
			self.notify(PlayerEvent::Started);
		}
	}
}

struct Engine {
	device_name: String,
	sink: Option<MixerDeviceSink>,
	player: Option<Player>,
	session: Option<Session>,
}

// A quarter second of audio is buffered before a live stream starts, so a slow network does not make speech stutter.
const INITIAL_BUFFER_SAMPLES: usize = (STREAM_RATE / 4) as usize;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

fn to_float(samples: &[i16]) -> Vec<f32> {
	samples.iter().map(|sample| f32::from(*sample) / 32_768.0).collect()
}

fn mono_buffer(samples: Vec<f32>, rate: u32) -> SamplesBuffer {
	SamplesBuffer::new(NonZero::new(1).unwrap(), NonZero::new(rate.max(1)).unwrap(), samples)
}

impl Engine {
	fn new(device_name: String) -> Engine {
		Engine { device_name, sink: None, player: None, session: None }
	}

	fn run(mut self, receiver: &mpsc::Receiver<Command>) {
		loop {
			match receiver.recv_timeout(POLL_INTERVAL) {
				Ok(command) => self.handle(command),
				Err(RecvTimeoutError::Timeout) => {}
				Err(RecvTimeoutError::Disconnected) => break,
			}
			self.check_finished();
		}
		self.end_session(false);
	}

	fn open_sink(&mut self) -> Result<(), String> {
		if self.sink.is_some() {
			return Ok(());
		}
		let device = if self.device_name.is_empty() {
			None
		} else {
			default_host().output_devices().ok().and_then(|mut devices| {
				devices.find(|device| device.description().is_ok_and(|d| d.name() == self.device_name))
			})
		};
		let builder = match device {
			Some(device) => DeviceSinkBuilder::from_device(device),
			None => DeviceSinkBuilder::from_default_device(),
		}
		.map_err(|error| format!("The audio output device could not be opened: {error}"))?;
		let mut sink =
			builder.open_stream().map_err(|error| format!("The audio output device could not be opened: {error}"))?;
		sink.log_on_drop(false);
		self.sink = Some(sink);
		Ok(())
	}

	fn begin_session(&mut self, listener: Listener, stream: Option<StreamState>) -> bool {
		self.end_session(true);
		if let Err(message) = self.open_sink() {
			listener(PlayerEvent::Error(message));
			return false;
		}
		let sink = self.sink.as_ref().expect("sink opened above");
		self.player = Some(Player::connect_new(sink.mixer()));
		self.session = Some(Session { listener, paused: false, stream });
		true
	}

	fn end_session(&mut self, announce_stop: bool) {
		if let Some(player) = self.player.take() {
			player.stop();
		}
		if let Some(session) = self.session.take()
			&& announce_stop
		{
			session.notify(PlayerEvent::Stopped);
		}
	}

	fn handle(&mut self, command: Command) {
		match command {
			Command::Clip(playable, listener) => self.play_clip(playable, listener),
			Command::BeginStream(listener) => {
				self.begin_session(listener, Some(StreamState { buffered: Vec::new(), started: false, ended: false }));
			}
			Command::Feed(samples) => self.feed(samples),
			Command::EndStream => self.end_stream(),
			Command::Pause => self.set_paused(true),
			Command::Resume => self.set_paused(false),
			Command::Stop => self.end_session(true),
			Command::SetDevice(name) => {
				self.end_session(true);
				self.sink = None;
				self.device_name = name;
			}
		}
	}

	fn play_clip(&mut self, playable: Playable, listener: Listener) {
		// Decode before touching the current session so a bad file does not cut off playing audio.
		let source: Result<AppendToPlayer, String> = match playable {
			Playable::Encoded(bytes) => match Decoder::try_from(Cursor::new(bytes)) {
				Ok(decoder) => Ok(Box::new(move |player: &Player| player.append(decoder))),
				Err(error) => Err(format!("This audio could not be decoded: {error}")),
			},
			Playable::Pcm { samples, rate } => {
				let buffer = mono_buffer(to_float(&samples), rate);
				Ok(Box::new(move |player: &Player| player.append(buffer)))
			}
		};
		match source {
			Err(message) => listener(PlayerEvent::Error(message)),
			Ok(append) => {
				if self.begin_session(listener, None) {
					append(self.player.as_ref().expect("player created by begin_session"));
					if let Some(session) = &self.session {
						session.notify(PlayerEvent::Started);
					}
				}
			}
		}
	}

	fn feed(&mut self, samples: Vec<i16>) {
		let (Some(session), Some(player)) = (self.session.as_mut(), self.player.as_ref()) else { return };
		let Some(stream) = session.stream.as_mut() else { return };
		stream.buffered.extend(samples);
		if stream.started || stream.buffered.len() >= INITIAL_BUFFER_SAMPLES {
			session.flush_stream(player);
		}
	}

	fn end_stream(&mut self) {
		let (Some(session), Some(player)) = (self.session.as_mut(), self.player.as_ref()) else { return };
		let Some(stream) = session.stream.as_mut() else { return };
		stream.ended = true;
		// Short clips never reach the initial buffer threshold; flush what is left.
		session.flush_stream(player);
	}

	fn set_paused(&mut self, paused: bool) {
		let (Some(session), Some(player)) = (self.session.as_mut(), self.player.as_ref()) else { return };
		if session.paused == paused {
			return;
		}
		session.paused = paused;
		if paused {
			player.pause();
			session.notify(PlayerEvent::Paused);
		} else {
			player.play();
			session.notify(PlayerEvent::Resumed);
		}
	}

	fn check_finished(&mut self) {
		let finished = match (&self.session, &self.player) {
			(Some(session), Some(player)) => {
				!session.paused
					&& player.empty()
					&& match &session.stream {
						None => true,
						Some(stream) => stream.ended && stream.buffered.is_empty(),
					}
			}
			_ => false,
		};
		if finished {
			if let Some(session) = self.session.take() {
				session.notify(PlayerEvent::Finished);
			}
			self.player = None;
		}
	}
}
