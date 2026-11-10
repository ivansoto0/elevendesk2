//! Pronunciation Dictionaries dialog: a read-only listing, as in the original app.

use std::rc::Rc;

use wxdragon::prelude::*;

use super::{
	announce::Announcer,
	inbox::Inbox,
	services::Services,
	widgets::{self, Form},
};
use crate::{
	api::{ApiResult, PronunciationDictionary},
	runtime,
};

pub fn show(services: &Rc<Services>) {
	let Some(api) = services.api() else {
		services.announcer.say(super::services::NO_KEY_MESSAGE);
		return;
	};
	let dialog = Dialog::builder(&services.frame, "Pronunciation Dictionaries").build();
	let announcer = Announcer::new(&dialog);
	let form = Form::new(&dialog);
	let list = form.list("&Dictionaries");
	let status = form.status(announcer);
	let row = widgets::button_row();
	let close = form.button(row, "Close");
	form.add_row(row);
	dialog.set_sizer_and_fit(form.sizer, true);
	dialog.set_size(Size::new(640, 420));
	dialog.set_escape_id(close.get_id());
	close.on_click(move |_| dialog.end_modal(ID_CLOSE));

	let inbox = Inbox::new(move |result: ApiResult<Vec<PronunciationDictionary>>| match result {
		Ok(items) => {
			widgets::fill_list(
				list,
				&items
					.iter()
					.map(|item| {
						if item.description.is_empty() {
							item.name.clone()
						} else {
							format!("{}. {}", item.name, item.description)
						}
					})
					.collect::<Vec<_>>(),
			);
			widgets::select_row_within(list, 0, items.len());
			if items.is_empty() {
				status.say("You have no pronunciation dictionaries.");
			} else {
				status.set(&format!("{} pronunciation dictionaries.", items.len()));
			}
		}
		Err(error) => status.say(&error.message),
	});
	status.set("Loading pronunciation dictionaries…");
	runtime::spawn_and_send(inbox.sender(), async move { api.list_pronunciation_dictionaries().await });
	list.set_focus();
	dialog.show_modal();
	dialog.destroy();
}
