use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::demo;
use crate::model::{Chat, Message, Reply, UpdateStatus, Upload, UploadKind, merge_latest_history};
use crate::storage::{PendingSave, attachment_name};
use crate::telegram::{LoginStep, Telegram};

struct Ui {
    runtime: Handle,
    stop: watch::Receiver<bool>,
    window: glib::WeakRef<adw::ApplicationWindow>,
    root: gtk::Stack,
    login_title: gtk::Label,
    login_status: gtk::Label,
    login_entry: gtk::PasswordEntry,
    login_submit: gtk::Button,
    login_reset: gtk::Button,
    connect_retry: gtk::Button,
    split: adw::NavigationSplitView,
    toasts: adw::ToastOverlay,
    list: gtk::ListBox,
    filter: gtk::SearchEntry,
    content: gtk::Stack,
    title: adw::WindowTitle,
    history: gtk::Box,
    scroll: gtk::ScrolledWindow,
    older: gtk::Button,
    input: gtk::Entry,
    send: gtk::Button,
    attach: gtk::MenuButton,
    upload_bar: gtk::Box,
    upload_name: gtk::Label,
    reply_bar: gtk::Box,
    reply_label: gtk::Label,
    client: RefCell<Option<Arc<Telegram>>>,
    login_step: Cell<LoginStep>,
    is_demo: Cell<bool>,
    chats: RefCell<Vec<Chat>>,
    selection: Cell<Option<i64>>,
    generation: Cell<u64>,
    chat_refresh: Cell<bool>,
    chat_refresh_pending: Cell<bool>,
    history_refresh: Cell<bool>,
    latest_history_pending: Cell<Option<bool>>,
    sending: Cell<bool>,
    downloading: Cell<bool>,
    messages: RefCell<BTreeMap<i32, Message>>,
    demo_messages: RefCell<HashMap<i64, Vec<Message>>>,
    demo_next_message: Cell<i32>,
    drafts: RefCell<HashMap<i64, String>>,
    uploads: RefCell<HashMap<i64, Upload>>,
    replies: RefCell<HashMap<i64, Reply>>,
    loaded: Cell<bool>,
    has_older: Cell<bool>,
}

