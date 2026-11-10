//! Dubbing tab (ElevenLabs dubbing project API): start a dub, watch its progress,
//! and manage projects and their target languages.

use std::{
	cell::{Cell, RefCell},
	collections::HashMap,
	path::PathBuf,
	rc::Rc,
	sync::mpsc::Sender,
	time::Duration,
};

use bytes::Bytes;
use tokio::time::sleep;
use wxdragon::prelude::*;

use super::{
	announce::Announcer,
	dub_languages,
	inbox::Inbox,
	services::Services,
	widgets::{self, Clip, ClipControls, Form, Job, Page, Status},
};
use crate::{
	api::{ApiClient, ApiError, ApiResult, DubLanguage, DubProject, file_name_of},
	audio::{encode::Playable, player::PlayerEvent},
	runtime,
};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_POLLS: usize = 1800;
const PROJECT_READY: &str = "ready";
const LANGUAGE_COMPLETE: &str = "completed";
const FAILED: &str = "failed";
const OUTPUT_FORMAT: &str = "flac";

fn failure(what: &str, detail: &str) -> ApiError {
	if detail.is_empty() {
		ApiError::new(format!("{what} failed."))
	} else {
		ApiError::new(format!("{what} failed: {detail}"))
	}
}

async fn wait_for_project(api: &ApiClient, project_id: &str, progress: &(impl Fn(String) + Sync)) -> ApiResult<()> {
	let mut last = String::new();
	for _ in 0..MAX_POLLS {
		let project = api.dub_project(project_id).await?;
		if project.status == FAILED {
			return Err(failure("Preparing the dub", &project.error));
		}
		if project.status == PROJECT_READY {
			return Ok(());
		}
		if project.status != last {
			progress(format!("Preparing the source: {}", project.status));
			last = project.status;
		}
		sleep(POLL_INTERVAL).await;
	}
	Err(ApiError::new("Preparing the dub took too long. Check the project again later."))
}

async fn dub_language(
	api: &ApiClient,
	project_id: &str,
	target: &str,
	progress: &(impl Fn(String) + Sync),
) -> ApiResult<DubLanguage> {
	let language_id = api.create_dub_language(project_id, target).await?;
	let mut last = String::new();
	for _ in 0..MAX_POLLS {
		let language = api.dub_language(project_id, &language_id).await?;
		if language.status == FAILED {
			return Err(failure("Dubbing", &language.error));
		}
		if language.status == LANGUAGE_COMPLETE {
			return Ok(language);
		}
		if language.status != last {
			progress(format!("Dubbing into {}: {}", dub_languages::name_for(target), language.status));
			last = language.status;
		}
		sleep(POLL_INTERVAL).await;
	}
	Err(ApiError::new("Dubbing took too long. Open the project later to check on it."))
}

#[derive(Clone)]
struct NewDub {
	path: PathBuf,
	name: String,
	source_language: Option<String>,
	target_language: String,
}

async fn run_new_dub(api: ApiClient, request: NewDub, progress: impl Fn(String) + Sync) -> ApiResult<(String, Bytes)> {
	progress("Uploading the source file…".into());
	let project_id = api.create_dub_project(&request.path, &request.name, request.source_language.as_deref()).await?;
	wait_for_project(&api, &project_id, &progress).await?;
	let language = dub_language(&api, &project_id, &request.target_language, &progress).await?;
	progress("Downloading the dubbed audio…".into());
	let audio = api.download_public(&language.output_url).await?;
	Ok((project_id, audio))
}

enum DubEvent {
	Projects { result: ApiResult<Vec<DubProject>>, quiet: bool },
	Progress { job: u64, message: String },
	Done { job: u64, result: ApiResult<(String, Bytes)> },
	Deleted(ApiResult<String>),
}

struct Inner {
	services: Rc<Services>,
	panel: Panel,
	status: Status,
	list: ListBox,
	cancel: Button,
	clip: ClipControls,
	projects: RefCell<Vec<DubProject>>,
	job: Job,
	sender: Sender<DubEvent>,
}

pub struct DubbingPanel {
	inner: Rc<Inner>,
	_inbox: Inbox<DubEvent>,
}

fn project_row(project: &DubProject) -> String {
	let mut parts = vec![
		if project.name.is_empty() { "Untitled project".to_string() } else { project.name.clone() },
		project.status.clone(),
	];
	parts.push(format!("from {}", dub_languages::name_for(&project.source_language)));
	let date = widgets::format_api_date(&project.created_at);
	if !date.is_empty() {
		parts.push(date);
	}
	parts.join(", ")
}

