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

use crate::model::{
    ForumCursor, ForumPage, ForumPermissions, ForumTopic, ForumTopicChange, ForumWrite,
    ForumWriteResult,
};

pub type PageFuture = Pin<Box<dyn Future<Output = Result<ForumPage>> + Send>>;
pub type WriteFuture = Pin<Box<dyn Future<Output = Result<ForumWriteResult>> + Send>>;

pub struct Management {
    pub write: Box<dyn Fn(ForumWrite) -> WriteFuture>,
    pub changed: Box<dyn Fn(&ForumWriteResult)>,
}

pub fn open(
    anchor: &impl IsA<gtk::Widget>,
    runtime: Handle,
    load: impl Fn(ForumCursor) -> PageFuture + 'static,
    management: Option<Management>,
    choose: impl Fn(ForumTopic) + 'static,
) -> adw::Dialog {
    let (dialog, body) = crate::chat_management::dialog("Темы");
    let rows = adw::PreferencesGroup::new();
    let status = crate::contact_picker::caption();
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let add = gtk::Button::with_label("Новая тема");
    add.add_css_class("suggested-action");
    add.set_visible(false);
    actions.append(&add);
    let refresh = gtk::Button::with_label("Обновить темы");
    actions.append(&refresh);
    let more = gtk::Button::with_label("Показать ещё");
    more.set_halign(gtk::Align::Center);
    more.set_visible(false);
    body.append(&actions);
    body.append(&status);
    body.append(&rows);
    body.append(&more);
    let state = Rc::new(Picker {
        runtime,
        load: Box::new(load),
        management,
        dialog: dialog.downgrade(),
        status,
        add,
        rows,
        more,
        seen: RefCell::new(HashSet::new()),
        visited: RefCell::new(HashSet::new()),
        rendered: RefCell::new(Vec::new()),
        permissions: Cell::new(ForumPermissions::default()),
        generation: Cell::new(0),
        cursor: Cell::new(Some(ForumCursor::default())),
        reading: Cell::new(false),
        selected: RefCell::new(None),
    });
    state.more.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.next_page()
    ));
    state.add.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |button| state.edit(button, None)
    ));
    refresh.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.reset()
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
    management: Option<Management>,
    dialog: glib::WeakRef<adw::Dialog>,
    status: gtk::Label,
    add: gtk::Button,
    rows: adw::PreferencesGroup,
    more: gtk::Button,
    seen: RefCell<HashSet<i32>>,
    visited: RefCell<HashSet<ForumCursor>>,
    rendered: RefCell<Vec<adw::ActionRow>>,
    permissions: Cell<ForumPermissions>,
    generation: Cell<u64>,
    cursor: Cell<Option<ForumCursor>>,
    reading: Cell<bool>,
    selected: RefCell<Option<ForumTopic>>,
}

impl Picker {
    fn reset(self: &Rc<Self>) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.reading.set(false);
        for row in self.rendered.take() {
            self.rows.remove(&row);
        }
        self.seen.borrow_mut().clear();
        self.visited.borrow_mut().clear();
        self.permissions.set(ForumPermissions::default());
        self.add.set_visible(false);
        self.cursor.set(Some(ForumCursor::default()));
        self.next_page();
    }

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
        let generation = self.generation.get();
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(state) = weak
                .upgrade()
                .filter(|state| state.generation.get() == generation)
            else {
                return;
            };
            state.reading.set(false);
            match result {
                Ok(page) => {
                    if let Some(permissions) = page.permissions {
                        state.permissions.set(permissions);
                        state
                            .add
                            .set_visible(permissions.create && state.management.is_some());
                    }
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
        let permissions = self.permissions.get();
        for topic in topics {
            if topic.hidden && (!permissions.manage || self.management.is_none()) {
                continue;
            }
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
            if self.management.is_some()
                && (permissions.allows(&topic, &ForumTopicChange::Closed(false))
                    || permissions.allows(&topic, &ForumTopicChange::Pinned(false))
                    || permissions.allows(&topic, &ForumTopicChange::Delete))
            {
                let edit = gtk::Button::from_icon_name("document-edit-symbolic");
                edit.set_valign(gtk::Align::Center);
                edit.set_tooltip_text(Some("Параметры темы"));
                edit.update_property(&[gtk::accessible::Property::Label(&format!(
                    "Параметры — {}",
                    topic.title
                ))]);
                edit.connect_clicked({
                    let topic = topic.clone();
                    glib::clone!(
                        #[weak(rename_to = state)]
                        self,
                        move |button| state.edit(button, Some(topic.clone()))
                    )
                });
                row.add_suffix(&edit);
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
            self.rendered.borrow_mut().push(row);
        }
    }

    fn edit(self: &Rc<Self>, anchor: &gtk::Button, topic: Option<ForumTopic>) {
        let write = Rc::downgrade(self);
        let completed = write.clone();
        crate::forum_topic_editor::open(
            anchor,
            self.runtime.clone(),
            self.permissions.get(),
            topic,
            move |request| {
                if let Some(state) = write.upgrade()
                    && let Some(management) = &state.management
                {
                    (management.write)(request)
                } else {
                    Box::pin(async { anyhow::bail!("Список тем закрыт") })
                }
            },
            move |result| {
                let Some(state) = completed.upgrade() else {
                    return;
                };
                if let Some(result) = &result
                    && let Some(management) = &state.management
                {
                    (management.changed)(result);
                }
                if let Some(ForumWriteResult::Created(topic)) = result
                    && let Some(dialog) = state.dialog.upgrade()
                {
                    state.selected.replace(Some(topic));
                    dialog.close();
                } else {
                    state.reset();
                }
            },
        );
    }
}
