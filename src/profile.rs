use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::blocked::{self, BlockAction};
use crate::chat_management::{self, EditFuture, MemberChangeFuture, MemberFuture, MembersContext};
use crate::model::ChatAddresses;
use crate::model::{
    Avatar, ChatInfo, ChatKind, MemberChange, MemberFilter, MemberModeration, PeerProfile,
};

type ProfileFuture = Pin<Box<dyn Future<Output = Result<PeerProfile>> + Send>>;
pub(crate) type AvatarLoader = Box<dyn Fn(&adw::Avatar, Avatar)>;

pub struct ProfileContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn() -> ProfileFuture>,
    pub avatar: AvatarLoader,
    pub set_blocked: BlockAction,
    pub edit: Box<dyn Fn()>,
    pub edit_chat: Box<dyn Fn(ChatInfo, ChatInfo) -> EditFuture>,
    pub edit_addresses:
        Box<dyn Fn(ChatAddresses, ChatAddresses) -> crate::chat_addresses::EditAddressesFuture>,
    pub chat_saved: Box<dyn Fn(ChatInfo)>,
    pub members: Box<dyn Fn(MemberFilter, i32) -> MemberFuture>,
    pub moderate: Box<dyn Fn(i64, MemberChange) -> MemberChangeFuture>,
    pub invite_members: crate::invite_members::OpenChatInvite,
    pub invites: Box<dyn Fn(&gtk::Button)>,
    pub join_requests: Box<dyn Fn(&gtk::Button)>,
    pub admin_log: Box<dyn Fn(&gtk::Button)>,
    pub command: Box<dyn Fn(String)>,
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
    selected_command: RefCell<Option<String>>,
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
        selected_command: RefCell::new(None),
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
        if let Some(command) = state.selected_command.take() {
            (state.context.command)(command);
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
        while let Some(child) = self.body.first_child() {
            self.body.remove(&child);
        }
        self.body.append(&self.status);
        self.body.append(&self.retry);
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
        self.add_management(&profile);
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

    fn add_management(self: &Rc<Self>, profile: &PeerProfile) {
        if let Some(addresses) = profile.addresses.clone() {
            let button = gtk::Button::with_label("Публичные адреса");
            button.set_halign(gtk::Align::Center);
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |button| {
                    let edit = Rc::downgrade(&state);
                    let refresh = edit.clone();
                    crate::chat_addresses::open(
                        button,
                        state.context.runtime.clone(),
                        addresses.clone(),
                        move |original, proposed| {
                            if let Some(state) = edit.upgrade() {
                                (state.context.edit_addresses)(original, proposed)
                            } else {
                                Box::pin(async {
                                    anyhow::bail!("Профиль чата закрыт")
                                })
                            }
                        },
                        move || {
                            if let Some(state) = refresh.upgrade() {
                                state.load();
                            }
                        },
                    );
                }
            ));
            self.body.append(&button);
        }
        if profile.can_read_admin_log {
            let button = gtk::Button::with_label("Недавние действия");
            button.set_halign(gtk::Align::Center);
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |button| (state.context.admin_log)(button)
            ));
            self.body.append(&button);
        }
        if profile.can_manage_invites {
            let button = gtk::Button::with_label("Пригласительные ссылки");
            button.set_halign(gtk::Align::Center);
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |button| (state.context.invites)(button)
            ));
            self.body.append(&button);
            let requests = gtk::Button::with_label("Заявки на вступление");
            requests.set_halign(gtk::Align::Center);
            requests.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |button| (state.context.join_requests)(button)
            ));
            self.body.append(&requests);
        }
        if profile.can_edit_chat {
            let settings = gtk::Button::with_label("Настройки чата");
            settings.set_halign(gtk::Align::Center);
            let info = profile.chat_info();
            settings.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |button| {
                    let edit = Rc::downgrade(&state);
                    let complete = edit.clone();
                    chat_management::edit(
                        button,
                        state.context.runtime.clone(),
                        info.clone(),
                        move |original, proposed| {
                            if let Some(state) = edit.upgrade() {
                                (state.context.edit_chat)(original, proposed)
                            } else {
                                Box::pin(async {
                                    anyhow::bail!("Профиль чата закрыт")
                                })
                            }
                        },
                        move |saved, attempted| {
                            if let Some(state) = complete.upgrade() {
                                if let Some(info) = saved {
                                    (state.context.chat_saved)(info);
                                }
                                if attempted {
                                    state.load();
                                }
                            }
                        },
                    );
                }
            ));
            self.body.append(&settings);
        }
        if profile.can_view_members
            || profile.admin_policy.is_some()
            || profile.moderation == Some(MemberModeration::Ban)
        {
            let can_view_members = profile.can_view_members;
            let moderation = profile.moderation;
            let admins = profile.admin_policy;
            let can_add_members = profile.can_add_members;
            let subscribers = profile.kind == ChatKind::Broadcast;
            let button = gtk::Button::with_label(match (can_view_members, subscribers) {
                (false, _) if admins.is_some() => "Администраторы",
                (false, _) => "Заблокированные",
                (true, true) => "Список подписчиков",
                (true, false) => "Список участников",
            });
            button.set_halign(gtk::Align::Center);
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |button| {
                    chat_management::members(
                        button,
                        subscribers,
                        can_view_members,
                        moderation,
                        admins,
                        MembersContext {
                            runtime: state.context.runtime.clone(),
                            load: Box::new(glib::clone!(
                                #[weak]
                                state,
                                #[upgrade_or]
                                Box::pin(async {
                                    anyhow::bail!("Профиль чата закрыт")
                                }),
                                move |filter, offset| (state.context.members)(filter, offset)
                            )),
                            avatar: Box::new(glib::clone!(
                                #[weak]
                                state,
                                move |avatar, photo| (state.context.avatar)(avatar, photo)
                            )),
                            moderate: Box::new(glib::clone!(
                                #[weak]
                                state,
                                #[upgrade_or]
                                Box::pin(async {
                                    anyhow::bail!("Профиль чата закрыт")
                                }),
                                move |member, change| (state.context.moderate)(member, change)
                            )),
                            invite: can_add_members.then(|| {
                                let open: crate::invite_members::OpenInvite =
                                    Box::new(glib::clone!(
                                        #[weak]
                                        state,
                                        move |anchor, changed| (state.context.invite_members)(
                                            anchor,
                                            !subscribers,
                                            changed
                                        )
                                    ));
                                open
                            }),
                            refresh_profile: Box::new(glib::clone!(
                                #[weak]
                                state,
                                move || state.load()
                            )),
                        },
                    );
                }
            ));
            self.body.append(&button);
        } else if profile.can_add_members {
            let allow_bots = profile.kind != ChatKind::Broadcast;
            let button = gtk::Button::with_label("Добавить участников");
            button.set_halign(gtk::Align::Center);
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |button| (state.context.invite_members)(
                    button,
                    allow_bots,
                    Box::new(glib::clone!(
                        #[weak]
                        state,
                        move || state.load()
                    ))
                )
            ));
            self.body.append(&button);
        }
        if !profile.bot_commands.is_empty() {
            let commands = adw::PreferencesGroup::builder()
                .title("Команды бота")
                .build();
            for command in &profile.bot_commands {
                let row = adw::ActionRow::builder()
                    .use_markup(false)
                    .title(&command.description)
                    .build();
                let name = command.name.clone();
                let button = gtk::Button::builder()
                    .label(&name)
                    .valign(gtk::Align::Center)
                    .build();
                button.connect_clicked(glib::clone!(
                    #[weak(rename_to = state)]
                    self,
                    move |_| if let Some(dialog) = state.dialog.upgrade() {
                        state.selected_command.replace(Some(name.clone()));
                        dialog.close();
                    }
                ));
                row.add_suffix(&button);
                row.set_activatable_widget(Some(&button));
                commands.add(&row);
            }
            self.body.append(&commands);
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
