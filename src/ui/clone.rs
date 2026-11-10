//! Clone Voice dialog: instant voice cloning from local audio files.

use std::{
	cell::RefCell,
	fmt::Write as _,
	fs,
	path::{Path, PathBuf},
	rc::Rc,
	sync::mpsc::Sender,
};

use wxdragon::prelude::*;

use super::{
	announce::Announcer,
	services::Services,
	widgets::{self, Form, Job, Status},
};
use crate::{
	api::{ApiResult, file_name_of},
	config, runtime,
};

pub const MAX_FILES: usize = 25;
const AUDIO_EXTENSIONS: [&str; 8] = ["mp3", "wav", "m4a", "flac", "ogg", "opus", "aac", "wma"];

fn is_audio_file(path: &Path) -> bool {
	path.extension()
		.and_then(|extension| extension.to_str())
		.is_some_and(|extension| AUDIO_EXTENSIONS.contains(&extension.to_lowercase().as_str()))
}

/// Recursively collects audio files below `directory`, sorted for a predictable order.
/// Directory links are not followed, so a link cycle cannot recurse forever.
fn audio_files_in(directory: &Path) -> Vec<PathBuf> {
	let mut found = Vec::new();
	collect_audio_files(directory, &mut found);
	found.sort();
	found
}

fn collect_audio_files(directory: &Path, found: &mut Vec<PathBuf>) {
	let Ok(entries) = fs::read_dir(directory) else { return };
	for entry in entries.flatten() {
		let path = entry.path();
		let Ok(kind) = entry.file_type() else { continue };
		if kind.is_dir() {
			collect_audio_files(&path, found);
		} else if is_audio_file(&path) {
			found.push(path);
		}
	}
}

// reason: displays a file size in megabytes, rounded to one decimal.
#[allow(clippy::cast_precision_loss)]
fn row_text(path: &Path) -> String {
	let megabytes = fs::metadata(path).map_or(0.0, |meta| meta.len() as f64 / 1_048_576.0);
	let folder = path.parent().map(|parent| parent.to_string_lossy().into_owned()).unwrap_or_default();
	format!("{}, {megabytes:.1} MB, in {folder}", file_name_of(path))
}

enum CloneEvent {
	Done { job: u64, result: ApiResult<String> },
}

struct State {
	services: Rc<Services>,
	dialog: Dialog,
	status: Status,
	name: TextCtrl,
	description: TextCtrl,
	list: ListBox,
	files: RefCell<Vec<PathBuf>>,
	rights: CheckBox,
	clone: Button,
	other_controls: Vec<Button>,
	job: Job,
	created: RefCell<Option<String>>,
	sender: Sender<CloneEvent>,
}

pub fn show(services: &Rc<Services>) -> Option<String> {
	let dialog = Dialog::builder(&services.frame, "Clone Voice").build();
	let announcer = Announcer::new(&dialog);
	let form = Form::new(&dialog);
	let (name, description) = form.group("Voice details", false, |group| {
		(group.line("Voice &name", ""), group.line("&Description (optional)", ""))
	});
	let (list, add_files, add_folder, remove) = form.group("Audio samples", true, |group| {
		let list = group.list("Audio &files");
		let row = widgets::button_row();
		let add_files = group.button(row, "&Add Files…");
		let add_folder = group.button(row, "Add F&older…");
		let remove = group.button(row, "&Remove File");
		group.add_row(row);
		(list, add_files, add_folder, remove)
	});
	let rights = form.check("I &confirm I have the rights and consent to clone this voice", false);
	let status = form.status(announcer);
	let row = widgets::button_row();
	let clone_button = form.button(row, "Clone &Voice");
	let close = form.button(row, "Close");
	form.add_row(row);
	dialog.set_sizer_and_fit(form.sizer, true);
	dialog.set_size(Size::new(700, 600));
	dialog.set_escape_id(close.get_id());

	let (state, _inbox) = widgets::with_inbox(
		|sender| {
			Rc::new(State {
				services: services.clone(),
				dialog,
				status,
				name,
				description,
				list,
				files: RefCell::new(Vec::new()),
				rights,
				clone: clone_button,
				other_controls: vec![add_files, add_folder, remove],
				job: Job::default(),
				created: RefCell::new(None),
				sender,
			})
		},
		State::handle,
	);
	add_files.on_click(widgets::handler(&state, State::add_files));
	add_folder.on_click(widgets::handler(&state, State::add_folder));
	remove.on_click(widgets::handler(&state, State::remove_file));
	state.clone.on_click(widgets::handler(&state, State::clone_or_cancel));
	close.on_click(widgets::handler(&state, State::request_close));
	state.name.set_focus();
	dialog.show_modal();
	state.job.cancel();
	dialog.destroy();

	state.created.borrow_mut().take()
}

