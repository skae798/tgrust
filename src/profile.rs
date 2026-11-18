use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::blocked::{self, BlockAction};
use crate::model::{Avatar, ChatKind, PeerProfile};

type ProfileFuture = Pin<Box<dyn Future<Output = Result<PeerProfile>> + Send>>;
pub(crate) type AvatarLoader = Box<dyn Fn(&adw::Avatar, Avatar)>;

pub struct ProfileContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn() -> ProfileFuture>,
    pub avatar: AvatarLoader,
    pub set_blocked: BlockAction,
    pub edit: Box<dyn Fn()>,
}

struct ProfileDialog {
    context: ProfileContext,
    body: gtk::Box,
    status: gtk::Label,
    retry: gtk::Button,
    scroll: gtk::ScrolledWindow,
    task: RefCell<Option<tokio::task::AbortHandle>>,
    closed: Cell<bool>,
    edit_requested: Cell<bool>,
    dialog: glib::WeakRef<adw::Dialog>,
}

pub fn open(window: &adw::ApplicationWindow, context: ProfileContext) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Информация о чате")
        .content_width(440)
        .content_height(520)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let body = gtk::Box::new(gtk::Orientation::Vertical, 16);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.set_margin_top(12);
    body.set_margin_bottom(18);
    let status = gtk::Label::new(Some("Загрузка профиля…"));
    status.set_wrap(true);
    status.add_css_class("dim-label");
    body.append(&status);
    let retry = gtk::Button::with_label("Повторить");
    retry.set_halign(gtk::Align::Center);
    retry.set_visible(false);
    body.append(&retry);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    scroll.set_focusable(true);
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));
    dialog.set_focus(Some(&scroll));
    let state = Rc::new(ProfileDialog {
        context,
        body,
        status,
        retry,
        scroll,
        task: RefCell::new(None),
        closed: Cell::new(false),
        edit_requested: Cell::new(false),
        dialog: dialog.downgrade(),
    });
    state.retry.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.load()
    ));
    state.load();
    dialog.connect_closed(move |_| {
        state.closed.set(true);
        if let Some(task) = state.task.take() {
            task.abort();
        }
        if state.edit_requested.get() {
            (state.context.edit)();
        }
    });
    dialog.present(Some(window));
    dialog
}