pub fn open(app: &adw::Application, runtime: Handle, stop: watch::Receiver<bool>, demo: bool) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("tgrust")
        .default_width(1050)
        .default_height(720)
        .width_request(360)
        .height_request(480)
        .build();

    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::WidgetExt::display(&window),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let root = gtk::Stack::new();
    root.set_transition_type(gtk::StackTransitionType::Crossfade);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&root));
    window.set_content(Some(&toasts));

    let login = adw::ToolbarView::new();
    login.add_top_bar(&adw::HeaderBar::new());
    let login_box = gtk::Box::new(gtk::Orientation::Vertical, 13);
    login_box.set_halign(gtk::Align::Center);
    login_box.set_valign(gtk::Align::Center);
    login_box.set_width_request(300);
    let login_title = label("Вход в Telegram", "title-1");
    let login_status = label("", "dim-label");
    login_status.set_wrap(true);
    login_status.set_max_width_chars(36);
    let login_entry = gtk::PasswordEntry::builder().show_peek_icon(true).build();
    let login_submit = gtk::Button::with_label("Продолжить");
    login_submit.add_css_class("suggested-action");
    let login_reset = gtk::Button::with_label("Запросить новый код");
    login_reset.add_css_class("flat");
    login_reset.set_visible(false);
    let connect_retry = gtk::Button::with_label("Повторить подключение");
    connect_retry.add_css_class("suggested-action");
    connect_retry.set_visible(false);
    let demo_button = gtk::Button::with_label("Открыть деморежим");
    demo_button.add_css_class("flat");
    for widget in [
        login_title.upcast_ref::<gtk::Widget>(),
        login_status.upcast_ref(),
        login_entry.upcast_ref(),
        login_submit.upcast_ref(),
        login_reset.upcast_ref(),
        connect_retry.upcast_ref(),
        demo_button.upcast_ref(),
    ] {
        login_box.append(widget);
    }
    login.set_content(Some(&login_box));
    root.add_named(&login, Some("login"));

    let split = adw::NavigationSplitView::new();
    split.set_min_sidebar_width(260.0);
    split.set_max_sidebar_width(350.0);
    split.set_sidebar_width_fraction(0.32);
    let breakpoint =
        adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 680sp").unwrap());
    breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(breakpoint);
    root.add_named(&split, Some("chats"));

    let sidebar = adw::ToolbarView::new();
    let side_header = adw::HeaderBar::new();
    side_header.set_title_widget(Some(&adw::WindowTitle::new("tgrust", "")));
    let refresh = icon_button("view-refresh-symbolic", "Обновить чаты");
    side_header.pack_end(&refresh);
    sidebar.add_top_bar(&side_header);
    let side_body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let filter = gtk::SearchEntry::builder()
        .placeholder_text("Найти чат")
        .margin_top(8)
        .margin_start(10)
        .margin_end(10)
        .build();
    side_body.append(&filter);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("navigation-sidebar");
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    side_body.append(&list_scroll);
    sidebar.set_content(Some(&side_body));
    split.set_sidebar(Some(&adw::NavigationPage::new(&sidebar, "Чаты")));

    let pane = adw::ToolbarView::new();
    let pane_header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Чаты", "");
    pane_header.set_title_widget(Some(&title));
    pane.add_top_bar(&pane_header);
    let content = gtk::Stack::new();
    let empty = adw::StatusPage::builder()
        .icon_name("mail-unread-symbolic")
        .title("Выберите чат")
        .build();
    content.add_named(&empty, Some("empty"));
    let conversation = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let history = gtk::Box::new(gtk::Orientation::Vertical, 9);
    history.set_valign(gtk::Align::End);
    history.add_css_class("conversation");
    let older = gtk::Button::with_label("Загрузить более ранние сообщения");
    older.set_halign(gtk::Align::Center);
    older.add_css_class("flat");
    let scroll = gtk::ScrolledWindow::builder()
        .child(&history)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    conversation.append(&scroll);

    let composer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    composer.add_css_class("composer");
    let attach = gtk::MenuButton::builder()
        .icon_name("mail-attachment-symbolic")
        .tooltip_text("Добавить вложение")
        .build();
    let attach_menu = gtk::gio::Menu::new();
    attach_menu.append(Some("Файл"), Some("win.attach-file"));
    attach_menu.append(Some("Фотография"), Some("win.attach-photo"));
    attach.set_menu_model(Some(&attach_menu));
    let upload_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    upload_bar.add_css_class("composer");
    upload_bar.set_visible(false);
    let upload_name = label("", "");
    upload_name.set_hexpand(true);
    upload_name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let remove_upload = icon_button("edit-clear-symbolic", "Убрать вложение");
    upload_bar.append(&upload_name);
    upload_bar.append(&remove_upload);
    let reply_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    reply_bar.add_css_class("composer");
    reply_bar.set_visible(false);
    let reply_label = label("", "dim-label");
    reply_label.set_hexpand(true);
    reply_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let remove_reply = icon_button("edit-clear-symbolic", "Отменить ответ");
    reply_bar.append(&reply_label);
    reply_bar.append(&remove_reply);
    let input = gtk::Entry::builder()
        .hexpand(true)
        .placeholder_text("Сообщение")
        .max_length(4096)
        .build();
    let send = icon_button("mail-send-symbolic", "Отправить");
    send.add_css_class("suggested-action");
    composer.append(&attach);
    composer.append(&input);
    composer.append(&send);
    conversation.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    conversation.append(&reply_bar);
    conversation.append(&upload_bar);
    conversation.append(&composer);
    content.add_named(&conversation, Some("conversation"));
    pane.set_content(Some(&content));
    split.set_content(Some(&adw::NavigationPage::new(&pane, "Чат")));

    let ui = Rc::new(Ui {
        runtime,
        stop,
        window: window.downgrade(),
        root,
        login_title,
        login_status,
        login_entry,
        login_submit,
        login_reset,
        connect_retry,
        split,
        toasts,
        list,
        filter,
        content,
        title,
        history,
        scroll,
        older,
        input,
        send,
        attach,
        upload_bar,
        upload_name,
        reply_bar,
        reply_label,
        client: RefCell::new(None),
        login_step: Cell::new(LoginStep::Phone),
        is_demo: Cell::new(false),
        chats: RefCell::new(Vec::new()),
        selection: Cell::new(None),
        generation: Cell::new(0),
        chat_refresh: Cell::new(false),
        chat_refresh_pending: Cell::new(false),
        history_refresh: Cell::new(false),
        latest_history_pending: Cell::new(None),
        sending: Cell::new(false),
        downloading: Cell::new(false),
        messages: RefCell::new(BTreeMap::new()),
        demo_messages: RefCell::new(HashMap::new()),
        demo_next_message: Cell::new(100),
        drafts: RefCell::new(HashMap::new()),
        uploads: RefCell::new(HashMap::new()),
        replies: RefCell::new(HashMap::new()),
        loaded: Cell::new(false),
        has_older: Cell::new(false),
    });
    ui.login_submit.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.submit_login()
    ));
    ui.login_entry.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_| ui.submit_login()
    ));
    ui.login_reset.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.restart_login()
    ));
    ui.connect_retry.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.connect()
    ));
    demo_button.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.start_demo()
    ));
    refresh.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| {
            ui.refresh_chats();
            ui.load_history(None, false);
        }
    ));
    ui.filter.connect_search_changed(glib::clone!(
        #[weak]
        ui,
        move |_| ui.render_chats()
    ));
    ui.list.connect_row_activated(glib::clone!(
        #[weak]
        ui,
        move |_, row| {
            if let Ok(id) = row.widget_name().parse() {
                ui.select_chat(id);
            }
        }
    ));
    ui.older.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| {
            let before = ui.messages.borrow().first_key_value().map(|(&id, _)| id);
            if before.is_some() {
                ui.load_history(before, false);
            }
        }
    ));
    ui.send.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.send_message()
    ));
    ui.input.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_| ui.send_message()
    ));
    ui.input.connect_changed(glib::clone!(
        #[weak]
        ui,
        move |_| ui.update_send_button()
    ));
    for (name, kind) in [
        ("attach-file", UploadKind::File),
        ("attach-photo", UploadKind::Photo),
    ] {
        let action = gtk::gio::SimpleAction::new(name, None);
        action.connect_activate(glib::clone!(
            #[weak]
            ui,
            move |_, _| ui.choose_upload(kind)
        ));
        window.add_action(&action);
    }
    remove_upload.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| {
            if let Some(chat) = ui.selection.get() {
                ui.uploads.borrow_mut().remove(&chat);
                ui.update_send_button();
            }
        }
    ));
    remove_reply.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| {
            if let Some(chat) = ui.selection.get() {
                ui.replies.borrow_mut().remove(&chat);
                ui.update_send_button();
            }
        }
    ));

    window.connect_close_request({
        let ui = Rc::clone(&ui);
        move |_| {
            ui.window.set(None);
            glib::Propagation::Proceed
        }
    });
    window.present();
    if demo {
        ui.start_demo();
    } else {
        ui.connect();
    }
}