impl State {
	fn add_paths(&self, paths: Vec<PathBuf>) {
		let mut files = self.files.borrow_mut();
		let (mut added, mut duplicates, mut over_limit) = (0, 0, 0);
		for path in paths {
			if files.contains(&path) {
				duplicates += 1;
			} else if files.len() >= MAX_FILES {
				over_limit += 1;
			} else {
				files.push(path);
				added += 1;
			}
		}
		widgets::fill_list(self.list, &files.iter().map(|path| row_text(path)).collect::<Vec<_>>());
		if !files.is_empty() && self.list.get_selection().is_none() {
			self.list.set_selection(0, true);
		}
		let mut message = format!("Added {added}. {} total.", files.len());
		if duplicates > 0 {
			write!(message, " Skipped {duplicates} already in the list.").expect("writing to a String cannot fail");
		}
		if over_limit > 0 {
			write!(message, " {over_limit} not added because the limit is {MAX_FILES} files.")
				.expect("writing to a String cannot fail");
		}
		drop(files);
		self.status.say(&message);
	}

	fn add_files(&self) {
		let paths = widgets::open_files(&self.dialog, "Choose audio files to clone from", widgets::AUDIO_FILTER, true);
		self.add_paths(paths);
	}

	fn add_folder(&self) {
		let start = config::load().output_directory;
		if let Some(directory) = widgets::choose_directory(&self.dialog, "Choose a folder of audio files", &start) {
			let found = audio_files_in(&directory);
			if found.is_empty() {
				self.status.say("That folder has no audio files.");
			} else {
				self.add_paths(found);
			}
		}
	}

	fn remove_file(&self) {
		let Some(index) = widgets::list_selection(self.list).filter(|index| *index < self.files.borrow().len()) else {
			self.status.say("Select a file to remove first.");
			return;
		};
		let mut files = self.files.borrow_mut();
		files.remove(index);
		widgets::fill_list(self.list, &files.iter().map(|path| row_text(path)).collect::<Vec<_>>());
		widgets::select_row_within(self.list, index, files.len());
		let count = files.len();
		drop(files);
		self.list.set_focus();
		self.status.say(&format!("File removed. {count} left."));
	}

	/// Esc and Close both land here; closing mid-upload would silently throw the clone away.
	fn request_close(&self) {
		if self.job.is_running()
			&& !widgets::confirm(
				&self.dialog,
				"Cloning in progress",
				"The voice is still being cloned. Cancel it and close?",
			) {
			self.clone.set_focus();
			return;
		}
		self.dialog.end_modal(ID_CANCEL);
	}

	fn clone_or_cancel(&self) {
		if self.job.cancel() {
			self.set_busy(false);
			self.status.say("Voice cloning cancelled.");
			return;
		}
		let name = self.name.get_value().trim().to_string();
		if name.is_empty() {
			self.status.say_at(&self.name, "Give the new voice a name.");
			return;
		}
		let files = self.files.borrow().clone();
		if files.is_empty() {
			self.status.say("Add at least one audio file first.");
			return;
		}
		if !self.rights.is_checked() {
			self.status.say_at(&self.rights, "Confirm that you have the rights and consent to clone this voice.");
			return;
		}
		let Some(api) = self.services.api_or_say(self.status) else { return };
		let description = self.description.get_value().trim().to_string();
		self.set_busy(true);
		self.status.set(&format!("Cloning the voice from {} files…", files.len()));
		let job = self.job.begin();
		let sender = self.sender.clone();
		let handle = runtime::spawn(async move {
			let result = api.clone_voice(&name, &description, &files).await;
			let _ = sender.send(CloneEvent::Done { job, result });
		});
		self.job.attach(handle);
	}

	fn set_busy(&self, busy: bool) {
		self.clone.set_label(if busy { "Cancel &voice cloning" } else { "Clone &Voice" });
		for button in &self.other_controls {
			button.enable(!busy);
		}
	}

	fn handle(&self, event: CloneEvent) {
		let CloneEvent::Done { job, result } = event;
		if !self.job.finish(job) {
			return;
		}
		self.set_busy(false);
		match result {
			Ok(voice_id) => {
				*self.created.borrow_mut() = Some(voice_id);
				self.dialog.end_modal(ID_OK);
			}
			Err(error) => self.status.say_at(&self.clone, &error.message),
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{env::temp_dir, process::id};

	use super::*;

	#[test]
	fn finds_audio_recursively_in_a_stable_order() {
		let root = temp_dir().join(format!("elevendesk2-clone-test-{}", id()));
		let nested = root.join("nested");
		fs::create_dir_all(&nested).unwrap();
		for name in ["b.MP3", "a.wav", "notes.txt"] {
			fs::write(root.join(name), b"x").unwrap();
		}
		fs::write(nested.join("c.flac"), b"x").unwrap();
		let names: Vec<String> = audio_files_in(&root).iter().map(|path| file_name_of(path)).collect();
		assert_eq!(names, ["a.wav", "b.MP3", "c.flac"]);
		fs::remove_dir_all(&root).unwrap();
	}
}
