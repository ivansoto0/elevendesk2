//! Hands results from worker threads to the GUI thread.
//!
//! wx widgets are not thread-safe, so workers never touch them. Instead a worker
//! sends a message through an `Inbox`'s sender; one shared timer on the main
//! frame drains every live inbox on the GUI thread and runs its handler.

use std::{
	cell::{Cell, RefCell},
	rc::Rc,
	sync::mpsc::{self, Receiver, Sender},
};

use wxdragon::{prelude::*, timer::Timer};

const POLL_MS: i32 = 40;

type Poller = Rc<RefCell<dyn FnMut()>>;

thread_local! {
	static POLLERS: RefCell<Vec<(u64, Poller)>> = const { RefCell::new(Vec::new()) };
	static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

/// Keeps the receiving side alive; dropping it unregisters the handler.
pub struct Inbox<M> {
	sender: Sender<M>,
	id: u64,
}

impl<M: Send + 'static> Inbox<M> {
	pub fn new(mut handler: impl FnMut(M) + 'static) -> Inbox<M> {
		let (sender, receiver): (Sender<M>, Receiver<M>) = mpsc::channel();
		let id = NEXT_ID.with(|next| {
			let id = next.get();
			next.set(id + 1);
			id
		});
		let poller: Poller = Rc::new(RefCell::new(move || {
			while let Ok(message) = receiver.try_recv() {
				handler(message);
			}
		}));
		POLLERS.with(|pollers| pollers.borrow_mut().push((id, poller)));
		Inbox { sender, id }
	}

	pub fn sender(&self) -> Sender<M> {
		self.sender.clone()
	}
}

impl<M> Drop for Inbox<M> {
	fn drop(&mut self) {
		let id = self.id;
		POLLERS.with(|pollers| pollers.borrow_mut().retain(|(existing, _)| *existing != id));
	}
}

fn poll_all() {
	// Snapshot first: handlers may open dialogs (nested event loops) that add or
	// remove inboxes and re-enter this function.
	let snapshot: Vec<Poller> =
		POLLERS.with(|pollers| pollers.borrow().iter().map(|(_, poller)| poller.clone()).collect());
	for poller in snapshot {
		// A poller that is already running (re-entrant tick) is simply skipped.
		if let Ok(mut poller) = poller.try_borrow_mut() {
			poller();
		}
	}
}

/// Starts the single timer that drives every inbox. Keep the returned value alive
/// until the frame closes, then call `stop`.
pub struct Pump {
	timer: Timer<Frame>,
}

impl Pump {
	pub fn start(frame: &Frame) -> Pump {
		let timer = Timer::new(frame);
		timer.on_tick(|_| poll_all());
		timer.start(POLL_MS, false);
		Pump { timer }
	}

	pub fn stop(&self) {
		self.timer.stop();
	}
}