impl DubbingPanel {
	pub fn new(parent: Notebook, services: Rc<Services>) -> DubbingPanel {
		let panel = Panel::builder(&parent).build();
		let form = Form::new(&panel);
		let (list, new, open, refresh, delete) = form.group("Projects", true, |group| {
			let list = group.list("&Dubbing projects");
			let row = widgets::button_row();
			let new = group.button(row, "&New Dubbing…");
			let open = group.button(row, "&Manage Project…");
			let refresh = group.button(row, "&Refresh");
			let delete = group.button(row, "Dele&te Project");
			group.add_row(row);
			(list, new, open, refresh, delete)
		});
		let cancel = form.group("Running job", false, |group| {
			let row = widgets::button_row();
			let cancel = group.button(row, "&Cancel Dubbing");
			group.add_row(row);
			cancel
		});
		cancel.enable(false);
		let clip = form.group("Dubbed audio", false, |group| {
			ClipControls::new(group, services.clone(), "dubbed audio", "elevendesk-dub")
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
					list,
					cancel,
					clip,
					projects: RefCell::new(Vec::new()),
					job: Job::default(),
					sender,
				})
			},
			Inner::handle,
		);
		new.on_click(widgets::handler(&inner, Inner::new_dubbing));
		open.on_click(widgets::handler(&inner, Inner::open_project));
		refresh.on_click(widgets::handler(&inner, Inner::refresh));
		delete.on_click(widgets::handler(&inner, Inner::delete_project));
		inner.cancel.on_click(widgets::handler(&inner, Inner::cancel_job));
		inner.list.on_item_double_clicked(widgets::handler(&inner, Inner::open_project));
		DubbingPanel { inner, _inbox: inbox }
	}
}

impl Inner {
	fn selected_project(&self) -> Option<DubProject> {
		widgets::list_item(self.list, &self.projects)
	}

	fn refresh(&self) {
		self.load_projects(false);
	}

	/// `quiet` skips the spoken summary so it cannot talk over the result of the action that triggered the reload.
	fn load_projects(&self, quiet: bool) {
		let Some(api) = self.services.api_or_say(self.status) else { return };
		if !quiet {
			self.status.set("Loading dubbing projects…");
		}
		runtime::spawn_and_send(self.sender.clone(), async move {
			DubEvent::Projects { result: api.list_dub_projects().await, quiet }
		});
	}

	fn show_projects(&self, mut projects: Vec<DubProject>, keep: Option<String>) {
		projects.sort_by(|a, b| b.created_at.cmp(&a.created_at));
		widgets::fill_list(self.list, &projects.iter().map(project_row).collect::<Vec<_>>());
		let kept = keep.and_then(|id| projects.iter().position(|project| project.id == id));
		widgets::select_row_within(self.list, kept.unwrap_or(0), projects.len());
		*self.projects.borrow_mut() = projects;
	}

	fn new_dubbing(&self) {
		if self.job.is_running() {
			self.status.say("A dubbing job is already running. Cancel it first to start another.");
			return;
		}
		let Some(api) = self.services.api_or_say(self.status) else { return };
		let Some(request) = new_dub_dialog(&self.services) else {
			self.list.set_focus();
			return;
		};
		let job = self.job.begin();
		self.clip.set_clip(None);
		self.cancel.enable(true);
		self.status.say_at(&self.cancel, "Dubbing started.");
		let sender = self.sender.clone();
		let handle = runtime::spawn(async move {
			let progress_sender = sender.clone();
			let result = run_new_dub(api, request, move |message| {
				let _ = progress_sender.send(DubEvent::Progress { job, message });
			})
			.await;
			let _ = sender.send(DubEvent::Done { job, result });
		});
		self.job.attach(handle);
	}

	fn cancel_job(&self) {
		self.job.cancel();
		widgets::set_enabled(self.cancel, false, &self.list);
		self.status.say_at(&self.list, "Dubbing cancelled. The project stays in your account until you delete it.");
	}

	fn open_project(&self) {
		let Some(project) = self.selected_project() else {
			self.status.say("Select a dubbing project first.");
			return;
		};
		let Some(api) = self.services.api_or_say(self.status) else { return };
		let deleted = project_dialog(&self.services, api, project);
		self.load_projects(deleted);
		self.list.set_focus();
		if deleted {
			self.status.say("Project deleted.");
		}
	}

