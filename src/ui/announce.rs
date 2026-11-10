//! Screen reader announcements for changes that do not move keyboard focus.
//! One hidden live region per top-level window (and per modal dialog, because a
//! modal dialog cannot rely on the main window's live region).

use std::cell::RefCell;

use live_region::{Priority, announce_with_priority, set_live_region};
use wxdragon::prelude::*;

thread_local! {
	static LAST_MESSAGE: RefCell<String> = const { RefCell::new(String::new()) };
}

#[derive(Clone, Copy)]
pub struct Announcer {
	label: StaticText,
}

impl Announcer {
	pub fn new(parent: &impl WxWidget) -> Announcer {
		let label = StaticText::builder(parent).with_label("").with_size(Size::new(0, 0)).build();
		label.show(false);
		if !set_live_region(&label) {
			eprintln!("live region setup failed; announcements will rely on status fields only");
		}
		Announcer { label }
	}

	/// Interrupting announcement for results the user is waiting for or must know about.
	pub fn say(self, message: &str) {
		LAST_MESSAGE.with(|last| *last.borrow_mut() = message.to_string());
		announce_with_priority(self.label, message, Priority::High);
	}

	/// Re-speaks the most recent announcement ("Repeat last announcement").
	pub fn repeat_last(self) {
		let message = LAST_MESSAGE.with(|last| last.borrow().clone());
		if message.is_empty() {
			self.say("Nothing to repeat.");
		} else {
			announce_with_priority(self.label, &message, Priority::High);
		}
	}
}