impl ProfileDialog {
    fn load(self: &Rc<Self>) {
        if self.closed.get() || self.task.borrow().is_some() {
            return;
        }
        self.status.set_text("Загрузка профиля…");
        self.retry.set_visible(false);
        let task = self.context.runtime.spawn((self.context.load)());
        self.task.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            if let Some(state) = weak.upgrade()
                && !state.closed.get()
            {
                state.task.take();
                match result {
                    Ok(profile) => state.show(profile),
                    Err(error) => {
                        state
                            .status
                            .set_text(&format!("Не удалось загрузить профиль. {error}"));
                        state.retry.set_visible(true);
                    }
                }
            }
        });
    }

    fn show(self: &Rc<Self>, profile: PeerProfile) {
        while let Some(child) = self.body.first_child() {
            self.body.remove(&child);
        }
        let avatar = adw::Avatar::new(88, Some(&profile.title), true);
        avatar.set_halign(gtk::Align::Center);
        if let Some(photo) = profile.avatar {
            (self.context.avatar)(&avatar, photo);
        }
        self.body.append(&avatar);
        let title = text(&profile.title);
        title.add_css_class("title-2");
        title.set_justify(gtk::Justification::Center);
        self.body.append(&title);
        let kind = text(if profile.is_self {
            "Ваш аккаунт"
        } else {
            match profile.kind {
                ChatKind::Contact => "Контакт",
                ChatKind::User => "Пользователь",
                ChatKind::Group => "Группа",
                ChatKind::Broadcast => "Канал",
                ChatKind::Bot => "Бот",
            }
        });
        kind.add_css_class("dim-label");
        self.body.append(&kind);
        for (enabled, value, css) in [
            (profile.verified, "Проверен Telegram", "success"),
            (profile.scam, "Метка Telegram: SCAM", "error"),
            (profile.fake, "Метка Telegram: FAKE", "error"),
        ] {
            if enabled {
                let badge = text(value);
                badge.add_css_class(css);
                self.body.append(&badge);
            }
        }
        if !profile.about.trim().is_empty() {
            let heading = text("Описание");
            heading.add_css_class("heading");
            heading.set_xalign(0.0);
            self.body.append(&heading);
            let about = text(&profile.about);
            about.set_xalign(0.0);
            self.body.append(&about);
        }
        let details = adw::PreferencesGroup::new();
        for username in &profile.usernames {
            details.add(&copy_row("Имя пользователя", username));
        }
        if let Some(phone) = profile.phone.as_deref() {
            let phone = if phone.starts_with('+') {
                phone.to_owned()
            } else {
                format!("+{phone}")
            };
            details.add(&copy_row("Телефон", &phone));
        }
        for (title, value) in [
            (
                if profile.kind == ChatKind::Broadcast {
                    "Подписчики"
                } else {
                    "Участники"
                },
                profile.members,
            ),
            ("В сети", profile.online_members),
        ] {
            if let Some(value) = value {
                let row = adw::ActionRow::builder()
                    .title(title)
                    .subtitle(value.to_string())
                    .build();
                row.set_subtitle_selectable(true);
                details.add(&row);
            }
        }
        self.body.append(&details);
        if profile.is_self {
            let edit = gtk::Button::builder()
                .label("Редактировать профиль")
                .halign(gtk::Align::Center)
                .build();
            edit.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |_| if let Some(dialog) = state.dialog.upgrade() {
                    state.edit_requested.set(true);
                    dialog.close();
                }
            ));
            self.body.append(&edit);
        }
        if let Some(value) = profile.blocked {
            self.add_block_controls(profile.id, profile.title, value);
        }
    }

    fn add_block_controls(self: &Rc<Self>, id: i64, name: String, initial: bool) {
        let status = text("");
        status.set_visible(false);
        let button = gtk::Button::new();
        button.set_halign(gtk::Align::Center);
        let blocked = Rc::new(Cell::new(initial));
        show_block_button(&button, initial);
        button.connect_clicked(glib::clone!(
            #[weak(rename_to = state)]
            self,
            #[weak]
            status,
            move |button| {
                let desired = !blocked.get();
                let weak = Rc::downgrade(&state);
                let button_ref = button.downgrade();
                let status = status.clone();
                let blocked = blocked.clone();
                blocked::confirm(button, &name, desired, move || {
                    let Some(state) = weak.upgrade().filter(|state| !state.closed.get()) else {
                        return;
                    };
                    let Some(button) = button_ref.upgrade() else {
                        return;
                    };
                    state.scroll.grab_focus();
                    button.set_sensitive(false);
                    status.set_text("Сохранение…");
                    status.set_visible(true);
                    let task = blocked::request_change(
                        &state.context.runtime,
                        &state.context.set_blocked,
                        id,
                        desired,
                    );
                    glib::MainContext::default().spawn_local(async move {
                        let result = task
                            .await
                            .map_err(anyhow::Error::from)
                            .and_then(|result| result);
                        if !weak.upgrade().is_some_and(|state| !state.closed.get()) {
                            return;
                        }
                        button.set_sensitive(true);
                        match result {
                            Ok(()) => {
                                blocked.set(desired);
                                show_block_button(&button, desired);
                                status.set_visible(false);
                            }
                            Err(error) => {
                                status.set_text(&format!(
                                    "Не удалось подтвердить изменение. {error}"
                                ));
                            }
                        }
                    });
                });
            }
        ));
        self.body.append(&button);
        self.body.append(&status);
    }
}

fn show_block_button(button: &gtk::Button, blocked: bool) {
    if blocked {
        button.set_label("Разблокировать");
        button.remove_css_class("destructive-action");
    } else {
        button.set_label("Заблокировать");
        button.add_css_class("destructive-action");
    }
}

fn text(value: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(value));
    label.set_use_markup(false);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label
}

fn copy_row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    row.set_use_markup(false);
    row.set_subtitle(value);
    row.set_subtitle_selectable(true);
    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text(format!("Скопировать: {value}"))
        .valign(gtk::Align::Center)
        .build();
    copy.add_css_class("flat");
    let value = value.to_owned();
    copy.connect_clicked(move |button| button.clipboard().set_text(&value));
    row.add_suffix(&copy);
    row
}
