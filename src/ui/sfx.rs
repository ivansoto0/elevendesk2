//! Sound Effects tab: describe a sound, generate it, replay or save it.

use std::{rc::Rc, sync::mpsc::Sender};

use wxdragon::prelude::*;

use super::{
	services::Services,
	widgets::{self, Clip, ClipControls, Form, Job, Page, Status},
};
use crate::{api::ApiError, runtime};

const MIN_DURATION: f64 = 0.5;
const MAX_DURATION: f64 = 30.0;

enum SfxEvent {
	Done { job: u64, result: Result<Vec<u8>, ApiError> },
}

struct Inner {
	services: Rc<Services>,
	panel: Panel,
	status: Status,
	prompt: TextCtrl,
	duration: TextCtrl,
	looping: CheckBox,
	influence: Slider,
	generate: Button,
	clip: ClipControls,
	job: Job,
	sender: Sender<SfxEvent>,
}

pub struct SfxPanel {
	inner: Rc<Inner>,
	_inbox: super::inbox::Inbox<SfxEvent>,
}

impl SfxPanel {
	pub fn new(parent: Notebook, services: Rc<Services>) -> SfxPanel {
		let panel = Panel::builder(&parent).build();
		let form = Form::new(&panel);
		let (prompt, duration, looping, influence) = form.group("Sound", false, |group| {
			(
				group.line("&Describe the sound", ""),
				group.line("D&uration in seconds (0.5 to 30)", "5"),
				group.check("Loop s&moothly", false),
				group.slider("Prompt &influence, percent", 0, 100, 30),
			)
		});
		let row = widgets::button_row();
		let generate = form.button(row, "&Generate");
		form.add_row(row);
		let clip = form.group("Generated sound effect", false, |group| {
			ClipControls::new(group, services.clone(), "generated sound effect", "elevendesk-sound-effect")
		});
		let status = form.status(services.announcer);
		clip.attach_status(status);
		form.finish();
		let (inner, inbox) = widgets::with_inbox(
			|sender| {
				Rc::new(Inner {
					services,
					panel,
					status,
					prompt,
					duration,
					looping,
					influence,
					generate,
					clip,
					job: Job::default(),
					sender,
				})
			},
			Inner::handle,
		);
		inner.generate.on_click(widgets::handler(&inner, Inner::generate_or_cancel));
		SfxPanel { inner, _inbox: inbox }
	}
}

impl Inner {
	fn generate_or_cancel(&self) {
		if self.job.is_running() {
			self.cancel("Sound effect generation cancelled.");
			return;
		}
		let prompt = self.prompt.get_value().trim().to_string();
		if prompt.is_empty() {
			self.status.say_at(&self.prompt, "Describe the sound you want first.");
			return;
		}
		let Some(api) = self.services.api_or_say(self.status) else { return };
		let duration = match self.duration.get_value().trim().replace(',', ".").parse::<f64>() {
			Ok(seconds) if (MIN_DURATION..=MAX_DURATION).contains(&seconds) => seconds,
			_ => {
				self.status.say_at(
					&self.duration,
					&format!("Enter a duration between {MIN_DURATION} and {MAX_DURATION} seconds."),
				);
				return;
			}
		};
		let (looping, influence) = (self.looping.is_checked(), f64::from(self.influence.value()) / 100.0);
		let job = self.job.begin();
		self.clip.set_clip(None);
		self.generate.set_label("Cancel &generation");
		self.status.say("Generating sound effect…");
		let sender = self.sender.clone();
		let handle = runtime::spawn(async move {
			let result = api.sound_effect(&prompt, duration, looping, influence).await.map(|bytes| bytes.to_vec());
			let _ = sender.send(SfxEvent::Done { job, result });
		});
		self.job.attach(handle);
	}

	fn cancel(&self, message: &str) {
		self.job.cancel();
		self.reset_button();
		self.status.say(message);
	}

	fn reset_button(&self) {
		self.generate.set_label("&Generate");
	}

	fn handle(&self, event: SfxEvent) {
		let SfxEvent::Done { job, result } = event;
		if !self.job.finish(job) {
			return;
		}
		self.reset_button();
		match result {
			Ok(bytes) => {
				self.clip.set_clip(Some(Clip { bytes, format: "mp3_44100_128".into() }));
				self.status.say("Sound effect ready.");
			}
			Err(error) => self.status.say(&error.message),
		}
	}
}

impl Page for SfxPanel {
	fn panel(&self) -> Panel {
		self.inner.panel
	}

	fn focus_first(&self) {
		self.inner.prompt.set_focus();
	}

	fn status_field(&self) -> TextCtrl {
		self.inner.status.field
	}

	fn primary_action(&self) {
		self.inner.generate_or_cancel();
	}

	fn toggle_play(&self) {
		self.inner.clip.toggle_play();
	}

	fn stop(&self) {
		if self.inner.job.is_running() {
			self.inner.cancel("Sound effect generation cancelled.");
		} else {
			self.inner.clip.stop();
		}
	}

	fn save(&self) {
		self.inner.clip.save();
	}
}