	fn delete_project(&self) {
		let Some(project) = self.selected_project() else {
			self.status.say("Select a dubbing project to delete.");
			return;
		};
		if !widgets::confirm(
			&self.panel,
			"Delete dubbing project",
			&format!("Delete the dubbing project \"{}\" and all of its languages?", project.name),
		) {
			self.list.set_focus();
			return;
		}
		let Some(api) = self.services.api_or_say(self.status) else { return };
		self.status.set("Deleting the project…");
		let id = project.id.clone();
		runtime::spawn_and_send(self.sender.clone(), async move {
			DubEvent::Deleted(api.delete_dub_project(&id).await.map(|()| id))
		});
	}

	fn handle(&self, event: DubEvent) {
		match event {
			DubEvent::Projects { result: Ok(projects), quiet } => {
				let keep = self.selected_project().map(|project| project.id);
				let count = projects.len();
				self.show_projects(projects, keep);
				let summary = format!("{count} dubbing projects.");
				if quiet {
					self.status.set(&summary);
				} else {
					self.status.say(&summary);
				}
			}
			DubEvent::Projects { result: Err(error), .. } | DubEvent::Deleted(Err(error)) => {
				self.status.say(&error.message);
			}
			DubEvent::Progress { job, message } if self.job.is_current(job) => self.status.set(&message),
			DubEvent::Done { job, result } if self.job.finish(job) => {
				widgets::set_enabled(self.cancel, false, &self.list);
				match result {
					Ok((_, audio)) => {
						self.clip.set_clip(Some(Clip { bytes: audio.to_vec(), format: OUTPUT_FORMAT.into() }));
						self.status.say("Dubbing complete.");
						self.load_projects(true);
					}
					Err(error) => self.status.say(&error.message),
				}
			}
			DubEvent::Deleted(Ok(id)) => {
				let keep = self.selected_project().map(|project| project.id).filter(|selected| *selected != id);
				let remaining: Vec<DubProject> =
					self.projects.borrow().iter().filter(|project| project.id != id).cloned().collect();
				self.show_projects(remaining, keep);
				self.list.set_focus();
				self.status.say("Project deleted.");
			}
			_ => {}
		}
	}
}

impl Page for DubbingPanel {
	fn panel(&self) -> Panel {
		self.inner.panel
	}

	fn focus_first(&self) {
		self.inner.list.set_focus();
	}

	fn status_field(&self) -> TextCtrl {
		self.inner.status.field
	}

	fn primary_action(&self) {
		self.inner.new_dubbing();
	}

	fn toggle_play(&self) {
		self.inner.clip.toggle_play();
	}

	fn stop(&self) {
		if self.inner.job.is_running() {
			self.inner.cancel_job();
		} else {
			self.inner.clip.stop();
		}
	}

	fn save(&self) {
		self.inner.clip.save();
	}

	fn open_file(&self) {
		self.inner.new_dubbing();
	}

	fn on_enter(&self) -> bool {
		let on_list = self.inner.list.has_focus();
		if on_list {
			self.inner.open_project();
		}
		on_list
	}

	fn on_delete(&self) -> bool {
		let on_list = self.inner.list.has_focus();
		if on_list {
			self.inner.delete_project();
		}
		on_list
	}
}

