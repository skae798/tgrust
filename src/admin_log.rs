use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::{runtime::Handle, task::AbortHandle};

use crate::model::{AdminLogEntry, AdminLogFilter, AdminLogPage, AdminLogQuery};
use crate::profile::AvatarLoader;

type LoadFuture = Pin<Box<dyn Future<Output = Result<AdminLogPage>> + Send>>;

pub struct AdminLogContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn(AdminLogQuery, Option<i64>) -> LoadFuture>,
    pub avatar: AvatarLoader,
}

const FILTERS: [(AdminLogFilter, &str); 8] = {
    use AdminLogFilter::*;
    [
        (All, "Все действия"),
        (Members, "Участники"),
        (Permissions, "Роли и ограничения"),
        (Messages, "Сообщения"),
        (Settings, "Настройки чата"),
        (Invites, "Пригласительные ссылки"),
        (Topics, "Темы"),
        (Calls, "Видеочаты"),
    ]
};

pub fn open(anchor: &gtk::Button, context: AdminLogContext) {
    let (dialog, toolbar, body) = crate::chat_management::dialog_with_toolbar("Недавние действия");
    let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
    header.set_margin_top(6);
    header.set_margin_bottom(6);
    header.set_margin_start(16);
    header.set_margin_end(16);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Поиск по журналу")
        .search_delay(250)
        .build();
    search.update_property(&[gtk::accessible::Property::Label("Поиск по журналу")]);
    header.append(&search);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let filters = gtk::DropDown::from_strings(&FILTERS.map(|(_, name)| name));
    filters.set_hexpand(true);
    filters.update_property(&[gtk::accessible::Property::Label("Тип события")]);
    controls.append(&filters);
    let refresh = icon_button("view-refresh-symbolic", "Обновить журнал");
    controls.append(&refresh);
    header.append(&controls);

    let selected_actor = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let actor_name = text(false);
    actor_name.set_hexpand(true);
    selected_actor.append(&actor_name);
    let clear_actor = icon_button("edit-clear-symbolic", "Показать всех авторов");
    selected_actor.append(&clear_actor);
    selected_actor.set_visible(false);
    header.append(&selected_actor);
    toolbar.add_top_bar(&header);

    let status = text(false);
    status.add_css_class("dim-label");
    body.append(&status);
    let rows = adw::PreferencesGroup::new();
    body.append(&rows);
    let more = gtk::Button::with_label("Показать ещё");
    more.set_halign(gtk::Align::Center);
    more.set_visible(false);
    body.append(&more);

    let state = Rc::new(Log {
        context,
        search,
        filters,
        selected_actor,
        actor_name,
        status,
        more,
        rows,
        widgets: RefCell::new(Vec::new()),
        actor: Cell::new(None),
        search_query: RefCell::new(String::new()),
        next: Cell::new(None),
        pending: RefCell::new(None),
        generation: Cell::new(0),
        closed: Cell::new(false),
    });
    state.search.connect_search_changed(glib::clone!(
        #[weak]
        state,
        move |_| state.refresh()
    ));
    state.filters.connect_selected_notify(glib::clone!(
        #[weak]
        state,
        move |_| state.refresh()
    ));
    refresh.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.refresh()
    ));
    clear_actor.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| {
            state.actor.set(None);
            state.selected_actor.set_visible(false);
            state.refresh();
        }
    ));
    state.more.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.load()
    ));
    dialog.connect_closed({
        let state = state.clone();
        move |_| {
            state.closed.set(true);
            state.cancel();
        }
    });
    state.load();
    dialog.set_focus(Some(&state.search));
    dialog.present(Some(anchor));
}

struct Log {
    context: AdminLogContext,
    search: gtk::SearchEntry,
    filters: gtk::DropDown,
    selected_actor: gtk::Box,
    actor_name: gtk::Label,
    status: gtk::Label,
    more: gtk::Button,
    rows: adw::PreferencesGroup,
    widgets: RefCell<Vec<gtk::Widget>>,
    actor: Cell<Option<i64>>,
    search_query: RefCell<String>,
    next: Cell<Option<i64>>,
    pending: RefCell<Option<AbortHandle>>,
    generation: Cell<u64>,
    closed: Cell<bool>,
}

