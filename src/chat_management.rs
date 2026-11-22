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
    ChatInfo, MemberChange, MemberFilter, MemberModeration, MemberPage, MemberRole,
};
use crate::profile::AvatarLoader;

pub type EditFuture = Pin<Box<dyn Future<Output = Result<ChatInfo>> + Send>>;
pub type MemberFuture = Pin<Box<dyn Future<Output = Result<MemberPage>> + Send>>;
pub type MemberChangeFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

fn dialog(title: &str) -> (adw::Dialog, gtk::Box) {
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(440)
        .content_height(480)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.set_margin_top(12);
    body.set_margin_bottom(16);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));
    (dialog, body)
}

fn status_label() -> gtk::Label {
    gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build()
}

pub fn edit(
    anchor: &impl IsA<gtk::Widget>,
    runtime: Handle,
    initial: ChatInfo,
    save: impl Fn(ChatInfo, ChatInfo) -> EditFuture + 'static,
    complete: impl Fn(Option<ChatInfo>, bool) + 'static,
) {
    let (dialog, body) = dialog("Настройки чата");
    let heading = adw::PreferencesGroup::new();
    let title = adw::EntryRow::builder()
        .title("Название")
        .text(&initial.title)
        .build();
    heading.add(&title);
    body.append(&heading);
    let about_label = gtk::Label::builder().label("Описание").xalign(0.0).build();
    body.append(&about_label);
    let about = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    about.update_property(&[gtk::accessible::Property::Label("Описание")]);
    about.buffer().set_text(&initial.about);
    let about_scroll = gtk::ScrolledWindow::builder()
        .child(&about)
        .height_request(105)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .css_classes(["card"])
        .build();
    body.append(&about_scroll);
    let status = status_label();
    body.append(&status);
    let submit = gtk::Button::builder()
        .label("Сохранить")
        .halign(gtk::Align::End)
        .css_classes(["suggested-action"])
        .build();
    body.append(&submit);
    let saved = Rc::new(RefCell::new(None));
    let attempted = Rc::new(Cell::new(false));
    dialog.connect_closed({
        let saved = saved.clone();
        let attempted = attempted.clone();
        move |_| complete(saved.take(), attempted.get())
    });
    submit.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        title,
        #[weak]
        about,
        #[weak]
        status,
        move |submit| {
            let buffer = about.buffer();
            let proposed = ChatInfo {
                title: title.text().into(),
                about: buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .into(),
            };
            match proposed.changes(&initial, &initial) {
                Ok((None, None)) => {
                    status.set_text("Нет изменений.");
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    status.set_text(&error.to_string());
                    return;
                }
            }
            attempted.set(true);
            dialog.set_can_close(false);
            submit.set_sensitive(false);
            title.set_editable(false);
            about.set_editable(false);
            status.set_text("Сохранение…");
            let request = save(initial.clone(), proposed);
            let task = runtime.spawn(async move {
                tokio::time::timeout(Duration::from_secs(25), request)
                    .await
                    .context("Telegram не подтвердил изменения вовремя")?
            });
            let saved = saved.clone();
            let submit = submit.clone();
            glib::spawn_future_local(async move {
                let result = task
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                dialog.set_can_close(true);
                match result {
                    Ok(info) => {
                        saved.replace(Some(info));
                        dialog.close();
                    }
                    Err(error) => {
                        status.set_text(&format!("Не удалось сохранить все изменения. {error}"));
                        title.set_editable(true);
                        about.set_editable(true);
                        submit.set_sensitive(true);
                    }
                }
            });
        }
    ));
    dialog.present(Some(anchor));
}

pub struct MembersContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn(MemberFilter, i32) -> MemberFuture>,
    pub avatar: AvatarLoader,
    pub moderate: Box<dyn Fn(i64, MemberChange) -> MemberChangeFuture>,
    pub refresh_profile: Box<dyn Fn()>,
}