fn new_dub_dialog(services: &Rc<Services>) -> Option<NewDub> {
	let dialog = Dialog::builder(&services.frame, "New dubbing").build();
	let announcer = Announcer::new(&dialog);
	let form = Form::new(&dialog);
	let (browse, source, name) = form.group("Source", false, |group| {
		let row = widgets::button_row();
		let browse = group.button(row, "&Choose Source File…");
		group.add_row(row);
		let source = group.read_only_line("Source file", "No file chosen");
		let name = group.line("Project &name (defaults to the file name)", "");
		(browse, source, name)
	});
	let mut source_names = vec!["Auto-detect".to_string()];
	source_names.extend(dub_languages::names());
	let (source_language, target_language) = form.group("Languages", false, |group| {
		let source_language = group.choice("Source &language", &source_names);
		widgets::select_choice(source_language, 0);
		(source_language, group.choice("&Target language", &dub_languages::names()))
	});
	let status = form.status(announcer);
	let buttons = widgets::button_row();
	let start = form.button(buttons, "Start Dubbing");
	let cancel = form.button(buttons, "Cancel");
	form.add_row(buttons);
	dialog.set_sizer_and_fit(form.sizer, true);
	dialog.set_affirmative_id(start.get_id());
	dialog.set_escape_id(cancel.get_id());
	start.set_default();

	let chosen: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
	{
		let chosen = chosen.clone();
		browse.on_click(move |_| {
			let files =
				widgets::open_files(&dialog, "Choose audio or video to dub", widgets::AUDIO_VIDEO_FILTER, false);
			if let Some(path) = files.into_iter().next() {
				source.set_value(&path.to_string_lossy());
				if name.get_value().trim().is_empty() {
					name.set_value(
						&path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default(),
					);
				}
				status.set(&format!("Chosen {}.", file_name_of(&path)));
				*chosen.borrow_mut() = Some(path);
			}
			browse.set_focus();
		});
	}
	let result: Rc<RefCell<Option<NewDub>>> = Rc::new(RefCell::new(None));
	{
		let (chosen, result) = (chosen.clone(), result.clone());
		start.on_click(move |_| {
			let Some(path) = chosen.borrow().clone() else {
				browse.set_focus();
				status.say("Choose a source file first.");
				return;
			};
			let Some(target) = widgets::choice_selection(target_language) else {
				target_language.set_focus();
				status.say("Choose a target language first.");
				return;
			};
			let source_index = widgets::choice_selection(source_language).unwrap_or(0);
			let typed_name = name.get_value().trim().to_string();
			*result.borrow_mut() = Some(NewDub {
				name: if typed_name.is_empty() { file_name_of(&path) } else { typed_name },
				path,
				source_language: (source_index > 0).then(|| dub_languages::LANGUAGES[source_index - 1].1.to_string()),
				target_language: dub_languages::LANGUAGES[target].1.to_string(),
			});
			dialog.end_modal(ID_OK);
		});
	}
	cancel.on_click(move |_| dialog.end_modal(ID_CANCEL));
	browse.set_focus();
	dialog.show_modal();
	dialog.destroy();

	result.borrow_mut().take()
}

enum ProjectEvent {
	Loaded(ApiResult<(DubProject, Vec<DubLanguage>)>),
	Progress { job: u64, message: String },
	Added { job: u64, result: ApiResult<DubLanguage> },
	Audio { language_id: String, result: ApiResult<Bytes> },
	Deleted(ApiResult<()>),
	Player(PlayerEvent),
}

struct ProjectState {
	services: Rc<Services>,
	api: ApiClient,
	dialog: Dialog,
	status: Status,
	summary: TextCtrl,
	list: ListBox,
	target: Choice,
	add: Button,
	play: Button,
	languages: RefCell<Vec<DubLanguage>>,
	cache: RefCell<HashMap<String, Bytes>>,
	/// The download the user is waiting on (language id, save instead of play). Later requests replace it.
	awaiting: RefCell<Option<(String, bool)>>,
	project: RefCell<DubProject>,
	adding: Job,
	playing: Cell<bool>,
	deleted: Cell<bool>,
	sender: Sender<ProjectEvent>,
}

fn language_row(language: &DubLanguage) -> String {
	format!("{}, {}", dub_languages::name_for(&language.target_language), language.status)
}

/// Modal project window. Returns true if the project was deleted from inside it.
fn project_dialog(services: &Rc<Services>, api: ApiClient, project: DubProject) -> bool {
	let dialog = Dialog::builder(&services.frame, &format!("Dubbing project: {}", project.name)).build();
	let announcer = Announcer::new(&dialog);
	let form = Form::new(&dialog);
	let summary = form.read_only_line("Project details", "Loading…");
	let (list, play, save) = form.group("Languages", true, |group| {
		let list = group.list("&Languages");
		let row = widgets::button_row();
		let play = group.button(row, "&Play Selected");
		let save = group.button(row, "Sa&ve Selected…");
		group.add_row(row);
		(list, play, save)
	});
	let (target, add) = form.group("Add a language", false, |group| {
		let target = group.choice("Add &target language", &dub_languages::names());
		let row = widgets::button_row();
		let add = group.button(row, "&Add Language");
		group.add_row(row);
		(target, add)
	});
	let status = form.status(announcer);
	let row = widgets::button_row();
	let refresh = form.button(row, "&Refresh");
	let delete = form.button(row, "&Delete Project");
	let close = form.button(row, "Close");
	form.add_row(row);
	dialog.set_sizer_and_fit(form.sizer, true);
	dialog.set_escape_id(close.get_id());
	dialog.set_size(Size::new(620, 520));

	let (state, _inbox) = widgets::with_inbox(
		|sender| {
			Rc::new(ProjectState {
				services: services.clone(),
				api,
				dialog,
				status,
				summary,
				list,
				target,
				add,
				play,
				languages: RefCell::new(Vec::new()),
				cache: RefCell::new(HashMap::new()),
				awaiting: RefCell::new(None),
				project: RefCell::new(project),
				adding: Job::default(),
				playing: Cell::new(false),
				deleted: Cell::new(false),
				sender,
			})
		},
		ProjectState::handle,
	);
	state.add.on_click(widgets::handler(&state, ProjectState::add_or_cancel));
	state.play.on_click(widgets::handler(&state, ProjectState::play_selected));
	save.on_click(widgets::handler(&state, ProjectState::save_selected));
	refresh.on_click(widgets::handler(&state, ProjectState::load));
	delete.on_click(widgets::handler(&state, ProjectState::delete_project));
	close.on_click(widgets::handler(&state, ProjectState::request_close));
	state.load();
	state.list.set_focus();
	dialog.show_modal();
	state.services.player.stop();
	state.adding.cancel();
	dialog.destroy();
	state.deleted.get()
}