impl Ui {
    fn job<T, F, C>(self: &Rc<Self>, future: F, complete: C)
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
        C: FnOnce(&Rc<Self>, Result<T>) + 'static,
    {
        let task = self.runtime.spawn(future);
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            if let Some(ui) = weak.upgrade()
                && ui.window.upgrade().is_some()
            {
                complete(&ui, result);
            }
        });
    }

    fn connect(self: &Rc<Self>) {
        self.root.set_visible_child_name("login");
        self.set_login_busy(true);
        self.connect_retry.set_visible(false);
        self.login_entry.set_visible(false);
        self.login_submit.set_visible(false);
        match crate::credentials() {
            Ok(Some((id, hash))) => {
                self.login_status.set_text("Подключение…");
                self.job(Telegram::connect(id, hash), |ui, result| {
                    if ui.is_demo.get() {
                        return;
                    }
                    match result {
                        Ok((client, step)) => {
                            ui.client.replace(Some(client));
                            ui.show_step(step);
                        }
                        Err(error) => {
                            ui.login_status
                                .set_text(&format!("Не удалось подключиться. {error}"));
                            ui.connect_retry.set_visible(true);
                            ui.connect_retry.grab_focus();
                        }
                    }
                });
            }
            Ok(None) => {
                self.login_status.set_text("Для входа укажите TGRUST_API_ID и TGRUST_API_HASH при запуске. Инструкция есть в README.");
            }
            Err(error) => self.login_status.set_text(&error.to_string()),
        }
    }

    fn set_login_busy(&self, busy: bool) {
        self.login_entry.set_sensitive(!busy);
        self.login_submit.set_sensitive(!busy);
        self.login_reset.set_sensitive(!busy);
    }

    fn show_step(self: &Rc<Self>, step: LoginStep) {
        self.login_step.set(step);
        self.login_entry.set_visible(true);
        self.login_submit.set_visible(true);
        self.login_entry.set_text("");
        self.set_login_busy(false);
        self.login_reset
            .set_visible(matches!(step, LoginStep::Code | LoginStep::Password));
        let (title, status, placeholder) = match step {
            LoginStep::Phone => (
                "Вход в Telegram",
                "Введите номер телефона своего аккаунта.",
                "+380…",
            ),
            LoginStep::Code => (
                "Код подтверждения",
                "Введите код, который прислал Telegram.",
                "Код",
            ),
            LoginStep::Password => (
                "Облачный пароль",
                "Введите пароль двухэтапной аутентификации.",
                "Пароль",
            ),
            LoginStep::Authorized => {
                self.start_client();
                return;
            }
        };
        self.login_title.set_text(title);
        self.login_status.set_text(status);
        self.login_entry.set_placeholder_text(Some(placeholder));
        self.login_entry.grab_focus();
    }

    fn submit_login(self: &Rc<Self>) {
        if !self.login_submit.is_sensitive() {
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let value = self.login_entry.text().to_string();
        let value = if self.login_step.get() == LoginStep::Password {
            value
        } else {
            value.trim().to_owned()
        };
        if value.is_empty() {
            return;
        }
        self.set_login_busy(true);
        self.login_status.set_text("Проверка…");
        self.job(
            async move {
                let result = client.login(&value).await;
                Ok((result, client.login_step().await))
            },
            |ui, result| {
                if ui.is_demo.get() {
                    return;
                }
                match result {
                    Ok((Ok(step), _)) => ui.show_step(step),
                    Ok((Err(error), step)) => {
                        ui.show_step(step);
                        ui.login_status.set_text(&error.to_string());
                    }
                    Err(error) => {
                        ui.login_entry.set_text("");
                        ui.login_status.set_text(&error.to_string());
                        ui.set_login_busy(false);
                    }
                }
            },
        );
    }

    fn restart_login(self: &Rc<Self>) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        self.set_login_busy(true);
        self.job(
            async move {
                client.restart_login().await;
                Ok(())
            },
            |ui, _| {
                if !ui.is_demo.get() {
                    ui.show_step(LoginStep::Phone);
                }
            },
        );
    }

    fn start_demo(self: &Rc<Self>) {
        self.is_demo.set(true);
        self.client.replace(None);
        self.root.set_visible_child_name("chats");
        self.chats.replace(demo::chats());
        self.render_chats();
        if !self.split.is_collapsed() {
            self.select_chat(1);
        }
    }

    fn start_client(self: &Rc<Self>) {
        self.root.set_visible_child_name("chats");
        self.refresh_chats();
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let mut changes = client.changes();
        let stop = self.stop.clone();
        self.job(client.listen(stop), |ui, result| {
            if result.is_err() {
                ui.toast(
                    "Обновления Telegram остановлены. Перезапустите клиент, чтобы проверить сессию.",
                );
            }
        });

        let (notify, mut notifications) = tokio::sync::mpsc::channel(1);
        let mut stop = self.stop.clone();
        self.runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    change = changes.changed() => {
                        if change.is_err() { break; }
                        tokio::time::sleep(Duration::from_millis(700)).await;
                        let status = *changes.borrow_and_update();
                        if notify.send(status).await.is_err() { break; }
                    }
                }
            }
        });

        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let mut previous = UpdateStatus::default();
            while let Some(status) = notifications.recv().await {
                let Some(ui) = weak.upgrade() else { break };
                if status.reconnecting && !previous.reconnecting {
                    ui.toast("Восстанавливаем связь с Telegram…");
                }
                if status.revision != previous.revision {
                    ui.refresh_chats();
                    if !ui.split.is_collapsed() || ui.split.shows_content() {
                        ui.load_history(None, true);
                    }
                }
                previous = status;
            }
        });
    }

    fn refresh_chats(self: &Rc<Self>) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        if self.chat_refresh.replace(true) {
            self.chat_refresh_pending.set(true);
            return;
        }
        self.job(async move { client.chats().await }, move |ui, result| {
            ui.chat_refresh.set(false);
            match result {
                Ok(chats) => {
                    ui.chats.replace(chats);
                    ui.render_chats();
                }
                Err(error) => ui.toast(&format!("Не удалось загрузить чаты. {error}")),
            }
            if ui.chat_refresh_pending.replace(false) {
                ui.refresh_chats();
            }
        });
    }

    fn render_chats(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let query = self.filter.text().trim().to_lowercase();
        for chat in self
            .chats
            .borrow()
            .iter()
            .filter(|chat| chat.title.to_lowercase().contains(&query))
        {
            let row = gtk::ListBoxRow::new();
            row.set_widget_name(&chat.id.to_string());
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            line.add_css_class("chat-row");
            line.append(&adw::Avatar::new(38, Some(&chat.title), true));
            let description = gtk::Box::new(gtk::Orientation::Vertical, 3);
            description.set_hexpand(true);
            description.set_valign(gtk::Align::Center);
            let title = label(&chat.title, "heading");
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let preview = label(&chat.preview, "chat-preview");
            preview.add_css_class("dim-label");
            preview.set_single_line_mode(true);
            preview.set_ellipsize(gtk::pango::EllipsizeMode::End);
            description.append(&title);
            description.append(&preview);
            line.append(&description);
            if chat.unread > 0 {
                let unread = label(&chat.unread.to_string(), "unread-count");
                unread.set_valign(gtk::Align::Center);
                line.append(&unread);
            }
            row.set_child(Some(&line));
            self.list.append(&row);
            if self.selection.get() == Some(chat.id) {
                self.list.select_row(Some(&row));
            }
        }
    }

    fn select_chat(self: &Rc<Self>, id: i64) {
        let Some(chat) = self
            .chats
            .borrow()
            .iter()
            .find(|chat| chat.id == id)
            .cloned()
        else {
            return;
        };
        self.split.set_show_content(true);
        if self.selection.get() == Some(id) && self.loaded.get() {
            self.load_history(None, true);
            return;
        }
        if let Some(previous) = self.selection.replace(Some(id)) {
            self.drafts
                .borrow_mut()
                .insert(previous, self.input.text().to_string());
        }
        self.generation.set(self.generation.get().wrapping_add(1));
        self.history_refresh.set(false);
        self.latest_history_pending.set(None);
        self.loaded.set(false);
        self.has_older.set(false);
        self.messages.borrow_mut().clear();
        self.title.set_title(&chat.title);
        self.title
            .set_subtitle(if chat.broadcast { "Канал" } else { "" });
        self.content.set_visible_child_name("conversation");
        self.input
            .set_text(self.drafts.borrow().get(&id).map_or("", String::as_str));
        self.input.set_sensitive(!chat.broadcast);
        self.input.set_placeholder_text(Some(if chat.broadcast {
            "Канал только для чтения"
        } else {
            "Сообщение"
        }));
        self.update_send_button();
        self.render_history(true);
        if self.is_demo.get() {
            let mut history = self.demo_messages.borrow_mut();
            let messages = history.entry(id).or_insert_with(|| demo::history(id));
            self.messages.borrow_mut().extend(
                messages
                    .iter()
                    .cloned()
                    .map(|message| (message.id, message)),
            );
            if let Some(chat) = self
                .chats
                .borrow_mut()
                .iter_mut()
                .find(|chat| chat.id == id)
            {
                chat.unread = 0;
            }
            self.loaded.set(true);
            self.render_chats();
            self.render_history(true);
        } else {
            self.load_history(None, true);
        }
    }

    fn load_history(self: &Rc<Self>, before: Option<i32>, mark_read: bool) {
        let Some(id) = self.selection.get() else {
            return;
        };
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        if self.history_refresh.replace(true) {
            if before.is_none() {
                self.latest_history_pending.set(Some(
                    mark_read || self.latest_history_pending.get() == Some(true),
                ));
            }
            return;
        }
        let generation = self.generation.get();
        let requested_through = self.messages.borrow().last_key_value().map(|(&id, _)| id);
        self.older.set_sensitive(false);
        let has_unread = self
            .chats
            .borrow()
            .iter()
            .any(|chat| chat.id == id && chat.unread > 0);
        self.job(
            async move { client.history(id, before, mark_read).await },
            move |ui, result| {
                if generation != ui.generation.get() {
                    return;
                }
                ui.history_refresh.set(false);
                ui.older.set_sensitive(true);
                match result {
                    Ok((messages, has_older, marked)) => {
                        let was_loaded = ui.loaded.replace(true);
                        let adjustment = ui.scroll.vadjustment();
                        let at_bottom =
                            adjustment.upper() - adjustment.page_size() - adjustment.value() < 36.0;
                        let mut current = ui.messages.borrow_mut();
                        if before.is_some() {
                            current
                                .extend(messages.into_iter().map(|message| (message.id, message)));
                            ui.has_older.set(has_older);
                        } else {
                            let older_loaded = merge_latest_history(
                                &mut current,
                                messages,
                                has_older,
                                requested_through,
                            );
                            if !older_loaded {
                                ui.has_older.set(has_older);
                            }
                        }
                        drop(current);
                        ui.render_history(before.is_none() && (!was_loaded || at_bottom));
                        if marked && has_unread {
                            ui.refresh_chats();
                        }
                        if before.is_some() {
                            let old_height = adjustment.upper();
                            let old_position = adjustment.value();
                            after_layout(
                                &ui.history,
                                glib::clone!(
                                    #[weak]
                                    ui,
                                    move || {
                                        if ui.generation.get() == generation {
                                            adjustment.set_value(
                                                old_position
                                                    + (adjustment.upper() - old_height).max(0.0),
                                            );
                                        }
                                    }
                                ),
                            );
                        }
                    }
                    Err(error) => {
                        if !ui.loaded.get() {
                            while let Some(child) = ui.history.first_child() {
                                ui.history.remove(&child);
                            }
                            let message = label("Не удалось загрузить историю", "dim-label");
                            message.set_halign(gtk::Align::Center);
                            ui.history.append(&message);
                        }
                        ui.toast(&format!("Не удалось загрузить историю. {error}"));
                    }
                }
                if let Some(mark_read) = ui.latest_history_pending.replace(None) {
                    ui.load_history(
                        None,
                        mark_read && (!ui.split.is_collapsed() || ui.split.shows_content()),
                    );
                }
            },
        );
    }

    fn render_history(self: &Rc<Self>, bottom: bool) {
        while let Some(child) = self.history.first_child() {
            self.history.remove(&child);
        }
        if self.has_older.get() {
            self.history.append(&self.older);
        }
        let messages = self.messages.borrow();
        if messages.is_empty() {
            let text = if self.loaded.get() {
                "Нет сообщений"
            } else {
                "Загрузка…"
            };
            let status = label(text, "dim-label");
            status.set_halign(gtk::Align::Center);
            self.history.append(&status);
        }
        let mut previous_day = None;
        for message in messages.values() {
            let timestamp = glib::DateTime::from_unix_local(message.timestamp).ok();
            let day = timestamp.as_ref().and_then(|time| time.format("%x").ok());
            if day != previous_day {
                let separator = label(day.as_deref().unwrap_or(""), "dim-label");
                separator.set_halign(gtk::Align::Center);
                self.history.append(&separator);
                previous_day = day;
            }
            let bubble = gtk::Box::new(gtk::Orientation::Vertical, 4);
            bubble.add_css_class("message-bubble");
            if message.outgoing {
                bubble.add_css_class("outgoing");
            }
            bubble.set_halign(if message.outgoing {
                gtk::Align::End
            } else {
                gtk::Align::Start
            });
            if !message.outgoing {
                bubble.append(&label(&message.sender, "message-name"));
            }
            if message.forwarded {
                bubble.append(&label("Пересланное сообщение", "dim-label"));
            }
            if let Some(id) = message.reply_to {
                let summary = messages
                    .get(&id)
                    .map_or_else(|| "Исходное сообщение".into(), Message::reply_summary);
                let reply = label(&format!("Ответ · {summary}"), "dim-label");
                reply.set_max_width_chars(44);
                reply.set_ellipsize(gtk::pango::EllipsizeMode::End);
                bubble.append(&reply);
            }
            if !message.text.is_empty() {
                let text = label(&message.text, "");
                text.set_wrap(true);
                text.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                text.set_max_width_chars(64);
                bubble.append(&text);
            }
            if let Some(name) = &message.attachment
                && let Some(chat) = self.selection.get()
            {
                let file_name = label(name, "");
                file_name.set_max_width_chars(32);
                file_name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                let id = message.id;
                if self.is_demo.get() && !demo::has_sample_attachment(chat, id) {
                    bubble.append(&file_name);
                } else {
                    let action = gtk::Button::new();
                    action.add_css_class("flat");
                    action.set_tooltip_text(Some("Сохранить вложение"));
                    let file = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                    file.append(&gtk::Image::from_icon_name("document-save-symbolic"));
                    file.append(&file_name);
                    action.set_child(Some(&file));
                    let name = name.clone();
                    action.connect_clicked(glib::clone!(
                        #[weak(rename_to = ui)]
                        self,
                        move |_| {
                            ui.save_file(chat, id, name.clone());
                        }
                    ));
                    bubble.append(&action);
                }
            }
            let mut time = timestamp
                .and_then(|time| time.format("%H:%M").ok())
                .unwrap_or_default();
            if message.edited {
                time = format!("Изменено · {time}").into();
            }
            let time = label(&time, "message-time");
            time.add_css_class("dim-label");
            let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            footer.set_halign(gtk::Align::End);
            if message.can_forward
                && let Some(chat) = self.selection.get()
            {
                let button = icon_button("mail-forward-symbolic", "Переслать");
                button.add_css_class("flat");
                let message = message.clone();
                button.connect_clicked(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |_| ui.forward_message(chat, message.clone())
                ));
                footer.append(&button);
            }
            if message.can_delete
                && let Some(chat) = self.selection.get()
            {
                let button = icon_button("user-trash-symbolic", "Удалить");
                button.add_css_class("flat");
                let id = message.id;
                button.connect_clicked(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |_| ui.delete_message(chat, id)
                ));
                footer.append(&button);
            }
            if message.can_edit
                && let Some(chat) = self.selection.get()
            {
                let button = icon_button("document-edit-symbolic", "Редактировать");
                button.add_css_class("flat");
                let id = message.id;
                let text = message.text.clone();
                button.connect_clicked(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |_| ui.edit_message(chat, id, &text, false)
                ));
                footer.append(&button);
            }
            if message.can_reply
                && self.input.is_sensitive()
                && let Some(chat) = self.selection.get()
            {
                let button = icon_button("mail-reply-sender-symbolic", "Ответить");
                button.add_css_class("flat");
                let reply = Reply {
                    id: message.id,
                    summary: message.reply_summary(),
                };
                button.connect_clicked(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |_| {
                        if ui.selection.get() == Some(chat) {
                            ui.replies.borrow_mut().insert(chat, reply.clone());
                            ui.update_send_button();
                            ui.input.grab_focus();
                        }
                    }
                ));
                footer.append(&button);
            }
            footer.append(&time);
            bubble.append(&footer);
            self.history.append(&bubble);
        }
        if bottom {
            let adjustment = self.scroll.vadjustment();
            let generation = self.generation.get();
            after_layout(
                &self.history,
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || {
                        if ui.generation.get() == generation {
                            adjustment.set_value(adjustment.upper());
                        }
                    }
                ),
            );
        }
    }

    fn update_send_button(&self) {
        let uploads = self.uploads.borrow();
        let upload = self.selection.get().and_then(|chat| uploads.get(&chat));
        self.upload_bar.set_visible(upload.is_some());
        if let Some(upload) = upload {
            self.upload_name.set_text(&upload.name);
        }
        let replies = self.replies.borrow();
        let reply = self.selection.get().and_then(|chat| replies.get(&chat));
        self.reply_bar.set_visible(reply.is_some());
        if let Some(reply) = reply {
            self.reply_label
                .set_text(&format!("Ответ · {}", reply.summary));
        }
        self.attach
            .set_sensitive(self.input.is_sensitive() && !self.sending.get());
        self.send.set_sensitive(
            self.input.is_sensitive()
                && (!self.input.text().trim().is_empty() || upload.is_some())
                && !self.sending.get(),
        );
    }

    fn choose_upload(self: &Rc<Self>, kind: UploadKind) {
        if !self.attach.is_sensitive() {
            return;
        }
        let (Some(window), Some(chat)) = (self.window.upgrade(), self.selection.get()) else {
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title(match kind {
                UploadKind::File => "Выберите файл",
                UploadKind::Photo => "Выберите фотографию",
            })
            .build();
        if kind == UploadKind::Photo {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Фотографии"));
            filter.add_mime_type("image/*");
            let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            dialog.set_filters(Some(&filters));
        }
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = dialog.open_future(Some(&window)).await;
            let (Some(ui), Ok(file)) = (weak.upgrade(), result) else {
                return;
            };
            if ui.window.upgrade().is_none() {
                return;
            }
            let Some(path) = file.path() else {
                ui.toast("Выберите локальный файл");
                return;
            };
            let name = attachment_name(
                &path.file_name().unwrap_or_default().to_string_lossy(),
                "Вложение",
            );
            ui.uploads
                .borrow_mut()
                .insert(chat, Upload { path, name, kind });
            ui.update_send_button();
        });
    }

    fn send_message(self: &Rc<Self>) {
        if !self.send.is_sensitive() {
            return;
        }
        let Some(chat) = self.selection.get() else {
            return;
        };
        let original = self.input.text().to_string();
        let text = original.trim().to_owned();
        let upload = self.uploads.borrow().get(&chat).cloned();
        let reply = self.replies.borrow().get(&chat).cloned();
        let reply_to = reply.as_ref().map(|reply| reply.id);
        if self.is_demo.get() {
            let message = Message {
                id: self.next_demo_message(),
                text: if text.is_empty() {
                    "Вложение".into()
                } else {
                    text
                },
                sender: "Вы".into(),
                outgoing: true,
                timestamp: demo::now(),
                attachment: upload.as_ref().map(|upload| upload.name.clone()),
                reply_to,
                can_reply: true,
                can_edit: upload.is_none(),
                can_delete: true,
                can_forward: true,
                forwarded: false,
                edited: false,
            };
            self.input.set_text("");
            self.uploads.borrow_mut().remove(&chat);
            self.replies.borrow_mut().remove(&chat);
            self.update_send_button();
            self.add_demo_message(chat, message);
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        self.sending.set(true);
        self.update_send_button();
        if upload.is_some() {
            self.toast("Отправляем вложение…");
        }
        let request_upload = upload.clone();
        self.job(
            async move { client.send(chat, text, request_upload, reply_to).await },
            move |ui, result| {
                ui.sending.set(false);
                match result {
                    Ok(message) => {
                        if ui.uploads.borrow().get(&chat) == upload.as_ref() {
                            ui.uploads.borrow_mut().remove(&chat);
                        }
                        if ui.replies.borrow().get(&chat) == reply.as_ref() {
                            ui.replies.borrow_mut().remove(&chat);
                        }
                        if ui.selection.get() == Some(chat) {
                            if ui.input.text() == original {
                                ui.input.set_text("");
                            }
                            ui.messages.borrow_mut().insert(message.id, message);
                            ui.render_history(true);
                        } else if ui.drafts.borrow().get(&chat) == Some(&original) {
                            ui.drafts.borrow_mut().remove(&chat);
                        }
                        ui.refresh_chats();
                    }
                    Err(error) => ui.toast(&format!("Не удалось отправить сообщение. {error}")),
                }
                ui.update_send_button();
            },
        );
    }

    fn edit_message(self: &Rc<Self>, chat: i64, id: i32, initial: &str, retry: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let dialog = adw::AlertDialog::builder()
            .heading("Редактировать сообщение")
            .close_response("cancel")
            .default_response("save")
            .build();
        dialog.add_response("cancel", "Отмена");
        dialog.add_response("save", "Сохранить");
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        let editor = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(10)
            .right_margin(10)
            .build();
        editor.update_property(&[gtk::accessible::Property::Label("Текст сообщения")]);
        let buffer = editor.buffer();
        buffer.set_text(initial);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&editor)
            .min_content_height(96)
            .max_content_height(230)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        scroll.add_css_class("card");
        dialog.set_extra_child(Some(&scroll));
        dialog.set_focus(Some(&editor));
        dialog.set_response_enabled("save", retry);
        buffer.connect_changed(glib::clone!(
            #[weak]
            dialog,
            #[strong(rename_to = initial)]
            initial.to_owned(),
            move |buffer| {
                let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
                let text = text.trim();
                dialog.set_response_enabled(
                    "save",
                    !text.is_empty() && (retry || text != initial) && text.chars().count() <= 4096,
                );
            }
        ));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            if dialog.choose_future(Some(&window)).await != "save" {
                return;
            }
            let Some(ui) = weak.upgrade() else { return };
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .trim()
                .to_owned();
            if ui.is_demo.get() {
                ui.apply_text_edit(chat, id, text);
                return;
            }
            let Some(client) = ui.client.borrow().clone() else {
                return;
            };
            let request_text = text.clone();
            ui.job(
                async move { client.edit_text(chat, id, request_text).await },
                move |ui, result| match result {
                    Ok(()) => {
                        ui.apply_text_edit(chat, id, text);
                        ui.refresh_chats();
                        if ui.selection.get() == Some(chat) {
                            ui.load_history(None, false);
                        }
                    }
                    Err(error) => {
                        if ui.selection.get() == Some(chat) {
                            ui.edit_message(chat, id, &text, true);
                        }
                        ui.toast(&format!("Не удалось сохранить правку. {error}"));
                    }
                },
            );
        });
    }

    fn next_demo_message(&self) -> i32 {
        let id = self.demo_next_message.get();
        self.demo_next_message.set(id + 1);
        id
    }

    fn add_demo_message(self: &Rc<Self>, chat: i64, message: Message) {
        self.demo_messages
            .borrow_mut()
            .entry(chat)
            .or_insert_with(|| demo::history(chat))
            .push(message.clone());
        if let Some(chat) = self
            .chats
            .borrow_mut()
            .iter_mut()
            .find(|item| item.id == chat)
        {
            chat.preview.clone_from(&message.text);
        }
        if self.selection.get() == Some(chat) {
            self.messages.borrow_mut().insert(message.id, message);
            self.render_history(true);
        }
        self.render_chats();
    }

    fn forward_message(self: &Rc<Self>, source: i64, message: Message) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if self.sending.get() {
            self.toast("Дождитесь текущей отправки");
            return;
        }
        let dialog = adw::AlertDialog::builder()
            .heading("Переслать сообщение")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", "Отмена");
        let chats = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let destination = Rc::new(Cell::new(None));
        for chat in self.chats.borrow().iter().filter(|chat| !chat.broadcast) {
            let button = gtk::Button::new();
            button.add_css_class("flat");
            let title = label(&chat.title, "");
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            button.set_child(Some(&title));
            let chat = chat.id;
            button.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                #[strong]
                destination,
                move |_| {
                    destination.set(Some(chat));
                    dialog.close();
                }
            ));
            chats.append(&button);
        }
        let scroll = gtk::ScrolledWindow::builder()
            .child(&chats)
            .max_content_height(280)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        dialog.set_extra_child(Some(&scroll));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            dialog.choose_future(Some(&window)).await;
            let (Some(ui), Some(chat)) = (weak.upgrade(), destination.get()) else {
                return;
            };
            if ui.is_demo.get() {
                ui.add_demo_message(
                    chat,
                    Message {
                        id: ui.next_demo_message(),
                        timestamp: demo::now(),
                        sender: "Вы".into(),
                        outgoing: true,
                        reply_to: None,
                        can_edit: false,
                        can_delete: true,
                        forwarded: true,
                        edited: false,
                        ..message
                    },
                );
                ui.toast("Сообщение переслано");
                return;
            }
            let Some(client) = ui.client.borrow().clone() else {
                return;
            };
            ui.sending.set(true);
            ui.update_send_button();
            ui.job(
                async move { client.forward_message(source, message.id, chat).await },
                move |ui, result| {
                    ui.sending.set(false);
                    ui.update_send_button();
                    match result {
                        Ok(message) => {
                            if ui.selection.get() == Some(chat) {
                                ui.messages.borrow_mut().insert(message.id, message);
                                ui.render_history(true);
                            }
                            ui.refresh_chats();
                            ui.toast("Сообщение переслано");
                        }
                        Err(error) => ui.toast(&format!("Не удалось переслать сообщение. {error}")),
                    }
                },
            );
        });
    }

    fn apply_text_edit(self: &Rc<Self>, chat: i64, id: i32, text: String) {
        if self.is_demo.get() {
            let mut demo = self.demo_messages.borrow_mut();
            if let Some(messages) = demo.get_mut(&chat)
                && let Some(message) = messages.iter_mut().find(|message| message.id == id)
            {
                message.text.clone_from(&text);
                message.edited = true;
                if messages.last().is_some_and(|message| message.id == id)
                    && let Some(chat) = self
                        .chats
                        .borrow_mut()
                        .iter_mut()
                        .find(|item| item.id == chat)
                {
                    chat.preview.clone_from(&text);
                }
                self.render_chats();
            }
        }
        if self.selection.get() == Some(chat)
            && let Some(message) = self.messages.borrow_mut().get_mut(&id)
        {
            message.text = text;
            message.edited = true;
        }
        if self.selection.get() == Some(chat) {
            self.render_history(false);
        }
    }

    fn delete_message(self: &Rc<Self>, chat: i64, id: i32) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let dialog = adw::AlertDialog::builder()
            .heading("Удалить сообщение?")
            .body("Сообщение будет удалено для всех участников чата.")
            .close_response("cancel")
            .default_response("cancel")
            .build();
        dialog.add_response("cancel", "Отмена");
        dialog.add_response("delete", "Удалить для всех");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            if dialog.choose_future(Some(&window)).await != "delete" {
                return;
            }
            let Some(ui) = weak.upgrade() else { return };
            if ui.is_demo.get() {
                ui.apply_deletion(chat, id);
                return;
            }
            let Some(client) = ui.client.borrow().clone() else {
                return;
            };
            ui.job(
                async move { client.delete_message(chat, id).await },
                move |ui, result| match result {
                    Ok(()) => {
                        ui.apply_deletion(chat, id);
                        ui.refresh_chats();
                        if ui.selection.get() == Some(chat) {
                            ui.load_history(None, false);
                        }
                    }
                    Err(error) => ui.toast(&format!("Не удалось удалить сообщение. {error}")),
                },
            );
        });
    }

    fn apply_deletion(self: &Rc<Self>, chat: i64, id: i32) {
        if self.is_demo.get()
            && let Some(messages) = self.demo_messages.borrow_mut().get_mut(&chat)
        {
            messages.retain(|message| message.id != id);
            if let Some(chat) = self
                .chats
                .borrow_mut()
                .iter_mut()
                .find(|item| item.id == chat)
            {
                chat.preview = messages
                    .last()
                    .map(|message| message.text.clone())
                    .unwrap_or_default();
            }
            self.render_chats();
        }
        if self
            .replies
            .borrow()
            .get(&chat)
            .is_some_and(|reply| reply.id == id)
        {
            self.replies.borrow_mut().remove(&chat);
        }
        if self.selection.get() == Some(chat) {
            self.messages.borrow_mut().remove(&id);
            self.update_send_button();
            self.render_history(false);
        }
    }

    fn save_file(self: &Rc<Self>, chat: i64, id: i32, name: String) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if self.downloading.replace(true) {
            self.toast("Дождитесь текущей загрузки");
            return;
        }
        let dialog = gtk::FileDialog::builder()
            .title("Сохранить вложение")
            .initial_name(&name)
            .build();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let target = dialog.save_future(Some(&window)).await;
            let Some(ui) = weak.upgrade() else { return };
            let Ok(target) = target else {
                ui.downloading.set(false);
                return;
            };
            let Some(path) = target.path() else {
                ui.downloading.set(false);
                ui.toast("Выберите локальный каталог");
                return;
            };
            let client = ui.client.borrow().clone();
            ui.toast("Загрузка вложения…");
            ui.job(
                async move {
                    if let Some(client) = client {
                        client.save(chat, id, path).await
                    } else {
                        let mut output = PendingSave::new(path)?;
                        output.write(demo::FILE_CONTENT.as_bytes()).await?;
                        output.finish(Some(demo::FILE_CONTENT.len())).await
                    }
                },
                |ui, result| {
                    ui.downloading.set(false);
                    match result {
                        Ok(()) => ui.toast("Вложение сохранено"),
                        Err(error) => ui.toast(&format!("Не удалось сохранить вложение. {error}")),
                    }
                },
            );
        });
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }
}

fn label(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    if !class.is_empty() {
        label.add_css_class(class);
    }
    label
}

fn icon_button(icon: &str, description: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(description)
        .build()
}

fn after_layout(widget: &impl IsA<gtk::Widget>, action: impl FnOnce() + 'static) {
    let action = Cell::new(Some(action));
    let ticks = Cell::new(0);
    widget.add_tick_callback(move |_, _| {
        ticks.set(ticks.get() + 1);
        if ticks.get() == 2 {
            action.take().unwrap()();
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}