pub fn members(
    anchor: &impl IsA<gtk::Widget>,
    subscribers: bool,
    can_view_members: bool,
    moderation: Option<MemberModeration>,
    context: MembersContext,
) {
    let (dialog, body) = dialog(if subscribers {
        "Подписчики"
    } else {
        "Участники"
    });
    let mut choices = Vec::new();
    if can_view_members {
        choices.extend([
            (MemberFilter::All, "Все"),
            (MemberFilter::Admins, "Администраторы"),
            (MemberFilter::Bots, "Боты"),
        ]);
    }
    if moderation == Some(MemberModeration::Ban) {
        choices.push((MemberFilter::Banned, "Заблокированные"));
    }
    let names: Vec<_> = choices.iter().map(|(_, name)| *name).collect();
    let filters = gtk::DropDown::from_strings(&names);
    filters.update_property(&[gtk::accessible::Property::Label("Фильтр участников")]);
    filters.set_hexpand(true);
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Обновить список"));
    refresh.update_property(&[gtk::accessible::Property::Label("Обновить список")]);
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    controls.append(&filters);
    controls.append(&refresh);
    body.append(&controls);
    let status = status_label();
    body.append(&status);
    let rows = adw::PreferencesGroup::new();
    body.append(&rows);
    let more = gtk::Button::with_label("Показать ещё");
    more.set_halign(gtk::Align::Center);
    body.append(&more);

    let state = Rc::new(Members {
        context,
        moderation,
        choices: choices.into_iter().map(|(filter, _)| filter).collect(),
        filters,
        refresh,
        rows,
        status,
        more,
        rendered: RefCell::new(Vec::new()),
        seen: RefCell::new(HashSet::new()),
        next: Cell::new(Some(0)),
        generation: Cell::new(0),
        task: RefCell::new(None),
        writing: Cell::new(false),
        changed: Cell::new(false),
        closed: Cell::new(false),
    });
    state.filters.connect_selected_notify(glib::clone!(
        #[weak]
        state,
        move |_| state.reset()
    ));
    state.more.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.load()
    ));
    state.refresh.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.reset()
    ));
    dialog.connect_closed({
        let state = state.clone();
        move |_| {
            state.closed.set(true);
            state.cancel();
            if state.changed.replace(false) {
                (state.context.refresh_profile)();
            }
        }
    });
    state.load();
    dialog.present(Some(anchor));
}

struct Members {
    context: MembersContext,
    moderation: Option<MemberModeration>,
    choices: Vec<MemberFilter>,
    filters: gtk::DropDown,
    refresh: gtk::Button,
    rows: adw::PreferencesGroup,
    status: gtk::Label,
    more: gtk::Button,
    rendered: RefCell<Vec<adw::ActionRow>>,
    seen: RefCell<HashSet<i64>>,
    next: Cell<Option<i32>>,
    generation: Cell<u64>,
    task: RefCell<Option<tokio::task::AbortHandle>>,
    writing: Cell<bool>,
    changed: Cell<bool>,
    closed: Cell<bool>,
}

impl Members {
    fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    fn reset(self: &Rc<Self>) {
        if self.writing.get() || self.closed.get() {
            return;
        }
        self.cancel();
        for row in self.rendered.take() {
            self.rows.remove(&row);
        }
        self.seen.borrow_mut().clear();
        self.next.set(Some(0));
        self.load();
    }