impl ProjectState {
	fn load(&self) {
		let api = self.api.clone();
		let id = self.project.borrow().id.clone();
		self.status.set("Loading project…");
		runtime::spawn_and_send(self.sender.clone(), async move {
			let result = async { Ok((api.dub_project(&id).await?, api.list_dub_languages(&id).await?)) }.await;
			ProjectEvent::Loaded(result)
		});
	}

	fn selected(&self) -> Option<DubLanguage> {
		widgets::list_item(self.list, &self.languages)
	}

	/// Esc and Close both land here; closing mid-dub would silently abandon the language being added.
	fn request_close(&self) {
		if self.adding.is_running()
			&& !widgets::confirm(
				&self.dialog,
				"Dubbing in progress",
				"A language is still being added. Stop waiting for it and close?",
			) {
			self.add.set_focus();
			return;
		}
		self.dialog.end_modal(ID_CLOSE);
	}

	fn add_or_cancel(&self) {
		if self.adding.cancel() {
			self.add.set_label("&Add Language");
			self.status.say("Cancelled.");
			return;
		}
		let Some(index) = widgets::choice_selection(self.target) else {
			self.status.say_at(&self.target, "Choose a target language first.");
			return;
		};
		let code = dub_languages::LANGUAGES[index].1.to_string();
		let (api, id, sender) = (self.api.clone(), self.project.borrow().id.clone(), self.sender.clone());
		let job = self.adding.begin();
		self.add.set_label("Cancel &adding");
		let handle = runtime::spawn(async move {
			let progress = sender.clone();
			let result = dub_language(&api, &id, &code, &move |message| {
				let _ = progress.send(ProjectEvent::Progress { job, message });
			})
			.await;
			let _ = sender.send(ProjectEvent::Added { job, result });
		});
		self.adding.attach(handle);
	}

	fn fetch_audio(&self, save: bool) {
		let Some(language) = self.selected() else {
			self.status.say("Select a language first.");
			return;
		};
		if language.status != LANGUAGE_COMPLETE && language.status != "stale" {
			self.status.say("That language is not finished yet.");
			return;
		}
		if let Some(bytes) = self.cache.borrow().get(&language.id).cloned() {
			self.use_audio(&language.id, &bytes, save);
			return;
		}
		if self.awaiting.borrow().as_ref() == Some(&(language.id.clone(), save)) {
			return;
		}
		*self.awaiting.borrow_mut() = Some((language.id.clone(), save));
		self.status.set("Downloading the dubbed audio…");
		let (api, project_id) = (self.api.clone(), self.project.borrow().id.clone());
		runtime::spawn_and_send(self.sender.clone(), async move {
			let result = async {
				let fresh = api.dub_language(&project_id, &language.id).await?;
				api.download_public(&fresh.output_url).await
			}
			.await;
			ProjectEvent::Audio { language_id: language.id, result }
		});
	}

	/// Clears and returns the wanted action if the user is still waiting on this language.
	fn take_awaiting(&self, language_id: &str) -> Option<bool> {
		let mut awaiting = self.awaiting.borrow_mut();
		match awaiting.as_ref() {
			Some((wanted, save)) if wanted == language_id => {
				let save = *save;
				*awaiting = None;
				Some(save)
			}
			_ => None,
		}
	}

