use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::demo;
use crate::formatting::{
    concealed_message, editor_text, message_attributes, set_editor_text, setup_editor,
};
use crate::media_viewer::fit_viewport as photo_viewport;
use crate::model::PlayableMedia;
use crate::model::{
    COMMON_REACTIONS, Chat, ChatFolder, ChatImage, Contact, FormattedText, HISTORY_PAGE_SIZE,
    HistoryCache, HistoryRevision, IncomingMessage, MAX_ALBUM_ITEMS, Message, Reaction,
    ReadRequest, Reply, SearchHit, TextStyle, UpdateStatus, Upload, UploadKind,
    merge_latest_history, order_chats, redact_spoilers,
};
use crate::storage::{PendingSave, attachment_name};
use crate::telegram::{LoginStep, Telegram};

type ImageTarget = Box<dyn FnOnce(&gtk::gdk::Texture)>;

enum ImageCacheEntry {
    Loading(Vec<ImageTarget>),
    Ready(gtk::gdk::Texture),
    Unavailable(Instant),
}

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
    chat_menu: gtk::Popover,
    menu_chat: Cell<Option<i64>>,
    restore_chat_focus: Cell<bool>,
    pinning_chats: RefCell<HashSet<i64>>,
    reading_chats: RefCell<HashSet<i64>>,
    unread_overrides: RefCell<HashSet<i64>>,
    filter: gtk::SearchEntry,
    archive: gtk::ToggleButton,
    archive_chat: gtk::Button,
    mute_chat: gtk::Button,
    search_in_chat: gtk::Button,
    folder_picker: gtk::DropDown,
    content: gtk::Stack,
    title: adw::WindowTitle,
    profile_button: gtk::Button,
    profile_dialog: glib::WeakRef<adw::Dialog>,
    contacts_dialog: glib::WeakRef<adw::Dialog>,
    history: gtk::Box,
    message_menu: gtk::Popover,
    menu_message: Cell<Option<i32>>,
    scroll: gtk::ScrolledWindow,
    older: gtk::Button,
    adjusting_history: Cell<bool>,
    latest: gtk::Button,
    input: gtk::TextView,
    input_placeholder: gtk::Label,
    send: gtk::Button,
    attach: gtk::MenuButton,
    upload_bar: gtk::Box,
    upload_name: gtk::Label,
    reply_bar: gtk::Box,
    reply_sender: gtk::Label,
    reply_label: gtk::Label,
    client: RefCell<Option<Arc<Telegram>>>,
    login_step: Cell<LoginStep>,
    is_demo: Cell<bool>,
    chats: RefCell<Vec<Chat>>,
    folders: RefCell<Vec<ChatFolder>>,
    folders_generation: Cell<u64>,
    updating_folders: Cell<bool>,
    external_chat: RefCell<Option<Chat>>,
    message_search: glib::WeakRef<adw::Dialog>,
    selection: Cell<Option<i64>>,
    notification_chat: Cell<Option<i64>>,
    notified_messages: RefCell<HashMap<i64, i32>>,
    generation: Cell<u64>,
    chat_refresh: Cell<bool>,
    chat_refresh_pending: Cell<bool>,
    moving_chats: RefCell<HashSet<i64>>,
    muting_chats: RefCell<HashSet<i64>>,
    chat_settings_revision: Cell<u64>,
    history_refresh: Cell<bool>,
    latest_history_pending: Cell<Option<bool>>,
    search_anchor: Cell<Option<i32>>,
    scroll_to_search_result: Cell<bool>,
    sending: Cell<bool>,
    downloading: Cell<bool>,
    messages: RefCell<BTreeMap<i32, Message>>,
    revealed_spoilers: RefCell<HashMap<i32, FormattedText>>,
    history_cache: RefCell<HistoryCache>,
    history_revision: Cell<HistoryRevision>,
    history_refreshed: Cell<Option<Instant>>,
    demo_messages: RefCell<HashMap<i64, Vec<Message>>>,
    demo_next_message: Cell<i32>,
    drafts: RefCell<HashMap<i64, FormattedText>>,
    uploads: RefCell<HashMap<i64, Vec<Upload>>>,
    replies: RefCell<HashMap<i64, Reply>>,
    reacting: RefCell<HashSet<(i64, i32)>>,
    images: RefCell<HashMap<ChatImage, ImageCacheEntry>>,
    demo_image: OnceCell<gtk::gdk::MemoryTexture>,
    media_viewer: glib::WeakRef<adw::Window>,
    media_generation: Cell<u64>,
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
    let search_messages = icon_button("system-search-symbolic", "Поиск по сообщениям");
    side_header.pack_end(&refresh);
    side_header.pack_end(&search_messages);
    let archive = gtk::ToggleButton::builder()
        .icon_name("folder-symbolic")
        .tooltip_text("Архив чатов")
        .build();
    side_header.pack_start(&archive);
    let contacts_button = icon_button("x-office-address-book-symbolic", "Контакты");
    side_header.pack_start(&contacts_button);
    sidebar.add_top_bar(&side_header);
    let side_body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let filter = gtk::SearchEntry::builder()
        .placeholder_text("Найти чат")
        .margin_top(8)
        .margin_start(10)
        .margin_end(10)
        .build();
    side_body.append(&filter);
    let folder_picker = gtk::DropDown::from_strings(&["Все чаты"]);
    folder_picker.set_margin_start(10);
    folder_picker.set_margin_end(10);
    folder_picker.set_visible(false);
    folder_picker.update_property(&[gtk::accessible::Property::Label("Папки чатов")]);
    side_body.append(&folder_picker);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("navigation-sidebar");
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    let list_overlay = gtk::Overlay::new();
    list_overlay.set_child(Some(&list_scroll));
    let chat_menu = gtk::Popover::new();
    chat_menu.set_has_arrow(false);
    list_overlay.add_overlay(&chat_menu);
    side_body.append(&list_overlay);
    sidebar.set_content(Some(&side_body));
    split.set_sidebar(Some(&adw::NavigationPage::new(&sidebar, "Чаты")));

    let pane = adw::ToolbarView::new();
    let pane_header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Чаты", "");
    let profile_button = gtk::Button::builder()
        .child(&title)
        .tooltip_text("Информация о чате")
        .sensitive(false)
        .build();
    profile_button.add_css_class("flat");
    profile_button.update_property(&[gtk::accessible::Property::Label("Информация о чате")]);
    pane_header.set_title_widget(Some(&profile_button));
    let archive_chat = icon_button("folder-symbolic", "Переместить в архив");
    archive_chat.set_visible(false);
    pane_header.pack_end(&archive_chat);
    let mute_chat = icon_button(
        "preferences-system-notifications-symbolic",
        "Выключить уведомления",
    );
    mute_chat.set_visible(false);
    pane_header.pack_end(&mute_chat);
    let search_in_chat = icon_button("system-search-symbolic", "Поиск в чате");
    search_in_chat.set_visible(false);
    pane_header.pack_end(&search_in_chat);
    pane.add_top_bar(&pane_header);
    let content = gtk::Stack::new();
    let empty = adw::StatusPage::builder()
        .icon_name("mail-unread-symbolic")
        .title("Выберите чат")
        .build();
    content.add_named(&empty, Some("empty"));
    let conversation = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let latest = gtk::Button::with_label("К последним сообщениям");
    latest.add_css_class("flat");
    latest.set_visible(false);
    conversation.append(&latest);
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
    let history_overlay = gtk::Overlay::new();
    history_overlay.set_vexpand(true);
    history_overlay.set_child(Some(&scroll));
    let message_menu = gtk::Popover::new();
    message_menu.set_has_arrow(false);
    history_overlay.add_overlay(&message_menu);
    conversation.append(&history_overlay);

    let composer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    composer.add_css_class("composer");
    let attach = gtk::MenuButton::builder()
        .icon_name("mail-attachment-symbolic")
        .tooltip_text("Добавить вложение")
        .build();
    let attach_menu = gtk::gio::Menu::new();
    attach_menu.append(Some("Файлы"), Some("win.attach-file"));
    attach_menu.append(Some("Фотографии"), Some("win.attach-photo"));
    attach.set_menu_model(Some(&attach_menu));
    let upload_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    upload_bar.add_css_class("composer");
    upload_bar.set_visible(false);
    let upload_name = label("", "");
    upload_name.set_hexpand(true);
    upload_name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let remove_upload = icon_button("edit-clear-symbolic", "Убрать вложения");
    upload_bar.append(&upload_name);
    upload_bar.append(&remove_upload);
    let reply_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    reply_bar.add_css_class("composer");
    reply_bar.set_visible(false);
    let (reply_quote, reply_sender, reply_label) = reply_quote();
    reply_quote.set_hexpand(true);
    let remove_reply = icon_button("edit-clear-symbolic", "Отменить ответ");
    reply_bar.append(&reply_quote);
    reply_bar.append(&remove_reply);
    let input = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .accepts_tab(false)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(10)
        .right_margin(10)
        .build();
    input.update_property(&[gtk::accessible::Property::Label("Сообщение")]);
    setup_editor(&input);
    let input_scroll = gtk::ScrolledWindow::builder()
        .child(&input)
        .min_content_height(36)
        .max_content_height(132)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let input_box = gtk::Overlay::new();
    input_box.add_css_class("composer-field");
    input_box.set_hexpand(true);
    input_box.set_child(Some(&input_scroll));
    let input_placeholder = label("Сообщение", "dim-label");
    input_placeholder.set_halign(gtk::Align::Start);
    input_placeholder.set_valign(gtk::Align::Start);
    input_placeholder.set_margin_start(10);
    input_placeholder.set_margin_top(8);
    input_placeholder.set_can_target(false);
    input_placeholder.set_accessible_role(gtk::AccessibleRole::Presentation);
    input_box.add_overlay(&input_placeholder);
    let send = icon_button("mail-send-symbolic", "Отправить");
    send.add_css_class("suggested-action");
    composer.append(&attach);
    composer.append(&input_box);
    attach.set_valign(gtk::Align::End);
    send.set_valign(gtk::Align::End);
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
        chat_menu,
        menu_chat: Cell::new(None),
        restore_chat_focus: Cell::new(false),
        pinning_chats: RefCell::new(HashSet::new()),
        reading_chats: RefCell::new(HashSet::new()),
        unread_overrides: RefCell::new(HashSet::new()),
        filter,
        archive,
        archive_chat,
        mute_chat,
        search_in_chat,
        folder_picker,
        content,
        title,
        profile_button,
        profile_dialog: glib::WeakRef::new(),
        contacts_dialog: glib::WeakRef::new(),
        history,
        message_menu,
        menu_message: Cell::new(None),
        scroll,
        older,
        adjusting_history: Cell::new(false),
        latest,
        input,
        input_placeholder,
        send,
        attach,
        upload_bar,
        upload_name,
        reply_bar,
        reply_sender,
        reply_label,
        client: RefCell::new(None),
        login_step: Cell::new(LoginStep::Phone),
        is_demo: Cell::new(false),
        chats: RefCell::new(Vec::new()),
        folders: RefCell::new(Vec::new()),
        folders_generation: Cell::new(0),
        updating_folders: Cell::new(false),
        external_chat: RefCell::new(None),
        message_search: glib::WeakRef::new(),
        selection: Cell::new(None),
        notification_chat: Cell::new(None),
        notified_messages: RefCell::new(HashMap::new()),
        generation: Cell::new(0),
        chat_refresh: Cell::new(false),
        chat_refresh_pending: Cell::new(false),
        moving_chats: RefCell::new(HashSet::new()),
        muting_chats: RefCell::new(HashSet::new()),
        chat_settings_revision: Cell::new(0),
        history_refresh: Cell::new(false),
        latest_history_pending: Cell::new(None),
        search_anchor: Cell::new(None),
        scroll_to_search_result: Cell::new(false),
        sending: Cell::new(false),
        downloading: Cell::new(false),
        messages: RefCell::new(BTreeMap::new()),
        revealed_spoilers: RefCell::new(HashMap::new()),
        history_cache: RefCell::new(HistoryCache::default()),
        history_revision: Cell::new(HistoryRevision::default()),
        history_refreshed: Cell::new(None),
        demo_messages: RefCell::new(HashMap::new()),
        demo_next_message: Cell::new(200),
        drafts: RefCell::new(HashMap::new()),
        uploads: RefCell::new(HashMap::new()),
        replies: RefCell::new(HashMap::new()),
        reacting: RefCell::new(HashSet::new()),
        images: RefCell::new(HashMap::new()),
        demo_image: OnceCell::new(),
        media_viewer: glib::WeakRef::new(),
        media_generation: Cell::new(0),
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
            ui.refresh_folders();
            ui.load_history(None, false);
        }
    ));
    ui.archive.connect_toggled(glib::clone!(
        #[weak]
        ui,
        move |_| {
            ui.folder_picker.set_sensitive(!ui.archive.is_active());
            ui.render_chats();
            ui.split.set_show_content(false);
        }
    ));
    ui.folder_picker.connect_selected_notify(glib::clone!(
        #[weak]
        ui,
        move |_| {
            if !ui.updating_folders.get() {
                ui.render_chats();
            }
        }
    ));
    ui.archive_chat.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.move_selected_chat()
    ));
    ui.mute_chat.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.toggle_chat_notifications()
    ));
    search_messages.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.open_message_search(false)
    ));
    ui.search_in_chat.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.open_message_search(true)
    ));
    ui.profile_button.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.open_chat_profile()
    ));
    for (name, scoped, accelerator) in [
        ("search-chat", true, "<Primary>f"),
        ("search-messages", false, "<Primary><Shift>f"),
    ] {
        let action = gtk::gio::SimpleAction::new(name, None);
        action.connect_activate(glib::clone!(
            #[weak]
            ui,
            move |_, _| ui.open_message_search(scoped)
        ));
        window.add_action(&action);
        app.set_accels_for_action(&format!("win.{name}"), &[accelerator]);
    }
    contacts_button.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.open_contacts()
    ));
    ui.latest.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| {
            if let Some(chat) = ui.selection.get() {
                ui.show_chat(chat, None);
            }
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
    ui.scroll.vadjustment().connect_value_changed(glib::clone!(
        #[weak]
        ui,
        move |_| ui.load_older_near_top()
    ));
    ui.scroll.connect_edge_overshot(glib::clone!(
        #[weak]
        ui,
        move |_, edge| {
            if edge == gtk::PositionType::Top {
                ui.load_older_near_top();
            }
        }
    ));
    ui.send.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.send_message()
    ));
    let send_keys = gtk::EventControllerKey::new();
    send_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    send_keys.connect_key_pressed(glib::clone!(
        #[weak]
        ui,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |controller, key, _, modifiers| {
            if !matches!(
                key,
                gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter | gtk::gdk::Key::ISO_Enter
            ) || modifiers.intersects(
                gtk::gdk::ModifierType::SHIFT_MASK
                    | gtk::gdk::ModifierType::CONTROL_MASK
                    | gtk::gdk::ModifierType::ALT_MASK
                    | gtk::gdk::ModifierType::SUPER_MASK
                    | gtk::gdk::ModifierType::META_MASK,
            ) {
                return glib::Propagation::Proceed;
            }
            if !controller
                .current_event()
                .is_some_and(|event| ui.input.im_context_filter_keypress(event))
            {
                ui.send_message();
            }
            glib::Propagation::Stop
        }
    ));
    ui.input.add_controller(send_keys);
    ui.input.buffer().connect_changed(glib::clone!(
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

    let menu_keys = gtk::EventControllerKey::new();
    menu_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    menu_keys.connect_key_pressed(glib::clone!(
        #[weak]
        ui,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key != gtk::gdk::Key::Escape {
                return glib::Propagation::Proceed;
            }
            ui.message_menu.popdown();
            if let Some(bubble) = ui.menu_message.get().and_then(|id| ui.message_bubble(id)) {
                bubble.grab_focus();
            }
            glib::Propagation::Stop
        }
    ));
    ui.message_menu.add_controller(menu_keys);
    let chat_menu_keys = gtk::EventControllerKey::new();
    chat_menu_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    chat_menu_keys.connect_key_pressed(glib::clone!(
        #[weak]
        ui,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                ui.restore_chat_focus.set(true);
                ui.chat_menu.popdown();
                if let Some(id) = ui.menu_chat.get() {
                    ui.focus_chat_row(id);
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    ));
    ui.chat_menu.add_controller(chat_menu_keys);
    ui.chat_menu.connect_closed(glib::clone!(
        #[weak]
        ui,
        move |_| {
            if let Some(id) = ui.menu_chat.take()
                && ui.restore_chat_focus.replace(false)
            {
                ui.focus_chat_row(id);
            }
        }
    ));
    window.connect_is_active_notify(glib::clone!(
        #[weak]
        ui,
        move |_| {
            if let Some(id) = ui.selection.get()
                && ui.chat_in_foreground(id)
            {
                ui.withdraw_chat_notification(id);
            }
        }
    ));

    let open_chat = gtk::gio::SimpleAction::new("open-chat", Some(glib::VariantTy::INT64));
    open_chat.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_, target| {
            if let Some(id) = target.and_then(glib::Variant::get::<i64>) {
                ui.open_notified_chat(id);
            }
        }
    ));
    window.add_action(&open_chat);

    window.connect_close_request({
        let ui = Rc::clone(&ui);
        move |_| {
            if let Some(app) = ui.window.upgrade().and_then(|window| window.application()) {
                for id in ui.notified_messages.borrow().keys() {
                    app.withdraw_notification(&notification_id(*id));
                }
            }
            if let Some(viewer) = ui.media_viewer.upgrade() {
                viewer.close();
            }
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
                            ui.unread_overrides.borrow_mut().clear();
                            ui.reading_chats.borrow_mut().clear();
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
        self.unread_overrides.borrow_mut().clear();
        self.reading_chats.borrow_mut().clear();
        self.client.replace(None);
        self.root.set_visible_child_name("chats");
        self.chats.replace(demo::chats());
        self.set_folders(demo::folders());
        if !self.split.is_collapsed() {
            self.select_chat(1);
        }
    }

    fn start_client(self: &Rc<Self>) {
        self.root.set_visible_child_name("chats");
        self.refresh_chats();
        self.refresh_folders();
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let mut changes = client.changes();
        let stop = self.stop.clone();
        let (incoming, mut messages) = tokio::sync::mpsc::channel(32);
        self.job(client.clone().listen(stop, incoming), |ui, result| {
            if result.is_err() {
                ui.toast(
                    "Обновления Telegram остановлены. Перезапустите клиент, чтобы проверить сессию.",
                );
            }
        });
        let (allowed, mut desktop_messages) = tokio::sync::mpsc::channel(32);
        let notification_client = client.clone();
        self.runtime.spawn(async move {
            while let Some(message) = messages.recv().await {
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    notification_client.notification_enabled(message.chat, message.kind),
                )
                .await;
                if matches!(result, Ok(Ok(true))) && allowed.send(message).await.is_err() {
                    break;
                }
            }
        });
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            while let Some(message) = desktop_messages.recv().await {
                let Some(ui) = weak.upgrade() else { break };
                if !ui.chat_in_foreground(message.chat) {
                    ui.notify_message(&message);
                }
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
                    if (!ui.split.is_collapsed() || ui.split.shows_content())
                        && ui.selection.get().is_some_and(|chat| {
                            !ui.loaded.get()
                                || ui.history_revision.get() != client.history_revision(chat)
                        })
                    {
                        ui.load_history(None, true);
                    }
                }
                if status.folders_revision != previous.folders_revision {
                    ui.refresh_folders();
                }
                previous = status;
            }
        });
    }

    fn chat_in_foreground(&self, id: i64) -> bool {
        self.window
            .upgrade()
            .is_some_and(|window| window.is_active())
            && self.selection.get() == Some(id)
            && (!self.split.is_collapsed() || self.split.shows_content())
    }

    fn notify_message(&self, message: &IncomingMessage) {
        if self.muting_chats.borrow().contains(&message.chat)
            || self
                .chats
                .borrow()
                .iter()
                .any(|chat| chat.id == message.chat && chat.muted)
        {
            return;
        }
        let Some(app) = self
            .window
            .upgrade()
            .and_then(|window| window.application())
        else {
            return;
        };
        let notification = gtk::gio::Notification::new(&message.title);
        notification.set_body(Some("Новое сообщение"));
        notification
            .set_default_action_and_target_value("app.open-chat", Some(&message.chat.to_variant()));
        let mut notified = self.notified_messages.borrow_mut();
        if notified
            .get(&message.chat)
            .is_some_and(|id| *id >= message.message)
        {
            return;
        }
        notified.insert(message.chat, message.message);
        app.send_notification(Some(&notification_id(message.chat)), &notification);
    }

    fn withdraw_chat_notification(&self, chat: i64) {
        if self.notified_messages.borrow().contains_key(&chat)
            && let Some(app) = self
                .window
                .upgrade()
                .and_then(|window| window.application())
        {
            app.withdraw_notification(&notification_id(chat));
        }
    }

    fn open_notified_chat(self: &Rc<Self>, id: i64) {
        self.begin_media_preview();
        if let Some(window) = self.window.upgrade() {
            window.present();
        }
        if self.chats.borrow().iter().any(|chat| chat.id == id) {
            self.show_chat(id, None);
        } else {
            self.notification_chat.set(Some(id));
            if self.login_step.get() == LoginStep::Authorized {
                self.refresh_chats();
            }
        }
    }

    fn refresh_chats(self: &Rc<Self>) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        if self.chat_refresh.replace(true) {
            self.chat_refresh_pending.set(true);
            return;
        }
        let revision = self.chat_settings_revision.get();
        self.job(async move { client.chats().await }, move |ui, result| {
            ui.chat_refresh.set(false);
            if revision != ui.chat_settings_revision.get() {
                ui.chat_refresh_pending.set(true);
            } else {
                match result {
                    Ok(chats) => {
                        for chat in chats.iter().filter(|chat| chat.unread == 0 || chat.muted) {
                            ui.withdraw_chat_notification(chat.id);
                        }
                        ui.chats.replace(chats);
                        ui.render_chats();
                        if let Some(id) = ui.notification_chat.take() {
                            if ui.chats.borrow().iter().any(|chat| chat.id == id) {
                                ui.show_chat(id, None);
                            } else if ui.chat_refresh_pending.get() {
                                ui.notification_chat.set(Some(id));
                            } else {
                                ui.toast("Чат из уведомления недоступен");
                            }
                        }
                    }
                    Err(error) => ui.toast(&format!("Не удалось загрузить чаты. {error}")),
                }
            }
            if ui.chat_refresh_pending.replace(false) {
                ui.refresh_chats();
            }
        });
    }

    fn refresh_folders(self: &Rc<Self>) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let request = self.folders_generation.get().wrapping_add(1);
        self.folders_generation.set(request);
        self.job(async move { client.folders().await }, move |ui, result| {
            if request != ui.folders_generation.get() {
                return;
            }
            match result {
                Ok(folders) => ui.set_folders(folders),
                Err(error) => ui.toast(&format!("Не удалось загрузить папки. {error}")),
            }
        });
    }

    fn set_folders(self: &Rc<Self>, folders: Vec<ChatFolder>) {
        let current_id = self.selected_folder().map(|folder| folder.id);
        let names = gtk::StringList::new(&["Все чаты"]);
        let mut selection = 0;
        for (index, folder) in folders.iter().enumerate() {
            names.append(&folder.title);
            if current_id == Some(folder.id) {
                selection = index as u32 + 1;
            }
        }
        self.updating_folders.set(true);
        self.folder_picker.set_model(Some(&names));
        self.folder_picker.set_selected(selection);
        self.folder_picker.set_visible(!folders.is_empty());
        self.folders.replace(folders);
        self.updating_folders.set(false);
        self.render_chats();
    }

    fn selected_folder(&self) -> Option<ChatFolder> {
        let index = self.folder_picker.selected().checked_sub(1)? as usize;
        self.folders.borrow().get(index).cloned()
    }

    fn render_chats(self: &Rc<Self>) {
        self.update_archive_button();
        self.update_mute_button();
        let focused = self
            .list
            .root()
            .and_then(|root| gtk::prelude::RootExt::focus(&root))
            .and_then(|widget| widget.ancestor(gtk::ListBoxRow::static_type()))
            .filter(|row| row.parent().is_some_and(|parent| parent == self.list))
            .and_then(|row| row.widget_name().parse().ok());
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let query = self.filter.text().trim().to_lowercase();
        let archive = self.archive.is_active();
        let folder = (!archive).then(|| self.selected_folder()).flatten();
        let chats = self.chats.borrow();
        let mut visible = chats
            .iter()
            .filter(|chat| {
                let in_list = folder
                    .as_ref()
                    .map_or(chat.archived == archive, |folder| folder.contains(chat));
                in_list && chat.title.to_lowercase().contains(&query)
            })
            .collect::<Vec<_>>();
        order_chats(&mut visible, folder.as_ref());
        for chat in visible {
            let row = gtk::ListBoxRow::new();
            row.set_widget_name(&chat.id.to_string());
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            line.add_css_class("chat-row");
            let avatar = adw::Avatar::new(38, Some(&chat.title), true);
            if chat.saved_messages {
                avatar.set_icon_name(Some("user-bookmarks-symbolic"));
                avatar.set_show_initials(false);
            } else if let Some(photo) = chat.avatar {
                self.load_avatar(&avatar, ChatImage::Avatar(photo));
            }
            line.append(&avatar);
            let description = gtk::Box::new(gtk::Orientation::Vertical, 3);
            description.set_hexpand(true);
            description.set_valign(gtk::Align::Center);
            let title = label(&chat.title, "heading");
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let preview_text = chat.preview_sender.as_ref().map_or_else(
                || chat.preview.clone(),
                |sender| format!("{sender}: {}", chat.preview),
            );
            let preview = label(&preview_text, "chat-preview");
            preview.add_css_class("dim-label");
            preview.set_single_line_mode(true);
            preview.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let title_line = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            title.set_hexpand(true);
            title_line.append(&title);
            if folder
                .as_ref()
                .map_or(chat.pinned, |folder| folder.pinned.contains(&chat.id))
            {
                let pin = gtk::Image::from_icon_name("view-pin-symbolic");
                pin.set_pixel_size(12);
                pin.add_css_class("dim-label");
                pin.set_tooltip_text(Some("Закреплённый чат"));
                pin.update_property(&[gtk::accessible::Property::Label("Закреплённый чат")]);
                title_line.append(&pin);
            }
            if chat.muted {
                let muted = gtk::Image::from_icon_name("notifications-disabled-symbolic");
                muted.set_pixel_size(12);
                muted.add_css_class("dim-label");
                muted.set_tooltip_text(Some("Уведомления выключены"));
                title_line.append(&muted);
            }
            description.append(&title_line);
            let preview_line = gtk::Box::new(gtk::Orientation::Horizontal, 5);
            if let Some(image) = chat.preview_image {
                let thumbnail = gtk::Overlay::new();
                thumbnail.set_size_request(18, 18);
                thumbnail.set_valign(gtk::Align::Center);
                thumbnail.set_overflow(gtk::Overflow::Hidden);
                thumbnail.set_visible(false);
                let picture = gtk::Picture::builder()
                    .content_fit(gtk::ContentFit::Cover)
                    .can_shrink(true)
                    .alternative_text("Миниатюра вложения")
                    .build();
                picture.update_property(&[gtk::accessible::Property::Label("Миниатюра вложения")]);
                thumbnail.add_overlay(&picture);
                self.load_chat_image(
                    image,
                    glib::clone!(
                        #[weak]
                        thumbnail,
                        #[weak]
                        picture,
                        move |texture: &gtk::gdk::Texture| {
                            picture.set_paintable(Some(texture));
                            thumbnail.set_visible(true);
                        }
                    ),
                );
                preview_line.append(&thumbnail);
            }
            preview_line.append(&preview);
            description.append(&preview_line);
            line.append(&description);
            if chat.is_unread() {
                let value = if chat.unread > 0 {
                    chat.unread.to_string()
                } else {
                    "●".into()
                };
                let unread = label(&value, "unread-count");
                unread.update_property(&[gtk::accessible::Property::Label(if chat.unread > 0 {
                    "Непрочитанные сообщения"
                } else {
                    "Отмечен непрочитанным"
                })]);
                unread.set_valign(gtk::Align::Center);
                line.append(&unread);
            }
            row.set_child(Some(&line));
            self.connect_chat_menu(&row, chat.id);
            self.list.append(&row);
            if self.selection.get() == Some(chat.id) {
                self.list.select_row(Some(&row));
            }
        }
        if let Some(id) = focused {
            self.focus_chat_row(id);
        }
    }

    fn select_chat(self: &Rc<Self>, id: i64) {
        self.show_chat(id, None);
    }

    fn connect_chat_menu(self: &Rc<Self>, row: &gtk::ListBoxRow, id: i64) {
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_SECONDARY);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        click.connect_pressed(|gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        click.connect_released(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            #[weak]
            row,
            move |_, _, x, y| ui.open_chat_menu(&row, id, x, y)
        ));
        row.add_controller(click);
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            #[weak]
            row,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifiers| {
                if key == gtk::gdk::Key::Menu
                    || (key == gtk::gdk::Key::F10
                        && modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK))
                {
                    ui.open_chat_menu(
                        &row,
                        id,
                        f64::from(row.width()) / 2.0,
                        f64::from(row.height()) / 2.0,
                    );
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        row.add_controller(keys);
    }

    fn focus_chat_row(&self, id: i64) {
        let mut index = 0;
        while let Some(row) = self.list.row_at_index(index) {
            if row.widget_name() == id.to_string() {
                row.grab_focus();
                break;
            }
            index += 1;
        }
    }

    fn open_chat_menu(self: &Rc<Self>, row: &gtk::ListBoxRow, id: i64, x: f64, y: f64) {
        let folder = (!self.archive.is_active())
            .then(|| self.selected_folder())
            .flatten();
        let Some((pinned, unread, through)) = self
            .chats
            .borrow()
            .iter()
            .find(|chat| chat.id == id)
            .map(|chat| {
                (
                    folder
                        .as_ref()
                        .map_or(chat.pinned, |folder| folder.pinned.contains(&id)),
                    chat.is_unread(),
                    chat.last_message_id,
                )
            })
        else {
            return;
        };
        let Some(parent) = self.chat_menu.parent() else {
            return;
        };
        let Some(point) =
            row.compute_point(&parent, &gtk::graphene::Point::new(x as f32, y as f32))
        else {
            return;
        };
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let folder = folder.map(|folder| folder.id);
        let pin = gtk::Button::with_label(if pinned {
            "Открепить чат"
        } else {
            "Закрепить чат"
        });
        pin.add_css_class("flat");
        pin.connect_clicked(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| {
                ui.restore_chat_focus.set(true);
                ui.chat_menu.popdown();
                ui.pin_chat(id, !pinned, folder);
            }
        ));
        menu.append(&pin);
        pin.set_sensitive(!self.pinning_chats.borrow().contains(&id));
        let read = gtk::Button::with_label(if unread {
            "Отметить прочитанным"
        } else {
            "Отметить непрочитанным"
        });
        read.add_css_class("flat");
        read.set_sensitive(!self.reading_chats.borrow().contains(&id));
        read.connect_clicked(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| {
                ui.restore_chat_focus.set(true);
                ui.chat_menu.popdown();
                ui.change_dialog_unread(id, !unread, through);
            }
        ));
        menu.append(&read);
        self.restore_chat_focus.set(false);
        self.menu_chat.set(Some(id));
        self.chat_menu.set_child(Some(&menu));
        self.chat_menu
            .set_pointing_to(Some(&gtk::gdk::Rectangle::new(
                point.x() as i32,
                point.y() as i32,
                1,
                1,
            )));
        self.chat_menu.popup();
        pin.grab_focus();
    }

    fn pin_chat(self: &Rc<Self>, id: i64, pinned: bool, folder: Option<i32>) {
        if self.pinning_chats.borrow().contains(&id) {
            return;
        }
        if self.is_demo.get() {
            self.apply_chat_pin(id, pinned, folder);
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        self.pinning_chats.borrow_mut().insert(id);
        self.job(
            async move { client.set_chat_pinned(id, pinned, folder).await },
            move |ui, result| {
                ui.pinning_chats.borrow_mut().remove(&id);
                match result {
                    Ok(()) => {
                        ui.apply_chat_pin(id, pinned, folder);
                        ui.refresh_chats();
                        ui.refresh_folders();
                    }
                    Err(error) => {
                        ui.toast(&format!("Не удалось изменить закрепление чата. {error}"))
                    }
                }
            },
        );
    }

    fn apply_chat_pin(self: &Rc<Self>, id: i64, pinned: bool, folder: Option<i32>) {
        self.chat_settings_revision
            .set(self.chat_settings_revision.get().wrapping_add(1));
        if let Some(folder) = folder {
            if let Some(folder) = self
                .folders
                .borrow_mut()
                .iter_mut()
                .find(|item| item.id == folder)
            {
                folder.pinned.retain(|item| *item != id);
                if pinned {
                    folder.pinned.insert(0, id);
                }
            }
        } else {
            let mut chats = self.chats.borrow_mut();
            if let Some(index) = chats.iter().position(|chat| chat.id == id) {
                let mut chat = chats.remove(index);
                chat.pinned = pinned;
                chats.insert(0, chat);
            }
        }
        self.render_chats();
        self.toast(if pinned {
            "Чат закреплён"
        } else {
            "Чат откреплён"
        });
    }

    fn update_archive_button(&self) {
        let chats = self.chats.borrow();
        let chat = self.selection.get().and_then(|id| {
            chats
                .iter()
                .find(|chat| chat.id == id && !chat.saved_messages)
        });
        self.archive_chat.set_visible(chat.is_some());
        if let Some(chat) = chat {
            let title = if chat.archived {
                "Вернуть из архива"
            } else {
                "Переместить в архив"
            };
            self.archive_chat.set_tooltip_text(Some(title));
            self.archive_chat
                .update_property(&[gtk::accessible::Property::Label(title)]);
            self.archive_chat
                .set_sensitive(!self.moving_chats.borrow().contains(&chat.id));
        }
    }

    fn move_selected_chat(self: &Rc<Self>) {
        let Some(id) = self.selection.get() else {
            return;
        };
        let Some(archived) = self
            .chats
            .borrow()
            .iter()
            .find(|chat| chat.id == id && !chat.saved_messages)
            .map(|chat| !chat.archived)
        else {
            return;
        };
        if self.is_demo.get() {
            self.apply_archive(id, archived);
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        if !self.moving_chats.borrow_mut().insert(id) {
            return;
        }
        self.update_archive_button();
        self.job(
            async move { client.set_archived(id, archived).await },
            move |ui, result| {
                ui.moving_chats.borrow_mut().remove(&id);
                match result {
                    Ok(()) => {
                        ui.apply_archive(id, archived);
                        ui.refresh_chats();
                    }
                    Err(error) => {
                        ui.update_archive_button();
                        ui.toast(&format!("Не удалось переместить чат. {error}"));
                    }
                }
            },
        );
    }

    fn apply_archive(self: &Rc<Self>, id: i64, archived: bool) {
        self.chat_settings_revision
            .set(self.chat_settings_revision.get().wrapping_add(1));
        if let Some(chat) = self
            .chats
            .borrow_mut()
            .iter_mut()
            .find(|chat| chat.id == id)
        {
            chat.archived = archived;
            chat.pinned = false;
        }
        if self.selection.get() == Some(id) {
            self.archive.set_active(archived);
            self.split.set_show_content(true);
        }
        self.render_chats();
        self.toast(if archived {
            "Чат перемещён в архив"
        } else {
            "Чат возвращён из архива"
        });
    }

    fn update_mute_button(&self) {
        let chats = self.chats.borrow();
        let chat = self.selection.get().and_then(|id| {
            chats
                .iter()
                .find(|chat| chat.id == id && !chat.saved_messages)
        });
        self.mute_chat.set_visible(chat.is_some());
        if let Some(chat) = chat {
            let (icon, title) = if chat.muted {
                ("notifications-disabled-symbolic", "Включить уведомления")
            } else {
                (
                    "preferences-system-notifications-symbolic",
                    "Выключить уведомления",
                )
            };
            self.mute_chat.set_icon_name(icon);
            self.mute_chat.set_tooltip_text(Some(title));
            self.mute_chat
                .update_property(&[gtk::accessible::Property::Label(title)]);
            self.mute_chat
                .set_sensitive(!self.muting_chats.borrow().contains(&chat.id));
        }
    }

    fn toggle_chat_notifications(self: &Rc<Self>) {
        let Some(id) = self.selection.get() else {
            return;
        };
        let Some(muted) = self
            .chats
            .borrow()
            .iter()
            .find(|chat| chat.id == id && !chat.saved_messages)
            .map(|chat| !chat.muted)
        else {
            return;
        };
        if self.is_demo.get() {
            self.apply_muted(id, muted);
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        if !self.muting_chats.borrow_mut().insert(id) {
            return;
        }
        self.update_mute_button();
        self.job(
            async move { client.set_muted(id, muted).await },
            move |ui, result| {
                ui.muting_chats.borrow_mut().remove(&id);
                match result {
                    Ok(()) => {
                        ui.apply_muted(id, muted);
                        ui.refresh_chats();
                    }
                    Err(error) => {
                        ui.update_mute_button();
                        ui.toast(&format!("Не удалось изменить уведомления. {error}"));
                    }
                }
            },
        );
    }

    fn apply_muted(self: &Rc<Self>, id: i64, muted: bool) {
        self.chat_settings_revision
            .set(self.chat_settings_revision.get().wrapping_add(1));
        if let Some(chat) = self
            .chats
            .borrow_mut()
            .iter_mut()
            .find(|chat| chat.id == id)
        {
            chat.muted = muted;
        }
        if muted {
            self.withdraw_chat_notification(id);
        }
        self.render_chats();
        self.toast(if muted {
            "Уведомления чата выключены"
        } else {
            "Уведомления чата включены"
        });
    }

    fn chat_dialog_open(&self) -> bool {
        [
            &self.message_search,
            &self.profile_dialog,
            &self.contacts_dialog,
        ]
        .iter()
        .any(|dialog| dialog.upgrade().is_some_and(|dialog| dialog.is_visible()))
    }

    fn open_contacts(self: &Rc<Self>) {
        if (!self.is_demo.get() && self.login_step.get() != LoginStep::Authorized)
            || self.chat_dialog_open()
        {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        let dialog = crate::contacts::open(
            &window,
            crate::contacts::ContactContext {
                runtime: self.runtime.clone(),
                load: Box::new(move |refresh| {
                    let client = client.clone();
                    Box::pin(async move {
                        if demo {
                            Ok(Arc::new(demo::contacts()))
                        } else {
                            client
                                .context("Нет подключения к Telegram")?
                                .contacts(refresh)
                                .await
                        }
                    })
                }),
                avatar: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |avatar, photo| {
                        ui.load_avatar(avatar, ChatImage::Avatar(photo));
                    }
                )),
                selected: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |contact: Contact| {
                        let chat = contact.chat();
                        if let Some(known) = ui
                            .chats
                            .borrow_mut()
                            .iter_mut()
                            .find(|chat| chat.id == contact.id)
                        {
                            known.title = chat.title.clone();
                            known.avatar = chat.avatar;
                            known.kind = chat.kind;
                        }
                        ui.external_chat.replace(Some(chat));
                        ui.render_chats();
                        ui.show_chat(contact.id, None);
                    }
                )),
                ready: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || ui.focus_composer()
                )),
            },
        );
        self.contacts_dialog.set(Some(&dialog));
    }

    fn open_chat_profile(self: &Rc<Self>) {
        let Some(id) = self.selection.get() else {
            return;
        };
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if self.chat_dialog_open() {
            return;
        }
        let client = self.client.borrow().clone();
        let demo = self.is_demo.get();
        let dialog = crate::profile::open(
            &window,
            crate::profile::ProfileContext {
                runtime: self.runtime.clone(),
                load: Box::new(move || {
                    let client = client.clone();
                    Box::pin(async move {
                        if demo {
                            demo::peer_profile(id).ok_or_else(|| anyhow::anyhow!("Чат не найден"))
                        } else {
                            let client = client.context("Нет подключения к Telegram")?;
                            client.peer_profile(id).await
                        }
                    })
                }),
                avatar: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |avatar, photo| ui.load_avatar(avatar, ChatImage::Avatar(photo))
                )),
            },
        );
        self.profile_dialog.set(Some(&dialog));
    }

    fn open_message_search(self: &Rc<Self>, scoped: bool) {
        if (!self.is_demo.get() && self.login_step.get() != LoginStep::Authorized)
            || self.chat_dialog_open()
        {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let chat = self.selection.get().and_then(|id| {
            self.chats
                .borrow()
                .iter()
                .find(|chat| chat.id == id)
                .cloned()
                .or_else(|| {
                    self.external_chat
                        .borrow()
                        .clone()
                        .filter(|chat| chat.id == id)
                })
        });
        let demo_hits = if self.is_demo.get() {
            let history = self.demo_messages.borrow();
            let mut hits: Vec<_> = self
                .chats
                .borrow()
                .iter()
                .flat_map(|chat| {
                    history
                        .get(&chat.id)
                        .cloned()
                        .unwrap_or_else(|| demo::history(chat.id))
                        .into_iter()
                        .map(move |message| SearchHit {
                            chat: chat.clone(),
                            message,
                        })
                })
                .collect();
            hits.sort_by_key(|hit| std::cmp::Reverse((hit.message.timestamp, hit.message.id)));
            hits
        } else {
            Vec::new()
        };
        let dialog = crate::search::open(
            &window,
            crate::search::SearchContext {
                runtime: self.runtime.clone(),
                client: self.client.borrow().clone(),
                demo_hits,
                chat,
                selected: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |hit: SearchHit| {
                        let id = hit.chat.id;
                        ui.external_chat.replace(Some(hit.chat));
                        ui.show_chat(id, Some(hit.message.id));
                    }
                )),
            },
            scoped,
        );
        self.message_search.set(Some(&dialog));
    }

    fn show_chat(self: &Rc<Self>, id: i64, anchor: Option<i32>) {
        let Some(chat) = self
            .chats
            .borrow()
            .iter()
            .find(|chat| chat.id == id)
            .cloned()
            .or_else(|| {
                self.external_chat
                    .borrow()
                    .as_ref()
                    .filter(|chat| chat.id == id)
                    .cloned()
            })
        else {
            return;
        };
        self.withdraw_chat_notification(id);
        if self.chats.borrow().iter().any(|item| item.id == id) {
            let in_folder = !self.archive.is_active()
                && self
                    .selected_folder()
                    .is_some_and(|folder| folder.contains(&chat));
            if !in_folder {
                self.folder_picker.set_selected(0);
                self.archive.set_active(chat.archived);
            }
        }
        self.title.set_title(&chat.title);
        self.title
            .set_subtitle(if chat.broadcast { "Канал" } else { "" });
        self.split.set_show_content(true);
        let client = self.client.borrow().clone();
        if self.unread_overrides.borrow_mut().remove(&id)
            && let Some(client) = &client
        {
            client.begin_read_change(id);
        }

        let revision = client
            .as_ref()
            .map_or_else(HistoryRevision::default, |client| {
                client.history_revision(id)
            });
        let now = Instant::now();
        if self.selection.get() == Some(id)
            && self.loaded.get()
            && anchor.is_none()
            && self.search_anchor.get().is_none()
        {
            self.focus_composer();
            if self.is_demo.get()
                || self.history_refreshed.get().is_some_and(|refreshed| {
                    self.history_revision
                        .get()
                        .is_fresh(refreshed, revision, now)
                })
            {
                self.mark_cached_read(&chat);
            } else {
                self.load_history(None, true);
            }
            return;
        }
        if let Some(previous) = self.selection.replace(Some(id)) {
            self.drafts
                .borrow_mut()
                .insert(previous, editor_text(&self.input.buffer()));
            if self.search_anchor.get().is_none()
                && let Some(refreshed) = self.history_refreshed.get()
            {
                self.history_cache.borrow_mut().store(
                    previous,
                    self.messages.take(),
                    self.has_older.get(),
                    self.history_revision.get(),
                    refreshed,
                    now,
                );
            }
        }
        self.update_archive_button();
        self.update_mute_button();
        self.search_in_chat.set_visible(true);
        self.profile_button.set_sensitive(true);
        self.message_menu.popdown();
        self.generation.set(self.generation.get().wrapping_add(1));
        self.search_anchor.set(anchor);
        self.scroll_to_search_result.set(anchor.is_some());
        self.latest.set_visible(anchor.is_some());
        self.history_refresh.set(false);
        self.latest_history_pending.set(None);
        self.loaded.set(false);
        self.has_older.set(false);
        self.messages.borrow_mut().clear();
        self.revealed_spoilers.borrow_mut().clear();
        self.history_refreshed.set(None);
        self.content.set_visible_child_name("conversation");
        let buffer = self.input.buffer();
        if let Some(draft) = self.drafts.borrow().get(&id) {
            set_editor_text(&buffer, draft);
        } else {
            buffer.set_text("");
        }
        buffer.place_cursor(&buffer.end_iter());
        self.input.set_sensitive(!chat.broadcast);
        self.input_placeholder.set_text(if chat.broadcast {
            "Канал только для чтения"
        } else {
            "Сообщение"
        });
        self.update_send_button();
        let cached = if anchor.is_none() {
            self.history_cache.borrow_mut().restore(id, now)
        } else {
            None
        };
        if let Some(cached) = cached {
            let fresh = cached.revision.is_fresh(cached.refreshed, revision, now);
            self.history_revision.set(cached.revision);
            self.history_refreshed.set(Some(cached.refreshed));
            self.has_older.set(cached.has_older);
            self.messages.replace(cached.messages);
            self.loaded.set(true);
            self.render_history(true);
            if fresh {
                self.mark_cached_read(&chat);
            } else {
                self.load_history(None, true);
            }
        } else if self.is_demo.get() {
            let (messages, has_older, _) = self.demo_history(id, None, anchor);
            self.messages
                .borrow_mut()
                .extend(messages.into_iter().map(|message| (message.id, message)));
            if let Some(chat) = self
                .chats
                .borrow_mut()
                .iter_mut()
                .find(|chat| chat.id == id)
            {
                chat.unread = 0;
                chat.unread_mark = false;
            }
            self.loaded.set(true);
            self.has_older.set(has_older);
            self.history_revision.set(revision);
            self.history_refreshed.set(Some(now));
            self.render_chats();
            self.render_history(anchor.is_none());
        } else {
            self.render_history(true);
            self.load_history(None, true);
        }
        if anchor.is_none() {
            self.focus_composer();
        }
    }

    fn read_request(&self, id: i64) -> Option<ReadRequest> {
        if self.unread_overrides.borrow().contains(&id) {
            return None;
        }
        self.client.borrow().as_ref().map(|client| ReadRequest {
            revision: client.read_revision(id),
            clear_mark: self.reading_chats.borrow().contains(&id)
                || self
                    .chats
                    .borrow()
                    .iter()
                    .any(|chat| chat.id == id && chat.unread_mark),
        })
    }

    fn mark_cached_read(self: &Rc<Self>, chat: &Chat) {
        if (!chat.is_unread() && !self.reading_chats.borrow().contains(&chat.id))
            || self.unread_overrides.borrow().contains(&chat.id)
        {
            return;
        }
        if self.is_demo.get() {
            self.apply_dialog_read(chat.id, false, chat.last_message_id);
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let Some(read) = self.read_request(chat.id) else {
            return;
        };
        let max_id = self
            .messages
            .borrow()
            .last_key_value()
            .map_or(0, |(&id, _)| id);
        let id = chat.id;
        self.job(
            async move { client.mark_read_through(id, max_id, read).await },
            |ui, result| {
                if matches!(result, Ok(true)) {
                    ui.refresh_chats();
                }
            },
        );
    }

    fn change_dialog_unread(self: &Rc<Self>, id: i64, unread: bool, through: i32) {
        if self.reading_chats.borrow().contains(&id) {
            return;
        }
        self.unread_overrides.borrow_mut().insert(id);
        if self.is_demo.get() {
            self.apply_dialog_read(id, unread, through);
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let revision = client.begin_read_change(id);
        let owner = Arc::downgrade(&client);
        self.reading_chats.borrow_mut().insert(id);
        self.job(
            async move {
                client
                    .set_dialog_unread(id, unread, through, revision)
                    .await
            },
            move |ui, result| {
                let owner_matches = owner.upgrade().is_some_and(|owner| {
                    ui.client
                        .borrow()
                        .as_ref()
                        .is_some_and(|client| Arc::ptr_eq(&owner, client))
                });
                if !owner_matches {
                    return;
                }
                ui.reading_chats.borrow_mut().remove(&id);
                let current = ui
                    .client
                    .borrow()
                    .as_ref()
                    .is_some_and(|client| client.read_revision(id) == revision);
                if current {
                    match result {
                        Ok(true) => ui.apply_dialog_read(id, unread, through),
                        Ok(false) => {}
                        Err(error) => {
                            ui.toast(&format!("Не удалось изменить отметку чата. {error}"))
                        }
                    }
                }
                ui.refresh_chats();
            },
        );
    }

    fn apply_dialog_read(self: &Rc<Self>, id: i64, unread: bool, through: i32) {
        self.chat_settings_revision
            .set(self.chat_settings_revision.get().wrapping_add(1));
        if !unread {
            self.unread_overrides.borrow_mut().remove(&id);
        }
        let mut withdraw = false;
        if let Some(chat) = self
            .chats
            .borrow_mut()
            .iter_mut()
            .find(|chat| chat.id == id)
        {
            if unread {
                chat.unread_mark = true;
            } else {
                chat.read_through(through);
            }
            withdraw = !chat.is_unread();
        }
        if withdraw {
            self.withdraw_chat_notification(id);
        }
        self.render_chats();
    }

    fn demo_history(
        &self,
        chat: i64,
        before: Option<i32>,
        anchor: Option<i32>,
    ) -> (Vec<Message>, bool, bool) {
        let mut history = self.demo_messages.borrow_mut();
        let messages = history.entry(chat).or_insert_with(|| demo::history(chat));
        let end = if let Some(before) = before {
            messages.partition_point(|message| message.id < before)
        } else if let Some(anchor) = anchor {
            (messages.partition_point(|message| message.id <= anchor) + HISTORY_PAGE_SIZE / 2)
                .min(messages.len())
        } else {
            messages.len()
        };
        let start = end.saturating_sub(HISTORY_PAGE_SIZE);
        (messages[start..end].to_vec(), start > 0, false)
    }

    fn load_older_near_top(self: &Rc<Self>) {
        if self.adjusting_history.get()
            || self.history_refresh.get()
            || !self.has_older.get()
            || !self.scroll.is_mapped()
        {
            return;
        }
        let adjustment = self.scroll.vadjustment();
        if adjustment.value() - adjustment.lower() <= 180.0
            && let Some(before) = self.messages.borrow().first_key_value().map(|(&id, _)| id)
        {
            self.load_history(Some(before), false);
        }
    }

    fn focus_composer(self: &Rc<Self>) {
        if !self.input.is_sensitive() || self.input.grab_focus() {
            return;
        }
        let generation = self.generation.get();
        after_layout(
            &self.input,
            glib::clone!(
                #[weak(rename_to = ui)]
                self,
                move || {
                    if ui.generation.get() == generation
                        && (!ui.split.is_collapsed() || ui.split.shows_content())
                    {
                        ui.input.grab_focus();
                    }
                }
            ),
        );
    }

    fn load_history(self: &Rc<Self>, before: Option<i32>, mark_read: bool) {
        let Some(id) = self.selection.get() else {
            return;
        };
        let client = self.client.borrow().clone();
        let demo = self.is_demo.get();
        if client.is_none() && !demo {
            return;
        }
        if self.history_refresh.replace(true) {
            if before.is_none() {
                self.latest_history_pending.set(Some(
                    mark_read || self.latest_history_pending.get() == Some(true),
                ));
            }
            return;
        }
        let generation = self.generation.get();
        let anchor = self.search_anchor.get();
        let revision = client
            .as_ref()
            .map_or_else(HistoryRevision::default, |client| {
                client.history_revision(id)
            });
        let demo_page = demo.then(|| self.demo_history(id, before, anchor));
        let requested_through = self.messages.borrow().last_key_value().map(|(&id, _)| id);
        self.older.set_sensitive(false);
        let has_unread = self
            .chats
            .borrow()
            .iter()
            .any(|chat| chat.id == id && chat.is_unread());
        let read = mark_read.then(|| self.read_request(id)).flatten();
        self.job(
            async move {
                if let Some(page) = demo_page {
                    return Ok(page);
                }
                let client = client.expect("Telegram history requires a client");
                if before.is_none()
                    && let Some(anchor) = anchor
                {
                    let (messages, older) = client.history_around(id, anchor).await?;
                    Ok((messages, older, false))
                } else {
                    client.history(id, before, read).await
                }
            },
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
                        let previous_position =
                            before.map(|_| (adjustment.upper(), adjustment.value()));
                        let mut current = ui.messages.borrow_mut();
                        if before.is_some() {
                            current
                                .extend(messages.into_iter().map(|message| (message.id, message)));
                            ui.has_older.set(has_older);
                        } else {
                            let previous = ui.history_revision.replace(revision);
                            if anchor.is_none() {
                                ui.history_refreshed.set(Some(Instant::now()));
                            }
                            let older_loaded = merge_latest_history(
                                &mut current,
                                messages,
                                has_older && revision.can_keep_older(previous),
                                requested_through,
                            );
                            if !older_loaded {
                                ui.has_older.set(has_older);
                            }
                        }
                        drop(current);
                        ui.render_history_position(
                            anchor.is_none() && before.is_none() && (!was_loaded || at_bottom),
                            previous_position,
                        );
                        if marked && has_unread {
                            ui.refresh_chats();
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
        self.render_history_position(bottom, None);
    }

    fn render_history_position(
        self: &Rc<Self>,
        bottom: bool,
        previous_position: Option<(f64, f64)>,
    ) {
        self.adjusting_history.set(true);
        while let Some(child) = self.history.first_child() {
            self.history.remove(&child);
        }
        if self.has_older.get() {
            self.history.append(&self.older);
        }
        let messages = self.messages.borrow();
        self.revealed_spoilers
            .borrow_mut()
            .retain(|id, _| messages.contains_key(id));
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
        let mut remaining = messages.values().peekable();
        while let Some(message) = remaining.next() {
            let mut group = vec![message];
            if let Some(album) = message.album {
                while let Some(item) = remaining.next_if(|item| item.album == Some(album)) {
                    group.push(item);
                }
            }
            let selected = group
                .iter()
                .any(|item| self.search_anchor.get() == Some(item.id));
            let timestamp = glib::DateTime::from_unix_local(message.timestamp).ok();
            let day = timestamp.as_ref().and_then(|time| time.format("%x").ok());
            if day != previous_day {
                let separator = label(day.as_deref().unwrap_or(""), "dim-label");
                separator.set_halign(gtk::Align::Center);
                self.history.append(&separator);
                previous_day = day;
            }
            let bubble = gtk::Box::new(gtk::Orientation::Vertical, 4);
            bubble.set_vexpand(false);
            bubble.set_valign(gtk::Align::Start);
            bubble.add_css_class("message-bubble");
            bubble.set_widget_name(&format!("message-{}", message.id));
            if selected {
                bubble.add_css_class("search-result");
            }
            if message.outgoing {
                bubble.add_css_class("outgoing");
            }
            bubble.set_halign(if message.outgoing {
                gtk::Align::End
            } else {
                gtk::Align::Start
            });
            let author = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let avatar = adw::Avatar::new(24, Some(&message.sender), true);
            if let Some(photo) = message.sender_avatar {
                self.load_avatar(&avatar, ChatImage::Avatar(photo));
            }
            author.append(&avatar);
            let name = label(&message.sender, "message-name");
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            author.append(&name);
            bubble.append(&author);
            if message.forwarded {
                bubble.append(&label("Пересланное сообщение", "dim-label"));
            }
            if group.iter().any(|item| item.pinned) {
                bubble.append(&label("Закреплено", "dim-label"));
            }
            if let Some(id) = message.reply_to {
                let original = messages.get(&id).map(Message::reply_preview);
                let preview = message.reply_preview.as_ref().or(original.as_ref());
                let (quote, author, excerpt) = reply_quote();
                author.set_text(
                    original
                        .as_ref()
                        .and_then(|item| item.sender.as_deref())
                        .or_else(|| preview.and_then(|item| item.sender.as_deref()))
                        .unwrap_or("Исходное сообщение"),
                );
                excerpt.set_text(preview.map_or("", |item| item.text.as_str()));
                excerpt.set_visible(preview.is_some());
                if let Some(chat) = self.selection.get() {
                    let button = gtk::Button::builder().child(&quote).build();
                    button.add_css_class("flat");
                    button.add_css_class("reply-link");
                    button.connect_clicked(glib::clone!(
                        #[weak(rename_to = ui)]
                        self,
                        move |_| ui.open_reply(chat, id)
                    ));
                    bubble.append(&button);
                }
            } else if let Some(preview) = &message.reply_preview {
                let (quote, author, excerpt) = reply_quote();
                author.set_text("Цитата");
                excerpt.set_text(&preview.text);
                bubble.append(&quote);
            }
            let is_album = group.len() > 1;
            let photo_album = is_album
                && group.iter().all(|item| {
                    item.photo_preview || matches!(item.playback, Some(PlayableMedia::Video { .. }))
                });
            if let Some(chat) = self.selection.get() {
                if photo_album {
                    let grid = gtk::FlowBox::builder()
                        .selection_mode(gtk::SelectionMode::None)
                        .max_children_per_line(2)
                        .row_spacing(4)
                        .column_spacing(4)
                        .build();
                    for item in &group {
                        if let Some(widget) = self.attachment_widget(chat, item, true) {
                            grid.insert(&widget, -1);
                        }
                    }
                    bubble.append(&grid);
                    let mut captions = group.iter().filter(|item| !item.text.is_empty());
                    if let Some(item) = captions.next()
                        && captions.next().is_none()
                    {
                        self.append_message_text(&bubble, item);
                    }
                } else {
                    for item in &group {
                        if let Some(widget) = self.attachment_widget(chat, item, is_album) {
                            bubble.append(&widget);
                        }
                        self.append_message_text(&bubble, item);
                    }
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
            let footer = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            footer.set_halign(gtk::Align::End);
            let mut reaction_counts = BTreeMap::<&str, (i32, bool)>::new();
            for reaction in group.iter().flat_map(|item| &item.reactions) {
                let (count, chosen) = reaction_counts.entry(&reaction.emoji).or_default();
                *count += reaction.count;
                *chosen |= reaction.chosen;
            }
            if !reaction_counts.is_empty() {
                let reactions = gtk::FlowBox::builder()
                    .selection_mode(gtk::SelectionMode::None)
                    .column_spacing(6)
                    .build();
                for (emoji, (count, chosen)) in reaction_counts {
                    let text = format!("{emoji} {count}");
                    reactions.insert(
                        &label(&text, if chosen { "accent" } else { "dim-label" }),
                        -1,
                    );
                }
                bubble.append(&reactions);
            }
            if let Some(chat) = self.selection.get() {
                self.connect_message_menu(&bubble, chat, message.id);
            }
            footer.append(&time);
            bubble.append(&footer);
            self.history.append(&bubble);
            if selected && self.scroll_to_search_result.replace(false) {
                let weak = bubble.downgrade();
                let generation = self.generation.get();
                after_layout(
                    &self.history,
                    glib::clone!(
                        #[weak(rename_to = ui)]
                        self,
                        move || {
                            if ui.generation.get() == generation
                                && let Some(bubble) = weak.upgrade()
                                && let Some(bounds) = bubble.compute_bounds(&ui.history)
                            {
                                let adjustment = ui.scroll.vadjustment();
                                adjustment.set_value(
                                    f64::from(bounds.y()) - adjustment.page_size() / 3.0,
                                );
                            }
                        }
                    ),
                );
            }
        }
        let adjustment = self.scroll.vadjustment();
        let generation = self.generation.get();
        after_layout(
            &self.history,
            glib::clone!(
                #[weak(rename_to = ui)]
                self,
                move || {
                    if ui.generation.get() == generation {
                        if bottom {
                            adjustment.set_value(adjustment.upper());
                        } else if let Some((old_height, old_position)) = previous_position {
                            adjustment.set_value(old_position + adjustment.upper() - old_height);
                        }
                        ui.adjusting_history.set(false);
                        if adjustment.upper() - adjustment.page_size() <= adjustment.lower() {
                            ui.load_older_near_top();
                        }
                    }
                }
            ),
        );
    }

    fn message_bubble(&self, id: i32) -> Option<gtk::Widget> {
        let messages = self.messages.borrow();
        let first = messages
            .get(&id)
            .and_then(|message| message.album)
            .map_or(id, |album| {
                messages
                    .range(..id)
                    .rev()
                    .take_while(|(_, item)| item.album == Some(album))
                    .last()
                    .map_or(id, |(&id, _)| id)
            });
        let name = format!("message-{first}");
        let mut child = self.history.first_child();
        while let Some(candidate) = child {
            if candidate.widget_name() == name {
                return Some(candidate);
            }
            child = candidate.next_sibling();
        }
        None
    }

    fn open_reply(self: &Rc<Self>, chat: i64, id: i32) {
        if self.selection.get() != Some(chat) {
            return;
        }
        if !self.messages.borrow().contains_key(&id) {
            self.show_chat(chat, Some(id));
            return;
        }
        let Some(bubble) = self.message_bubble(id) else {
            return;
        };
        if let Some(bounds) = bubble.compute_bounds(&self.history) {
            let adjustment = self.scroll.vadjustment();
            adjustment.set_value(f64::from(bounds.y()) - adjustment.page_size() / 3.0);
            bubble.grab_focus();
            bubble.add_css_class("reply-target");
            glib::timeout_add_local_once(
                Duration::from_millis(850),
                glib::clone!(
                    #[weak]
                    bubble,
                    move || bubble.remove_css_class("reply-target")
                ),
            );
        }
    }

    fn attachment_widget(
        self: &Rc<Self>,
        chat: i64,
        message: &Message,
        in_album: bool,
    ) -> Option<gtk::Widget> {
        let id = message.id;
        let playback = message.playback.filter(|_| !message.ephemeral);
        let visual = message.photo_preview || matches!(playback, Some(PlayableMedia::Video { .. }));
        let preview = gtk::Stack::new();
        preview.set_vexpand(false);
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .build();
        picture.update_property(&[gtk::accessible::Property::Label("Миниатюра вложения")]);
        let fallback = if visual {
            let icon = gtk::Image::from_icon_name(if playback.is_some() {
                "media-playback-start-symbolic"
            } else {
                "image-x-generic-symbolic"
            });
            icon.set_pixel_size(48);
            let (width, height) = if in_album { (120, 92) } else { (260, 128) };
            icon.set_size_request(width, height);
            icon.upcast::<gtk::Widget>()
        } else if let Some(name) = &message.attachment {
            let file = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let name = label(name, "");
            name.set_max_width_chars(32);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            file.append(&gtk::Image::from_icon_name("mail-attachment-symbolic"));
            file.append(&name);
            file.upcast()
        } else {
            return None;
        };
        let item_name = format!("attachment-{id}");
        preview.set_hhomogeneous(false);
        preview.set_vhomogeneous(false);
        preview.add_child(&fallback);
        preview.add_child(&picture);
        preview.set_visible_child(&fallback);
        if let Some(media) = message.thumbnail {
            let message_photo = visual;
            self.load_chat_image(
                ChatImage::attachment(chat, id, media),
                glib::clone!(
                    #[weak]
                    picture,
                    #[weak]
                    preview,
                    move |texture: &gtk::gdk::Texture| {
                        let scale = if in_album && message_photo { 2 } else { 1 };
                        picture.set_size_request(
                            (texture.width() / scale).max(1),
                            (texture.height() / scale).max(1),
                        );
                        picture.set_paintable(Some(texture));
                        preview.set_visible_child(&picture);
                    }
                ),
            );
        }
        if message.photo_preview || playback.is_some() {
            let button = gtk::Button::builder().child(&preview).build();
            button.add_css_class("flat");
            button.add_css_class("attachment-preview");
            button.set_vexpand(false);
            button.set_valign(gtk::Align::Start);
            button.update_property(&[gtk::accessible::Property::Label(
                playback.map_or("Открыть фото", PlayableMedia::open_label),
            )]);
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = ui)]
                self,
                move |_| {
                    if let Some(media) = playback {
                        ui.open_playback(chat, id, media);
                    } else {
                        ui.open_photo_preview(chat, id);
                    }
                }
            ));
            button.set_widget_name(&item_name);
            Some(button.upcast())
        } else {
            preview.set_widget_name(&item_name);
            preview.set_focusable(in_album);
            Some(preview.upcast())
        }
    }

    fn load_avatar(self: &Rc<Self>, avatar: &adw::Avatar, image: ChatImage) {
        self.load_chat_image(
            image,
            glib::clone!(
                #[weak]
                avatar,
                move |texture: &gtk::gdk::Texture| avatar.set_custom_image(Some(texture))
            ),
        );
    }

    fn load_chat_image(
        self: &Rc<Self>,
        image: ChatImage,
        show: impl FnOnce(&gtk::gdk::Texture) + 'static,
    ) {
        if self.is_demo.get() {
            let texture = self.demo_image.get_or_init(demo_texture);
            if matches!(image, ChatImage::Attachment { .. }) {
                if let Some(thumbnail) = scaled_thumbnail(texture.upcast_ref()) {
                    show(&thumbnail);
                }
            } else {
                show(texture.upcast_ref());
            }
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let mut cache = self.images.borrow_mut();
        match cache.get_mut(&image) {
            Some(ImageCacheEntry::Ready(texture)) => show(texture),
            Some(ImageCacheEntry::Loading(targets)) => targets.push(Box::new(show)),
            Some(ImageCacheEntry::Unavailable(retry_after)) if Instant::now() < *retry_after => {}
            _ => {
                if cache.len() >= 256
                    && let Some(key) = cache.iter().find_map(|(key, entry)| {
                        (!matches!(entry, ImageCacheEntry::Loading(_))).then_some(*key)
                    })
                {
                    cache.remove(&key);
                }
                cache.insert(image, ImageCacheEntry::Loading(vec![Box::new(show)]));
                drop(cache);
                self.job(
                    async move { client.chat_image(image).await },
                    move |ui, result| {
                        let mut cache = ui.images.borrow_mut();
                        let Some(ImageCacheEntry::Loading(targets)) = cache.remove(&image) else {
                            return;
                        };
                        let texture = result.ok().and_then(|bytes| {
                            let texture =
                                gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
                                    .ok()?;
                            match image {
                                ChatImage::Avatar(_) => Some(texture),
                                ChatImage::Attachment { .. } => scaled_thumbnail(&texture),
                            }
                        });
                        if let Some(texture) = texture {
                            for show in targets {
                                show(&texture);
                            }
                            cache.insert(image, ImageCacheEntry::Ready(texture));
                        } else {
                            cache.insert(
                                image,
                                ImageCacheEntry::Unavailable(
                                    Instant::now() + Duration::from_secs(30),
                                ),
                            );
                        }
                    },
                );
            }
        }
    }

    fn connect_message_menu(self: &Rc<Self>, bubble: &gtk::Box, chat: i64, id: i32) {
        bubble.set_focusable(true);
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_SECONDARY);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        click.connect_pressed(|gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        click.connect_released(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            #[weak]
            bubble,
            move |_, _, x, y| {
                let item = attachment_message_id(bubble.pick(x, y, gtk::PickFlags::DEFAULT));
                ui.open_message_menu(&bubble, chat, item.unwrap_or(id), x, y);
            }
        ));
        bubble.add_controller(click);

        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            #[weak]
            bubble,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, modifiers| {
                if key == gtk::gdk::Key::Menu
                    || (key == gtk::gdk::Key::F10
                        && modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK))
                {
                    let focused = bubble
                        .root()
                        .and_then(|root| gtk::prelude::RootExt::focus(&root));
                    let item = attachment_message_id(focused.clone());
                    let (x, y) = focused
                        .and_then(|widget| widget.compute_bounds(&bubble))
                        .map(|bounds| {
                            (
                                f64::from(bounds.x() + bounds.width() / 2.0),
                                f64::from(bounds.y() + bounds.height() / 2.0),
                            )
                        })
                        .unwrap_or((
                            f64::from(bubble.width()) / 2.0,
                            f64::from(bubble.height()) / 2.0,
                        ));
                    ui.open_message_menu(&bubble, chat, item.unwrap_or(id), x, y);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        bubble.add_controller(keys);
    }

    fn open_message_menu(self: &Rc<Self>, bubble: &gtk::Box, chat: i64, id: i32, x: f64, y: f64) {
        if self.selection.get() != Some(chat) {
            return;
        }
        let Some(message) = self.messages.borrow().get(&id).cloned() else {
            return;
        };
        let Some(parent) = self.message_menu.parent() else {
            return;
        };
        let Some(point) =
            bubble.compute_point(&parent, &gtk::graphene::Point::new(x as f32, y as f32))
        else {
            return;
        };
        let Some(menu) = self.message_menu_content(chat, &message) else {
            return;
        };
        self.menu_message.set(Some(id));
        self.message_menu.set_child(Some(&menu));
        self.message_menu
            .set_pointing_to(Some(&gtk::gdk::Rectangle::new(
                point.x() as i32,
                point.y() as i32,
                1,
                1,
            )));
        self.message_menu.popup();
        self.message_menu
            .child_focus(gtk::DirectionType::TabForward);
    }

    fn message_menu_content(self: &Rc<Self>, chat: i64, message: &Message) -> Option<gtk::Box> {
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let id = message.id;
        if message.can_reply {
            if self.input.is_sensitive() {
                let reply = Reply {
                    id,
                    preview: message.reply_preview(),
                };
                add_menu_button(
                    &menu,
                    "Ответить",
                    glib::clone!(
                        #[weak(rename_to = ui)]
                        self,
                        move || {
                            if ui.selection.get() == Some(chat) {
                                ui.replies.borrow_mut().insert(chat, reply.clone());
                                ui.update_send_button();
                                ui.input.grab_focus();
                            }
                        }
                    ),
                );
            }
            let title = if message.pinned {
                "Открепить"
            } else {
                "Закрепить"
            };
            let pinned = !message.pinned;
            add_menu_button(
                &menu,
                title,
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || ui.set_message_pinned(chat, id, pinned)
                ),
            );
            let reactions = gtk::Box::new(gtk::Orientation::Vertical, 2);
            for emoji in COMMON_REACTIONS.iter().copied().chain(
                message
                    .reactions
                    .iter()
                    .map(|reaction| reaction.emoji.as_str())
                    .filter(|emoji| !COMMON_REACTIONS.contains(emoji)),
            ) {
                let title = if message
                    .reactions
                    .iter()
                    .any(|item| item.emoji == emoji && item.chosen)
                {
                    format!("Убрать {emoji}")
                } else {
                    emoji.to_owned()
                };
                add_menu_button(
                    &reactions,
                    &title,
                    glib::clone!(
                        #[weak(rename_to = ui)]
                        self,
                        #[strong(rename_to = emoji)]
                        emoji.to_owned(),
                        move || ui.toggle_reaction(chat, id, emoji.clone())
                    ),
                );
            }
            let reaction_menu = gtk::Expander::builder()
                .label("Реакция")
                .child(&reactions)
                .build();
            menu.append(&reaction_menu);
        }
        if message.can_forward {
            add_menu_button(
                &menu,
                "Переслать",
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    #[strong(rename_to = message)]
                    message.clone(),
                    move || ui.forward_message(chat, message.clone())
                ),
            );
        }
        if message.can_edit {
            add_menu_button(
                &menu,
                "Редактировать",
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    #[strong(rename_to = text)]
                    message.formatted_text(),
                    move || ui.edit_message(chat, id, text.clone(), false)
                ),
            );
        }
        if let Some(media) = message.playback.filter(|_| !message.ephemeral) {
            add_menu_button(
                &menu,
                media.open_label(),
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || ui.open_playback(chat, id, media)
                ),
            );
        }
        if message.photo_preview {
            add_menu_button(
                &menu,
                "Открыть фото",
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || ui.open_photo_preview(chat, id)
                ),
            );
        }
        if let Some(name) = &message.attachment
            && (!self.is_demo.get() || demo::has_sample_attachment(chat, id))
        {
            add_menu_button(
                &menu,
                "Сохранить вложение",
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    #[strong(rename_to = name)]
                    name.clone(),
                    move || ui.save_file(chat, id, name.clone())
                ),
            );
        }
        if message.can_delete {
            add_menu_button(
                &menu,
                "Удалить",
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || ui.delete_message(chat, id)
                ),
            );
        }
        menu.first_child()?;
        Some(menu)
    }

    fn composer_text(&self) -> glib::GString {
        let buffer = self.input.buffer();
        buffer.text(&buffer.start_iter(), &buffer.end_iter(), true)
    }

    fn update_send_button(&self) {
        let text = self.composer_text();
        self.input_placeholder.set_visible(text.is_empty());
        let uploads = self.uploads.borrow();
        let pending = self.selection.get().and_then(|chat| uploads.get(&chat));
        self.upload_bar.set_visible(pending.is_some());
        if let Some(pending) = pending {
            self.upload_name.set_text(
                &pending
                    .iter()
                    .map(|upload| upload.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        let replies = self.replies.borrow();
        let reply = self.selection.get().and_then(|chat| replies.get(&chat));
        self.reply_bar.set_visible(reply.is_some());
        if let Some(reply) = reply {
            self.reply_sender.set_text(
                reply
                    .preview
                    .sender
                    .as_deref()
                    .unwrap_or("Исходное сообщение"),
            );
            self.reply_label.set_text(&reply.preview.text);
        }
        self.attach
            .set_sensitive(self.input.is_sensitive() && !self.sending.get());
        self.send.set_sensitive(
            self.input.is_sensitive()
                && (!text.trim().is_empty() || pending.is_some())
                && text.trim().chars().count() <= 4096
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
                UploadKind::File => "Выберите файлы",
                UploadKind::Photo => "Выберите фотографии",
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
            let result = dialog.open_multiple_future(Some(&window)).await;
            let (Some(ui), Ok(files)) = (weak.upgrade(), result) else {
                return;
            };
            if ui.window.upgrade().is_none() {
                return;
            }
            if files.n_items() as usize > MAX_ALBUM_ITEMS {
                ui.toast("Выберите до 10 вложений для одного сообщения");
                return;
            }
            let mut selected = Vec::new();
            for index in 0..files.n_items() {
                let Some(path) = files
                    .item(index)
                    .and_downcast::<gtk::gio::File>()
                    .and_then(|file| file.path())
                else {
                    ui.toast("Выберите локальные файлы");
                    return;
                };
                let name = attachment_name(
                    &path.file_name().unwrap_or_default().to_string_lossy(),
                    "Вложение",
                );
                selected.push(Upload { path, name, kind });
            }
            if !selected.is_empty() {
                ui.uploads.borrow_mut().insert(chat, selected);
            }
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
        let original = editor_text(&self.input.buffer());
        let text = original.trimmed();
        let uploads = self
            .uploads
            .borrow()
            .get(&chat)
            .cloned()
            .unwrap_or_default();
        let reply = self.replies.borrow().get(&chat).cloned();
        let reply_to = reply.as_ref().map(|reply| reply.id);
        if self.is_demo.get() {
            let id = self.next_demo_message();
            let message = Message {
                id,
                text: text.text,
                styles: text.spans,
                sender: demo::OWN_NAME.into(),
                sender_avatar: Some(demo::OWN_AVATAR),
                outgoing: true,
                timestamp: demo::now(),
                attachment: None,
                photo_preview: false,
                playback: None,
                thumbnail: None,
                reply_to,
                reply_preview: reply.map(|reply| reply.preview),
                can_reply: true,
                can_edit: uploads.is_empty(),
                can_delete: true,
                can_forward: true,
                forwarded: false,
                pinned: false,
                reactions: Vec::new(),
                edited: false,
                ephemeral: false,
                album: (uploads.len() > 1).then_some(i64::from(id)),
            };
            self.input.buffer().set_text("");
            self.uploads.borrow_mut().remove(&chat);
            self.replies.borrow_mut().remove(&chat);
            self.update_send_button();
            let messages = if uploads.is_empty() {
                vec![message]
            } else {
                uploads
                    .into_iter()
                    .enumerate()
                    .map(|(index, upload)| {
                        let first = index == 0;
                        let id = if first {
                            message.id
                        } else {
                            self.next_demo_message()
                        };
                        let photo = upload.kind == UploadKind::Photo;
                        Message {
                            id,
                            photo_preview: photo,
                            thumbnail: photo
                                .then_some(crate::model::AttachmentThumbnail::Photo(i64::from(id))),
                            text: if first {
                                message.text.clone()
                            } else {
                                String::new()
                            },
                            styles: if first {
                                message.styles.clone()
                            } else {
                                Vec::new()
                            },
                            attachment: Some(upload.name),
                            reply_to: message.reply_to.filter(|_| first),
                            reply_preview: message.reply_preview.clone().filter(|_| first),
                            ..message.clone()
                        }
                    })
                    .collect()
            };
            self.add_demo_messages(chat, messages);
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        self.sending.set(true);
        self.update_send_button();
        if !uploads.is_empty() {
            self.toast("Отправляем вложения…");
        }
        let request_uploads = uploads.clone();
        self.job(
            async move { client.send(chat, text, request_uploads, reply_to).await },
            move |ui, result| {
                ui.sending.set(false);
                match result {
                    Ok(mut messages) => {
                        for message in &mut messages {
                            if message.reply_to == reply_to && message.reply_preview.is_none() {
                                message.reply_preview =
                                    reply.as_ref().map(|reply| reply.preview.clone());
                            }
                        }
                        if ui.uploads.borrow().get(&chat) == Some(&uploads) {
                            ui.uploads.borrow_mut().remove(&chat);
                        }
                        if ui.replies.borrow().get(&chat) == reply.as_ref() {
                            ui.replies.borrow_mut().remove(&chat);
                        }
                        if ui.selection.get() == Some(chat) {
                            if editor_text(&ui.input.buffer()) == original {
                                ui.input.buffer().set_text("");
                            }
                            if ui.search_anchor.get().is_some() || !ui.loaded.get() {
                                ui.history_cache.borrow_mut().remove(chat);
                                ui.show_chat(chat, None);
                            } else {
                                ui.messages.borrow_mut().extend(
                                    messages.into_iter().map(|message| (message.id, message)),
                                );
                                ui.render_history(true);
                            }
                        } else {
                            ui.history_cache.borrow_mut().remove(chat);
                            if ui.drafts.borrow().get(&chat) == Some(&original) {
                                ui.drafts.borrow_mut().remove(&chat);
                            }
                        }
                        ui.refresh_chats();
                        if ui.selection.get() == Some(chat)
                            && ui.search_anchor.get().is_none()
                            && uploads.len() > 1
                        {
                            ui.load_history(None, false);
                        }
                    }
                    Err(error) => ui.toast(&format!("Не удалось отправить сообщение. {error}")),
                }
                ui.update_send_button();
            },
        );
    }

    fn edit_message(self: &Rc<Self>, chat: i64, id: i32, initial: FormattedText, retry: bool) {
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
        setup_editor(&editor);
        let buffer = editor.buffer();
        set_editor_text(&buffer, &initial);
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
        let update_save = Rc::new(glib::clone!(
            #[weak]
            dialog,
            move |buffer: &gtk::TextBuffer| {
                let text = editor_text(buffer).trimmed();
                dialog.set_response_enabled(
                    "save",
                    !text.text.is_empty()
                        && (retry || text != initial)
                        && text.text.chars().count() <= 4096,
                );
            }
        ));
        buffer.connect_changed({
            let update_save = update_save.clone();
            move |buffer| update_save(buffer)
        });
        for signal in ["apply-tag", "remove-tag"] {
            let update_save = update_save.clone();
            buffer.connect_local(signal, true, move |values| {
                if let Ok(buffer) = values[0].get::<gtk::TextBuffer>() {
                    update_save(&buffer);
                }
                None
            });
        }
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            if dialog.choose_future(Some(&window)).await != "save" {
                return;
            }
            let Some(ui) = weak.upgrade() else { return };
            let text = editor_text(&buffer).trimmed();
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
                            ui.edit_message(chat, id, text, true);
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

    fn add_demo_messages(self: &Rc<Self>, chat: i64, messages: Vec<Message>) {
        let Some(last) = messages.last() else { return };
        if !self.chats.borrow().iter().any(|item| item.id == chat)
            && let Some(contact) = self
                .external_chat
                .borrow()
                .as_ref()
                .filter(|item| item.id == chat)
        {
            self.chats.borrow_mut().insert(0, contact.clone());
        }
        self.history_cache.borrow_mut().remove(chat);
        if let Some(chat) = self
            .chats
            .borrow_mut()
            .iter_mut()
            .find(|item| item.id == chat)
        {
            chat.preview = messages
                .iter()
                .find(|message| !message.text.is_empty())
                .map(|message| message.preview_text().into_owned())
                .unwrap_or_else(|| "Вложение".into());
            chat.last_activity = last.timestamp;
            chat.last_message_id = last.id;
            chat.preview_sender = chat.group.then(|| last.sender.clone());
            chat.preview_image = last
                .thumbnail
                .map(|media| ChatImage::attachment(chat.id, last.id, media));
        }
        self.demo_messages
            .borrow_mut()
            .entry(chat)
            .or_insert_with(|| demo::history(chat))
            .extend(messages.iter().cloned());
        if self.selection.get() == Some(chat) {
            self.messages
                .borrow_mut()
                .extend(messages.into_iter().map(|message| (message.id, message)));
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
                ui.add_demo_messages(
                    chat,
                    vec![Message {
                        id: ui.next_demo_message(),
                        timestamp: demo::now(),
                        sender: demo::OWN_NAME.into(),
                        sender_avatar: Some(demo::OWN_AVATAR),
                        outgoing: true,
                        reply_to: None,
                        reply_preview: None,
                        can_edit: false,
                        can_delete: true,
                        forwarded: true,
                        pinned: false,
                        reactions: Vec::new(),
                        edited: false,
                        album: None,
                        ..message
                    }],
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
                            if ui.selection.get() == Some(chat) && ui.search_anchor.get().is_none()
                            {
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

    fn apply_text_edit(self: &Rc<Self>, chat: i64, id: i32, text: FormattedText) {
        self.history_cache.borrow_mut().remove(chat);
        if self.selection.get() == Some(chat) {
            self.revealed_spoilers.borrow_mut().remove(&id);
        }
        if self.is_demo.get() {
            let mut demo = self.demo_messages.borrow_mut();
            if let Some(messages) = demo.get_mut(&chat)
                && let Some(message) = messages.iter_mut().find(|message| message.id == id)
            {
                message.text.clone_from(&text.text);
                message.styles.clone_from(&text.spans);
                message.edited = true;
                if messages.last().is_some_and(|message| message.id == id)
                    && let Some(chat) = self
                        .chats
                        .borrow_mut()
                        .iter_mut()
                        .find(|item| item.id == chat)
                {
                    chat.preview = redact_spoilers(&text.text, &text.spans).into_owned();
                }
                self.render_chats();
            }
        }
        if self.selection.get() == Some(chat)
            && let Some(message) = self.messages.borrow_mut().get_mut(&id)
        {
            message.text = text.text;
            message.styles = text.spans;
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
        let saved_messages = self
            .chats
            .borrow()
            .iter()
            .any(|item| item.id == chat && item.saved_messages)
            || self
                .external_chat
                .borrow()
                .as_ref()
                .is_some_and(|item| item.id == chat && item.saved_messages);
        let dialog = adw::AlertDialog::builder()
            .heading("Удалить сообщение?")
            .body(if saved_messages {
                "Сохранённая копия будет удалена из «Избранного»."
            } else {
                "Сообщение будет удалено для всех участников чата."
            })
            .close_response("cancel")
            .default_response("cancel")
            .build();
        dialog.add_response("cancel", "Отмена");
        dialog.add_response(
            "delete",
            if saved_messages {
                "Удалить"
            } else {
                "Удалить для всех"
            },
        );
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
                let last = messages.last();
                chat.preview = last
                    .map(|message| message.preview_text().into_owned())
                    .unwrap_or_default();
                chat.preview_sender = last
                    .filter(|_| chat.group)
                    .map(|message| message.sender.clone());
                chat.preview_image = last.and_then(|message| {
                    message
                        .thumbnail
                        .map(|media| ChatImage::attachment(chat.id, message.id, media))
                });
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

    fn set_message_pinned(self: &Rc<Self>, chat: i64, id: i32, pinned: bool) {
        if self.is_demo.get() {
            if let Some(messages) = self.demo_messages.borrow_mut().get_mut(&chat)
                && let Some(message) = messages.iter_mut().find(|message| message.id == id)
            {
                message.pinned = pinned;
            }
            self.apply_message_pin(chat, id, pinned);
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        self.job(
            async move { client.set_message_pinned(chat, id, pinned).await },
            move |ui, result| match result {
                Ok(()) => {
                    ui.apply_message_pin(chat, id, pinned);
                    ui.refresh_chats();
                    if ui.selection.get() == Some(chat) {
                        ui.load_history(None, false);
                    }
                }
                Err(error) => ui.toast(&format!("Не удалось изменить закрепление. {error}")),
            },
        );
    }

    fn apply_message_pin(self: &Rc<Self>, chat: i64, id: i32, pinned: bool) {
        if self.selection.get() != Some(chat) {
            return;
        }
        if let Some(message) = self.messages.borrow_mut().get_mut(&id) {
            message.pinned = pinned;
        }
        self.render_history(false);
    }

    fn toggle_reaction(self: &Rc<Self>, chat: i64, id: i32, emoji: String) {
        if self.is_demo.get() {
            let mut history = self.demo_messages.borrow_mut();
            let Some(message) = history
                .get_mut(&chat)
                .and_then(|messages| messages.iter_mut().find(|message| message.id == id))
            else {
                return;
            };
            if let Some(reaction) = message
                .reactions
                .iter_mut()
                .find(|item| item.emoji == emoji)
            {
                reaction.chosen = !reaction.chosen;
                reaction.count += if reaction.chosen { 1 } else { -1 };
                message.reactions.retain(|item| item.count > 0);
            } else {
                message.reactions.push(Reaction {
                    emoji,
                    count: 1,
                    chosen: true,
                });
            }
            self.apply_reactions(chat, id, message.reactions.clone());
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        if !self.reacting.borrow_mut().insert((chat, id)) {
            return;
        }
        self.job(
            async move { client.toggle_reaction(chat, id, emoji).await },
            move |ui, result| {
                ui.reacting.borrow_mut().remove(&(chat, id));
                match result {
                    Ok(reactions) => {
                        ui.apply_reactions(chat, id, reactions);
                        if ui.selection.get() == Some(chat) {
                            ui.load_history(None, false);
                        }
                    }
                    Err(error) => ui.toast(&format!("Не удалось изменить реакцию. {error}")),
                }
            },
        );
    }

    fn apply_reactions(self: &Rc<Self>, chat: i64, id: i32, reactions: Vec<Reaction>) {
        if self.selection.get() == Some(chat) {
            if let Some(message) = self.messages.borrow_mut().get_mut(&id) {
                message.reactions = reactions;
            }
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

    fn begin_media_preview(&self) -> u64 {
        let request = self.media_generation.get().wrapping_add(1);
        self.media_generation.set(request);
        if let Some(previous) = self.media_viewer.upgrade() {
            previous.close();
        }
        request
    }

    fn open_playback(self: &Rc<Self>, chat: i64, id: i32, media: PlayableMedia) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        self.begin_media_preview();
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        let viewer = crate::media_viewer::open(
            &window,
            crate::media_viewer::PlaybackContext {
                runtime: self.runtime.clone(),
                media,
                muted: demo,
                load: Box::new(move |progress| {
                    let client = client.clone();
                    Box::pin(async move {
                        if demo {
                            demo::playback_preview(media, progress).await
                        } else {
                            client
                                .context("Нет подключения к Telegram")?
                                .playback_preview(chat, id, progress)
                                .await
                        }
                    })
                }),
            },
        );
        self.media_viewer.set(Some(&viewer));
    }

    fn open_photo_preview(self: &Rc<Self>, chat: i64, id: i32) {
        let request = self.begin_media_preview();
        if self.is_demo.get() {
            self.present_photo(self.demo_image.get_or_init(demo_texture).upcast_ref());
        } else if let Some(client) = self.client.borrow().clone() {
            let progress = adw::Toast::new("Загрузка фотографии…");
            self.toasts.add_toast(progress.clone());
            self.job(
                async move { client.photo_preview(chat, id).await },
                move |ui, result| {
                    progress.dismiss();
                    if ui.media_generation.get() != request {
                        return;
                    }
                    let texture = result.and_then(|bytes| {
                        gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
                            .map_err(anyhow::Error::from)
                    });
                    match texture {
                        Ok(texture) => ui.present_photo(&texture),
                        Err(error) => ui.toast(&format!("Не удалось открыть фотографию. {error}")),
                    }
                },
            );
        }
    }

    fn present_photo(&self, texture: &gtk::gdk::Texture) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let display = gtk::prelude::WidgetExt::display(&window);
        let monitor = window
            .surface()
            .and_then(|surface| display.monitor_at_surface(&surface))
            .or_else(|| {
                display
                    .monitors()
                    .item(0)
                    .and_downcast::<gtk::gdk::Monitor>()
            });
        let (screen_width, screen_height) = monitor.map_or((640, 480), |monitor| {
            let bounds = monitor.geometry();
            (bounds.width(), bounds.height())
        });

        // GNOME centers a modal toplevel without a transient parent on the current monitor.
        let viewer = adw::Window::builder()
            .title("Фотография")
            .modal(true)
            .resizable(false)
            .build();
        viewer.set_display(&display);
        viewer.set_application(window.application().as_ref());
        viewer.set_size_request(-1, -1);
        self.media_viewer.set(Some(&viewer));
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::builder()
            .show_title(false)
            .decoration_layout(":close")
            .build();
        toolbar.add_top_bar(&header);
        viewer.set_content(Some(&toolbar));
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .alternative_text("Предпросмотр фотографии")
            .paintable(texture)
            .build();
        picture.update_property(&[gtk::accessible::Property::Label("Предпросмотр фотографии")]);
        let (header_width, _, _, _) = header.measure(gtk::Orientation::Horizontal, -1);
        let (header_height, _, _, _) = header.measure(gtk::Orientation::Vertical, -1);
        let (width, height) = photo_viewport(
            (texture.width(), texture.height()),
            (screen_width, screen_height),
            (header_width, header_height),
            crate::media_viewer::frame_spacing(&viewer),
        );
        let content = gtk::Overlay::new();
        content.set_size_request(width, height);
        content.add_overlay(&picture);
        toolbar.set_content(Some(&content));
        viewer.set_default_size(width.max(header_width), height + header_height);

        let close = gtk::EventControllerKey::new();
        close.connect_key_pressed(glib::clone!(
            #[weak]
            viewer,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    viewer.close();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        viewer.add_controller(close);
        viewer.present();
    }

    fn append_message_text(self: &Rc<Self>, bubble: &gtk::Box, message: &Message) {
        if message.text.is_empty() {
            return;
        }
        let label = label("", "");
        let formatted = message.formatted_text();
        let hidden = formatted
            .spans
            .iter()
            .any(|span| span.style == TextStyle::Spoiler)
            && self.revealed_spoilers.borrow().get(&message.id) != Some(&formatted);
        if hidden {
            concealed_message(&label, &formatted);
            let id = message.id;
            let generation = self.generation.get();
            label.connect_activate_link(glib::clone!(
                #[weak(rename_to = ui)]
                self,
                #[upgrade_or]
                glib::Propagation::Stop,
                move |label, _| {
                    let formatted = formatted.clone();
                    glib::idle_add_local_once(glib::clone!(
                        #[weak]
                        ui,
                        #[weak]
                        label,
                        move || {
                            if ui.generation.get() == generation && label.is_mapped() {
                                label.set_text(&formatted.text);
                                label.set_attributes(Some(&message_attributes(&formatted.spans)));
                                ui.revealed_spoilers.borrow_mut().insert(id, formatted);
                            }
                        }
                    ));
                    glib::Propagation::Stop
                }
            ));
        } else {
            label.set_text(&formatted.text);
            if !formatted.spans.is_empty() {
                label.set_attributes(Some(&message_attributes(&formatted.spans)));
            }
        }
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_max_width_chars(64);
        bubble.append(&label);
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }
}

fn attachment_message_id(mut widget: Option<gtk::Widget>) -> Option<i32> {
    while let Some(item) = widget {
        if let Some(id) = item.widget_name().strip_prefix("attachment-") {
            return id.parse().ok();
        }
        widget = item.parent();
    }
    None
}

fn reply_quote() -> (gtk::Box, gtk::Label, gtk::Label) {
    let quote = gtk::Box::new(gtk::Orientation::Vertical, 2);
    quote.add_css_class("reply-quote");
    let author = label("", "message-name");
    let excerpt = label("", "reply-excerpt");
    for line in [&author, &excerpt] {
        line.set_single_line_mode(true);
        line.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.set_max_width_chars(42);
        quote.append(line);
    }
    (quote, author, excerpt)
}

fn scaled_thumbnail(texture: &gtk::gdk::Texture) -> Option<gtk::gdk::Texture> {
    let width = f64::from(texture.width());
    let height = f64::from(texture.height());
    let scale = (264.0 / width).min(220.0 / height);
    let width = (width * scale).round().max(1.0) as i32;
    let height = (height * scale).round().max(1.0) as i32;
    let mut download = gtk::gdk::TextureDownloader::new(texture);
    download.set_format(gtk::gdk::MemoryFormat::R8g8b8a8);
    let (bytes, stride) = download.download_bytes();
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_bytes(
        &bytes,
        gtk::gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        texture.width(),
        texture.height(),
        i32::try_from(stride).ok()?,
    );
    let thumbnail = pixbuf.scale_simple(width, height, gtk::gdk_pixbuf::InterpType::Bilinear)?;
    Some(gtk::gdk::Texture::for_pixbuf(&thumbnail))
}

fn notification_id(chat: i64) -> String {
    format!("chat-{chat}")
}

fn demo_texture() -> gtk::gdk::MemoryTexture {
    let bytes = glib::Bytes::from_owned(demo::preview_pixels());
    gtk::gdk::MemoryTexture::new(480, 300, gtk::gdk::MemoryFormat::R8g8b8, &bytes, 1440)
}

fn add_menu_button(menu: &gtk::Box, title: &str, activate: impl Fn() + 'static) {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.set_child(Some(&label(title, "")));
    button.connect_clicked(move |button| {
        if let Some(popover) = button
            .ancestor(gtk::Popover::static_type())
            .and_downcast::<gtk::Popover>()
        {
            popover.popdown();
        }
        activate();
    });
    menu.append(&button);
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

#[cfg(test)]
mod tests {
    use super::photo_viewport;

    #[test]
    fn photo_window_includes_its_header_in_the_quarter_screen_limit() {
        for screen @ (screen_width, screen_height) in [(1280, 800), (800, 1280), (640, 480)] {
            for (width, height) in [(480, 300), (1600, 1200), (900, 1600), (15, 1600), (1600, 9)] {
                let (fitted_width, fitted_height) =
                    photo_viewport((width, height), screen, (44, 47), (10, 10));
                let area = (fitted_width.max(44) + 10) * (fitted_height + 47 + 10);
                assert!(4 * area <= screen_width * screen_height);
                assert!(fitted_width <= width && fitted_height <= height);
                assert!((fitted_width * height - fitted_height * width).abs() <= width.max(height));
            }
        }
    }
}
