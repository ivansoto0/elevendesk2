//! Voice Design tab: describe a voice in words and add it to your account.

use std::{cell::RefCell, rc::Rc, sync::mpsc::Sender};

use wxdragon::prelude::*;

use super::{
	services::Services,
	widgets::{self, Form, Job, Page, Status},
};
use crate::{
	api::{ApiError, ApiResult},
	runtime,
};

// ElevenLabs rejects shorter descriptions, so say so before the round trip.
const MIN_DESCRIPTION: usize = 20;
const MAX_DESCRIPTION: usize = 1000;

enum DesignEvent {
	Done { job: u64, result: ApiResult<(String, String)> },
}

struct Inner {
	services: Rc<Services>,
	panel: Panel,
	status: Status,
	description: TextCtrl,
	name: TextCtrl,
	design: Button,
	result: TextCtrl,
	use_voice: Button,
	designed: RefCell<Option<String>>,
	job: Job,
	sender: Sender<DesignEvent>,
	on_use: Rc<dyn Fn(&str)>,
}

pub struct DesignPanel {
	inner: Rc<Inner>,
	_inbox: super::inbox::Inbox<DesignEvent>,
}

impl DesignPanel {
	/// `on_use` is called with a voice ID when the user asks to use the new voice in Text to Speech.
	pub fn new(parent: Notebook, services: Rc<Services>, on_use: Rc<dyn Fn(&str)>) -> DesignPanel {
		let panel = Panel::builder(&parent).build();
		let form = Form::new(&panel);
		let (description, name, design) = form.group("Voice description", false, |group| {
			let description = group.area("&Describe the voice (20 to 1000 characters)", false);
			let name = group.line("Voice &name", "");
			let row = widgets::button_row();
			let design = group.button(row, "Design &Voice");
			group.add_row(row);
			(description, name, design)
		});
		let (result, use_voice) = form.group("Result", true, |group| {
			let result = group.area("&Result", true);
			let row = widgets::button_row();
			let use_voice = group.button(row, "&Use in Text to Speech");
			group.add_row(row);
			(result, use_voice)
		});
		use_voice.enable(false);
		let status = form.status(services.announcer);
		form.finish();
		let (inner, inbox) = widgets::with_inbox(
			|sender| {
				Rc::new(Inner {
					services,
					panel,
					status,
					description,
					name,
					design,
					result,
					use_voice,
					designed: RefCell::new(None),
					job: Job::default(),
					sender,
					on_use,
				})
			},
			Inner::handle,
		);
		inner.design.on_click(widgets::handler(&inner, Inner::design_or_cancel));
		inner.use_voice.on_click(widgets::handler(&inner, Inner::use_designed));
		DesignPanel { inner, _inbox: inbox }
	}
}

impl Inner {
	fn design_or_cancel(&self) {
		if self.job.is_running() {
			self.cancel();
			return;
		}
		let description = self.description.get_value().trim().to_string();
		let length = description.chars().count();
		if !(MIN_DESCRIPTION..=MAX_DESCRIPTION).contains(&length) {
			self.status.say_at(
				&self.description,
				&format!("The description needs {MIN_DESCRIPTION} to {MAX_DESCRIPTION} characters; it has {length}."),
			);
			return;
		}
		let name = self.name.get_value().trim().to_string();
		if name.is_empty() {
			self.status.say_at(&self.name, "Give the new voice a name.");
			return;
		}
		let Some(api) = self.services.api_or_say(self.status) else { return };
		let job = self.job.begin();
		self.design.set_label("Cancel &voice design");
		widgets::set_enabled(self.use_voice, false, &self.design);
		self.status.say("Designing the voice…");
		let sender = self.sender.clone();
		let handle = runtime::spawn(async move {
			let result = api.design_voice(&description, &name).await;
			let _ = sender.send(DesignEvent::Done { job, result });
		});
		self.job.attach(handle);
	}

	fn cancel(&self) {
		self.job.cancel();
		self.reset_button();
		self.status.say("Voice design cancelled.");
	}

	fn reset_button(&self) {
		self.design.set_label("Design &Voice");
	}

	fn handle(&self, event: DesignEvent) {
		let DesignEvent::Done { job, result } = event;
		if !self.job.finish(job) {
			return;
		}
		self.reset_button();
		match result {
			Ok((voice_id, name)) => {
				self.result.set_value(&format!("Name: {name}\nVoice ID: {voice_id}"));
				*self.designed.borrow_mut() = Some(voice_id);
				self.use_voice.enable(true);
				self.services.reload_voices(None);
				self.status.say(&format!("Voice {name} was created and added to your account."));
			}
			Err(ApiError { message, .. }) => self.status.say(&message),
		}
	}

	fn use_designed(&self) {
		if let Some(id) = self.designed.borrow().clone() {
			(self.on_use)(&id);
		}
	}
}

impl Page for DesignPanel {
	fn panel(&self) -> Panel {
		self.inner.panel
	}

	fn focus_first(&self) {
		self.inner.description.set_focus();
	}

	fn status_field(&self) -> TextCtrl {
		self.inner.status.field
	}

	fn primary_action(&self) {
		self.inner.design_or_cancel();
	}

	fn stop(&self) {
		if self.inner.job.is_running() {
			self.inner.cancel();
		}
	}
}