	fn play_selected(&self) {
		if self.playing.get() {
			self.stop_playback();
		} else {
			self.fetch_audio(false);
		}
	}

	fn save_selected(&self) {
		self.fetch_audio(true);
	}

	/// The language is looked up by ID because the selection may have moved while the audio downloaded.
	fn use_audio(&self, language_id: &str, bytes: &Bytes, save: bool) {
		if save {
			let language = self.languages.borrow().iter().find(|language| language.id == language_id).cloned();
			let name = language.map(|language| dub_languages::name_for(&language.target_language)).unwrap_or_default();
			let stem = widgets::safe_file_name(&format!("{} {}", self.project.borrow().name, name));
			let saved = widgets::save_bytes(
				&self.dialog,
				"Save dubbed audio",
				&format!("{stem}.flac"),
				"FLAC audio (*.flac)|*.flac|All files|*.*",
				bytes,
			);
			if let Some(message) = saved {
				self.status.say(&message);
			}
			return;
		}
		let sender = self.sender.clone();
		self.services.player.play_clip(
			Playable::Encoded(bytes.to_vec()),
			Box::new(move |event| {
				let _ = sender.send(ProjectEvent::Player(event));
			}),
		);
	}

	fn stop_playback(&self) {
		if self.playing.get() {
			self.services.player.stop();
			self.status.set("Playback stopped.");
		}
	}

	fn delete_project(&self) {
		let name = self.project.borrow().name.clone();
		if !widgets::confirm(
			&self.dialog,
			"Delete dubbing project",
			&format!("Delete the dubbing project \"{name}\" and all of its languages?"),
		) {
			return;
		}
		let (api, id) = (self.api.clone(), self.project.borrow().id.clone());
		self.status.set("Deleting the project…");
		runtime::spawn_and_send(self.sender.clone(), async move {
			ProjectEvent::Deleted(api.delete_dub_project(&id).await)
		});
	}

	fn handle(&self, event: ProjectEvent) {
		match event {
			ProjectEvent::Loaded(Ok((project, languages))) => {
				self.summary.set_value(&format!(
					"{}. Status: {}. Source: {}. Created: {}.",
					project.name,
					project.status,
					dub_languages::name_for(&project.source_language),
					widgets::format_api_date(&project.created_at)
				));
				let selected = widgets::list_selection(self.list).unwrap_or(0);
				widgets::fill_list(self.list, &languages.iter().map(language_row).collect::<Vec<_>>());
				widgets::select_row_within(self.list, selected, languages.len());
				self.status.set(&format!("{} languages.", languages.len()));
				*self.languages.borrow_mut() = languages;
				*self.project.borrow_mut() = project;
			}
			ProjectEvent::Loaded(Err(error)) | ProjectEvent::Deleted(Err(error)) => self.status.say(&error.message),
			ProjectEvent::Progress { job, message } => {
				if self.adding.is_current(job) {
					self.status.set(&message);
				}
			}
			ProjectEvent::Added { job, result } => {
				if !self.adding.finish(job) {
					return;
				}
				self.add.set_label("&Add Language");
				match result {
					Ok(_) => {
						self.status.say("Language added.");
						self.load();
					}
					Err(error) => self.status.say(&error.message),
				}
			}
			ProjectEvent::Audio { language_id, result } => {
				let save = self.take_awaiting(&language_id);
				match result {
					Ok(bytes) => {
						if let Some(save) = save {
							self.use_audio(&language_id, &bytes, save);
						}
						self.cache.borrow_mut().insert(language_id, bytes);
					}
					Err(error) => {
						if save.is_some() {
							self.status.say(&error.message);
						}
					}
				}
			}
			ProjectEvent::Deleted(Ok(())) => {
				self.deleted.set(true);
				self.dialog.end_modal(ID_OK);
			}
			ProjectEvent::Player(event) => match event {
				PlayerEvent::Started => {
					self.playing.set(true);
					self.play.set_label("Stop &Playing");
					self.status.set("Playing.");
				}
				PlayerEvent::Finished => {
					self.playing.set(false);
					self.play.set_label("&Play Selected");
					self.status.set("Playback finished.");
				}
				PlayerEvent::Stopped => {
					self.playing.set(false);
					self.play.set_label("&Play Selected");
				}
				PlayerEvent::Error(message) => {
					self.playing.set(false);
					self.play.set_label("&Play Selected");
					self.status.say(&message);
				}
				PlayerEvent::Paused | PlayerEvent::Resumed => {}
			},
		}
	}
}