    fn load(self: &Rc<Self>) {
        if self.closed.get() || self.writing.get() || self.task.borrow().is_some() {
            return;
        }
        let Some(offset) = self.next.get() else {
            return;
        };
        let Some(&filter) = self.choices.get(self.filters.selected() as usize) else {
            return;
        };
        self.more.set_visible(false);
        self.rows.set_sensitive(false);
        self.set_status("Загрузка участников…");
        let request = (self.context.load)(filter, offset);
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(15), request)
                .await
                .context("Telegram не прислал список вовремя")?
        });
        self.task.replace(Some(task.abort_handle()));
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
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
            state.task.take();
            state.rows.set_sensitive(true);
            match result {
                Ok(page) => state.show(page, offset),
                Err(error) => {
                    state.set_status(&format!("Не удалось получить участников. {error}"));
                    state.more.set_label("Повторить");
                    state.more.set_visible(true);
                }
            }
        });
    }

    fn set_status(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
    }

    fn show(self: &Rc<Self>, page: MemberPage, offset: i32) {
        self.next.set(page.next.filter(|next| *next > offset));
        for member in page.members {
            if !self.seen.borrow_mut().insert(member.id) {
                continue;
            }
            let row = adw::ActionRow::builder()
                .use_markup(false)
                .title(&member.name)
                .build();
            let role = match member.role {
                MemberRole::Owner => "Владелец",
                MemberRole::Administrator => "Администратор",
                MemberRole::Member => "Участник",
                MemberRole::Banned => "Заблокирован",
            };
            row.set_subtitle(&if member.bot {
                format!("{role} · Бот")
            } else {
                role.to_owned()
            });
            let avatar = adw::Avatar::new(30, Some(&member.name), true);
            if let Some(photo) = member.avatar {
                (self.context.avatar)(&avatar, photo);
            }
            row.add_prefix(&avatar);
            if member.can_update
                && let Some(action) = self.moderation.and_then(|mode| mode.action(member.role))
            {
                let button = gtk::Button::builder()
                    .label(change_label(action))
                    .valign(gtk::Align::Center)
                    .build();
                let name = member.name;
                button.update_property(&[gtk::accessible::Property::Label(&format!(
                    "{} — {name}",
                    change_label(action)
                ))]);
                let id = member.id;
                button.connect_clicked(glib::clone!(
                    #[weak(rename_to = state)]
                    self,
                    move |button| state.confirm(button, id, &name, action)
                ));
                row.add_suffix(&button);
            }
            self.rows.add(&row);
            self.rendered.borrow_mut().push(row);
        }
        let empty = self.seen.borrow().is_empty();
        self.set_status(if empty && self.next.get().is_none() {
            "Список пуст."
        } else {
            ""
        });
        self.more.set_label("Показать ещё");
        self.more.set_visible(self.next.get().is_some());
    }

    fn confirm(
        self: &Rc<Self>,
        anchor: &gtk::Button,
        member: i64,
        name: &str,
        change: MemberChange,
    ) {
        let (heading, body) = match change {
            MemberChange::Remove => (
                "Удалить участника?",
                format!("{name} будет удалён из группы. Его сообщения сохранятся."),
            ),
            MemberChange::Ban => (
                "Заблокировать участника?",
                format!("{name} потеряет доступ к этому чату до снятия блокировки."),
            ),
            MemberChange::Unban => (
                "Снять блокировку?",
                format!("{name} сможет снова вступить в чат, если у него есть доступ."),
            ),
        };
        let prompt = adw::AlertDialog::new(Some(heading), Some(&body));
        prompt.set_body_use_markup(false);
        prompt.add_responses(&[("cancel", "Отмена"), ("confirm", change_label(change))]);
        if change != MemberChange::Unban {
            prompt.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
        }
        prompt.set_default_response(Some("cancel"));
        prompt.set_close_response("cancel");
        prompt.connect_response(
            Some("confirm"),
            glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |_, _| state.change(member, change)
            ),
        );
        prompt.present(Some(anchor));
    }

    fn change(self: &Rc<Self>, member: i64, change: MemberChange) {
        if self.closed.get() || self.task.borrow().is_some() || self.writing.replace(true) {
            return;
        }
        self.set_controls_sensitive(false);
        self.set_status("Сохранение…");
        let request = (self.context.moderate)(member, change);
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(25), request)
                .await
                .context("Telegram не подтвердил изменение вовремя")?
        });
        let state = self.clone();
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            state.writing.set(false);
            if state.closed.get() {
                (state.context.refresh_profile)();
                return;
            }
            state.changed.set(true);
            state.set_controls_sensitive(true);
            state.filters.grab_focus();
            match result {
                Ok(()) => state.reset(),
                Err(error) => state.set_status(&format!("Не удалось изменить участника. {error}")),
            }
        });
    }

    fn set_controls_sensitive(&self, sensitive: bool) {
        self.filters.set_sensitive(sensitive);
        self.refresh.set_sensitive(sensitive);
        self.rows.set_sensitive(sensitive);
        self.more.set_sensitive(sensitive);
    }
}

fn change_label(change: MemberChange) -> &'static str {
    match change {
        MemberChange::Remove => "Удалить",
        MemberChange::Ban => "Заблокировать",
        MemberChange::Unban => "Разблокировать",
    }
}