impl Log {
    fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(task) = self.pending.take() {
            task.abort();
        }
    }

    fn refresh(self: &Rc<Self>) {
        self.cancel();
        self.search_query
            .replace(self.search.text().trim().to_owned());
        self.next.set(None);
        for row in self.widgets.take() {
            self.rows.remove(&row);
        }
        self.status.set_visible(false);
        self.more.set_visible(false);
        self.load();
    }

    fn load(self: &Rc<Self>) {
        if self.closed.get() || self.pending.borrow().is_some() {
            return;
        }
        let query = AdminLogQuery {
            text: self.search_query.borrow().clone(),
            filter: FILTERS
                .get(self.filters.selected() as usize)
                .map_or(AdminLogFilter::All, |(filter, _)| *filter),
            actor: self.actor.get(),
        };
        let search = query.text.clone();
        let request = (self.context.load)(query, self.next.get());
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(15), request)
                .await
                .context("Telegram не прислал журнал вовремя")?
        });
        self.pending.replace(Some(task.abort_handle()));
        self.more.set_visible(false);
        self.set_status("Загрузка журнала…");
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|page| page);
            let Some(state) = weak
                .upgrade()
                .filter(|state| !state.closed.get() && state.generation.get() == generation)
            else {
                return;
            };
            state.pending.take();
            if state.search.text().trim() != search {
                return;
            }
            match result {
                Ok(page) => state.show(page),
                Err(error) => {
                    state.set_status(&format!("Не удалось получить журнал. {error}"));
                    state.more.set_label("Повторить");
                    state.more.set_visible(true);
                }
            }
        });
    }

    fn show(self: &Rc<Self>, page: AdminLogPage) {
        self.next.set(page.next);
        for entry in page.entries {
            let row = self.row(entry);
            self.rows.add(&row);
            self.widgets.borrow_mut().push(row);
        }
        self.set_status(if self.widgets.borrow().is_empty() {
            "Событий не найдено."
        } else {
            ""
        });
        self.more.set_label("Показать ещё");
        self.more.set_visible(page.next.is_some());
    }

    fn set_status(&self, message: &str) {
        self.status.set_text(message);
        self.status.set_visible(!message.is_empty());
    }

    fn row(self: &Rc<Self>, entry: AdminLogEntry) -> gtk::Widget {
        let author = &entry.actor;
        let subtitle = glib::DateTime::from_unix_local(i64::from(entry.date))
            .and_then(|date| date.format("%d.%m.%Y %H:%M"))
            .ok()
            .map_or_else(
                || author.name.clone(),
                |date| format!("{} · {date}", author.name),
            );
        let avatar = adw::Avatar::new(30, Some(&author.name), true);
        if let Some(photo) = author.avatar {
            (self.context.avatar)(&avatar, photo);
        }
        let actor_filter = author
            .filter_id
            .filter(|id| Some(*id) != self.actor.get())
            .map(|id| {
                let title = format!("Действия автора — {}", author.name);
                let button = icon_button("system-users-symbolic", &title);
                let name = author.name.clone();
                button.connect_clicked(glib::clone!(
                    #[weak(rename_to = state)]
                    self,
                    move |_| {
                        state.actor.set(Some(id));
                        state.actor_name.set_text(&format!("Автор · {name}"));
                        state.selected_actor.set_visible(true);
                        state.refresh();
                    }
                ));
                button
            });
        if let Some(details) = entry.details {
            let row = adw::ExpanderRow::builder()
                .use_markup(false)
                .title(&entry.action)
                .subtitle(&subtitle)
                .title_lines(2)
                .subtitle_lines(2)
                .build();
            row.add_prefix(&avatar);
            if let Some(button) = actor_filter {
                row.add_suffix(&button);
            }
            let label = text(true);
            label.set_text(&details);
            label.set_margin_top(12);
            label.set_margin_bottom(12);
            label.set_margin_start(12);
            label.set_margin_end(12);
            row.add_row(&label);
            row.upcast()
        } else {
            let row = adw::ActionRow::builder()
                .use_markup(false)
                .title(&entry.action)
                .subtitle(&subtitle)
                .title_lines(2)
                .subtitle_lines(2)
                .build();
            row.add_prefix(&avatar);
            if let Some(button) = actor_filter {
                row.add_suffix(&button);
            }
            row.upcast()
        }
    }
}

fn icon_button(icon: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_valign(gtk::Align::Center);
    button.set_tooltip_text(Some(label));
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

fn text(selectable: bool) -> gtk::Label {
    gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .selectable(selectable)
        .build()
}
