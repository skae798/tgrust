use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::{ForumCursor, ForumPage, ForumTopic};

pub type PageFuture = Pin<Box<dyn Future<Output = Result<ForumPage>> + Send>>;

pub fn open(
    anchor: &impl IsA<gtk::Widget>,
    runtime: Handle,
    load: impl Fn(ForumCursor) -> PageFuture + 'static,
    choose: impl Fn(ForumTopic) + 'static,
) -> adw::Dialog {
    let (dialog, body) = crate::chat_management::dialog("Темы");
    let rows = adw::PreferencesGroup::new();
    let status = crate::contact_picker::caption();
    let more = gtk::Button::with_label("Показать ещё");
    more.set_halign(gtk::Align::Center);
    more.set_visible(false);
    body.append(&status);
    body.append(&rows);
    body.append(&more);
    let state = Rc::new(Picker {
        runtime,
        load: Box::new(load),
        dialog: dialog.downgrade(),
        status,
        rows,
        more,
        seen: RefCell::new(HashSet::new()),
        visited: RefCell::new(HashSet::new()),
        cursor: Cell::new(Some(ForumCursor::default())),
        reading: Cell::new(false),
        selected: RefCell::new(None),
    });
    state.more.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.next_page()
    ));
    dialog.connect_closed({
        let state = state.clone();
        move |_| {
            if let Some(topic) = state.selected.take() {
                choose(topic);
            }
        }
    });
    state.next_page();
    dialog.present(Some(anchor));
    dialog
}

struct Picker {
    runtime: Handle,
    load: Box<dyn Fn(ForumCursor) -> PageFuture>,
    dialog: glib::WeakRef<adw::Dialog>,
    status: gtk::Label,
    rows: adw::PreferencesGroup,
    more: gtk::Button,
    seen: RefCell<HashSet<i32>>,
    visited: RefCell<HashSet<ForumCursor>>,
    cursor: Cell<Option<ForumCursor>>,
    reading: Cell<bool>,
    selected: RefCell<Option<ForumTopic>>,
}

impl Picker {
    fn next_page(self: &Rc<Self>) {
        let Some(cursor) = self.cursor.get() else {
            return;
        };
        if self.reading.replace(true) {
            return;
        }
        self.more.set_visible(false);
        self.status.set_text("Загрузка тем…");
        self.status.set_visible(true);
        let request = (self.load)(cursor);
        let task = self.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(20), request)
                .await
                .context("Telegram не прислал список тем вовремя")?
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(state) = weak.upgrade() else {
                return;
            };
            state.reading.set(false);
            match result {
                Ok(page) => {
                    state.append(page.topics);
                    state.visited.borrow_mut().insert(cursor);
                    state.cursor.set(
                        page.next
                            .filter(|next| !state.visited.borrow().contains(next)),
                    );
                    state.status.set_visible(state.seen.borrow().is_empty());
                    state.status.set_text("Тем пока нет");
                    state.more.set_label("Показать ещё");
                    state.more.set_visible(state.cursor.get().is_some());
                }
                Err(error) => {
                    state
                        .status
                        .set_text(&format!("Не удалось загрузить темы. {error}"));
                    state.more.set_label("Повторить");
                    state.more.set_visible(true);
                }
            }
        });
    }

    fn append(self: &Rc<Self>, topics: Vec<ForumTopic>) {
        for topic in topics {
            if !self.seen.borrow_mut().insert(topic.id) {
                continue;
            }
            let row = adw::ActionRow::builder()
                .title(&topic.title)
                .use_markup(false)
                .activatable(true)
                .build();
            let mut details = Vec::new();
            if topic.pinned {
                details.push("Закреплена".into());
            }
            if topic.closed {
                details.push("Закрыта".into());
            }
            if topic.hidden {
                details.push("Скрыта".into());
            }
            if topic.unread > 0 {
                details.push(format!("Непрочитано {}", topic.unread));
            }
            if !details.is_empty() {
                row.set_subtitle(&details.join(" · "));
            }
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            row.connect_activated(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |_| {
                    if let Some(dialog) = state.dialog.upgrade() {
                        state.selected.replace(Some(topic.clone()));
                        dialog.close();
                    }
                }
            ));
            self.rows.add(&row);
        }
    }
}
