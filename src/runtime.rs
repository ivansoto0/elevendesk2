//! Shared tokio runtime. All network work runs here, never on the GUI thread.

use std::{
	future::Future,
	sync::{OnceLock, mpsc::Sender},
};

use tokio::{
	runtime::{Builder, Runtime},
	task::JoinHandle,
};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn runtime() -> &'static Runtime {
	RUNTIME.get_or_init(|| {
		Builder::new_multi_thread()
			.worker_threads(2)
			.enable_all()
			.thread_name("elevendesk-net")
			.build()
			.expect("failed to start the network runtime")
	})
}

pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
	F: Future + Send + 'static,
	F::Output: Send + 'static,
{
	runtime().spawn(future)
}

/// Runs `work` on the network runtime and sends its result to `inbox`.
pub fn spawn_and_send<T, F>(inbox: Sender<T>, work: F)
where
	T: Send + 'static,
	F: Future<Output = T> + Send + 'static,
{
	spawn(async move {
		let _ = inbox.send(work.await);
	});
}
