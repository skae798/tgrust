use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{oneshot, watch};

use crate::accounts::{Account, Accounts};
use crate::demo;
use crate::formatting::{SPOILER_LINK, editor_text, render_message, set_editor_text, setup_editor};
use crate::media_viewer::fit_viewport as photo_viewport;
use crate::model::{
    Avatar, COMMON_REACTIONS, Chat, ChatFolder, ChatImage, Contact, FormattedText,
    HISTORY_PAGE_SIZE, HistoryCache, HistoryRevision, IncomingMessage, MAX_ALBUM_ITEMS, Message,
    Poll, PollUpdate, PollVoteCount, Reaction, ReadRequest, Reply, SearchHit, TextStyle,
    UpdateStatus, Upload, UploadKind, merge_latest_history, order_chats, prepend_history_page,
    redact_spoilers,
};
use crate::model::{
    ChatInfo, Conversation, ForumTopic, ForumWriteResult, GENERAL_TOPIC, GroupPermissionEdit,
    GroupPermissions, PlayableMedia, slow_mode_change,
};
use crate::storage::{PendingSave, SessionRemoval, attachment_name};
use crate::telegram::{LoginStep, Telegram};

type ImageTarget = Box<dyn FnOnce(&gtk::gdk::Texture)>;

enum ImageCacheEntry {
    Loading(Vec<ImageTarget>),
    Ready(gtk::gdk::Texture),
    Unavailable(Instant),
}

struct InlineAudio {
    chat: i64,
    id: i32,
    media: PlayableMedia,
    player: Rc<crate::audio::InlinePlayer>,
}

#[derive(Clone, Copy)]
enum HistoryScroll {
    Bottom,
    Message { id: i32, offset: f64 },
    Value(f64),
}

struct Ui {
    runtime: Handle,
    stop: watch::Receiver<bool>,
    listener_finished: RefCell<Option<oneshot::Receiver<()>>>,
    session_removal: RefCell<Option<SessionRemoval>>,
    accounts: Rc<Accounts>,
    account: Account,
    api_hash: RefCell<Option<String>>,
    api_hash_input: gtk::PasswordEntry,
    accounts_dialog: glib::WeakRef<adw::Dialog>,
    logging_out: Cell<bool>,
    logout_dialog: glib::WeakRef<adw::Dialog>,
    window: glib::WeakRef<adw::ApplicationWindow>,
    root: gtk::Stack,
    login_title: gtk::Label,
    login_status: gtk::Label,
    login_entry: gtk::Entry,
    login_password: gtk::PasswordEntry,
    login_submit: gtk::Button,
    login_reset: gtk::Button,
    login_qr: gtk::Button,
    login_phone: gtk::Button,
    login_qr_retry: gtk::Button,
    login_qr_image: gtk::Picture,
    qr_cancel: RefCell<Option<watch::Sender<bool>>>,
    qr_visible: Cell<bool>,
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
    demo_chat_info: Rc<RefCell<HashMap<i64, ChatInfo>>>,
    demo_chat_addresses: Rc<RefCell<HashMap<i64, crate::model::ChatAddresses>>>,
    demo_group_permissions: Rc<RefCell<HashMap<i64, GroupPermissionEdit>>>,
    demo_created_chats: Rc<RefCell<HashMap<i64, crate::model::NewChatDraft>>>,
    create_chat_dialog: glib::WeakRef<adw::Dialog>,
    demo_member_changes: Rc<RefCell<demo::MemberChanges>>,
    demo_invites: Rc<RefCell<demo::InviteLinks>>,
    demo_join_requests: Rc<RefCell<demo::JoinRequests>>,
    own_profile_dialog: glib::WeakRef<adw::Dialog>,
    poll_dialog: glib::WeakRef<adw::Dialog>,
    stickers_dialog: glib::WeakRef<adw::Dialog>,
    demo_favorite_stickers: Rc<RefCell<HashSet<i64>>>,
    demo_sticker_packs: Rc<RefCell<demo::StickerPacks>>,
    own_name: RefCell<Option<String>>,
    own_avatar: Cell<Option<Option<Avatar>>>,
    account_label: gtk::Label,
    account_avatar: adw::Avatar,
    demo_profile: Rc<RefCell<crate::model::OwnProfile>>,
    demo_photo: Rc<RefCell<Option<(Avatar, gtk::gdk::Texture)>>>,
    contacts_dialog: glib::WeakRef<adw::Dialog>,
    devices_dialog: glib::WeakRef<adw::Dialog>,
    demo_devices: Rc<RefCell<crate::model::DeviceSessions>>,
    privacy_dialog: glib::WeakRef<adw::Dialog>,
    proxy_dialog: glib::WeakRef<adw::Dialog>,
    demo_privacy: Rc<RefCell<HashMap<crate::model::PrivacyKey, crate::model::PrivacySetting>>>,
    blocked_dialog: glib::WeakRef<adw::Dialog>,
    demo_blocked: Rc<RefCell<Vec<i64>>>,
    history: gtk::Box,
    message_menu: gtk::Popover,
    menu_message: Cell<Option<i32>>,
    scroll: gtk::ScrolledWindow,
    older: gtk::Button,
    history_scroll: Cell<Option<HistoryScroll>>,
    prepend_scroll: Cell<Option<(f64, f64)>>,
    latest: gtk::Button,
    input: gtk::TextView,
    input_placeholder: gtk::Label,
    send: gtk::Button,
    slow_mode_label: gtk::Label,
    slow_mode_waits: RefCell<HashMap<i64, Instant>>,
    slow_mode_requests: RefCell<HashMap<i64, Rc<()>>>,
    slow_mode_timer_running: Cell<bool>,
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
    selected_topic: RefCell<Option<ForumTopic>>,
    forum_jump: Cell<u64>,
    topic_picker: glib::WeakRef<adw::Dialog>,
    topics_button: gtk::Button,
    open_topics_button: gtk::Button,
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
    older_history_pending: Cell<bool>,
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
    demo_read_topics: Rc<RefCell<HashSet<Conversation>>>,
    demo_forums: Rc<RefCell<HashMap<i64, demo::ForumTopics>>>,
    demo_next_message: Cell<i32>,
    drafts: RefCell<HashMap<Conversation, FormattedText>>,
    uploads: RefCell<HashMap<Conversation, Vec<Upload>>>,
    replies: RefCell<HashMap<Conversation, Reply>>,
    reacting: RefCell<HashSet<(i64, i32)>>,
    voting: RefCell<HashSet<(i64, i32)>>,
    poll_revision: Cell<u64>,
    recent_polls: RefCell<VecDeque<(u64, PollUpdate)>>,
    images: RefCell<HashMap<ChatImage, ImageCacheEntry>>,
    demo_image: OnceCell<gtk::gdk::MemoryTexture>,
    media_viewer: glib::WeakRef<adw::Window>,
    inline_audio: RefCell<Option<InlineAudio>>,
    audio_cards: RefCell<HashMap<i32, crate::audio::AudioCard>>,
    media_generation: Cell<u64>,
    loaded: Cell<bool>,
    has_older: Cell<bool>,
}

pub fn open(
    app: &adw::Application,
    accounts: Rc<Accounts>,
    account: Account,
    api_hash: Option<String>,
    runtime: Runtime,
) -> adw::ApplicationWindow {
    build_window(app, accounts, account, api_hash, runtime).0
}

fn build_window(
    app: &adw::Application,
    accounts: Rc<Accounts>,
    account: Account,
    api_hash: Option<String>,
    runtime: Runtime,
) -> (adw::ApplicationWindow, Rc<Ui>) {
    let demo = matches!(account, Account::Demo(_));
    let title = if matches!(account, Account::Welcome | Account::Demo(0)) {
        "tgrust".into()
    } else {
        format!("tgrust — {}", accounts.display_name(&account))
    };
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(title)
        .default_width(1050)
        .default_height(720)
        .width_request(360)
        .height_request(480)
        .build();

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
    let login_entry = gtk::Entry::builder()
        .input_purpose(gtk::InputPurpose::Phone)
        .build();
    let login_password = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .visible(false)
        .build();
    let login_submit = gtk::Button::with_label("Продолжить");
    login_submit.add_css_class("suggested-action");
    let login_reset = gtk::Button::with_label("Начать вход заново");
    login_reset.add_css_class("flat");
    login_reset.set_visible(false);
    let login_qr = gtk::Button::with_label("Войти по QR-коду");
    login_qr.set_visible(false);
    let login_phone = gtk::Button::with_label("Войти по номеру телефона");
    login_phone.set_visible(false);
    let login_qr_retry = gtk::Button::with_label("Получить QR-код снова");
    login_qr_retry.set_visible(false);
    let login_qr_image = gtk::Picture::builder()
        .halign(gtk::Align::Center)
        .can_shrink(false)
        .visible(false)
        .build();
    login_qr_image.update_property(&[gtk::accessible::Property::Label("QR-код входа в Telegram")]);
    let api_hash_input = gtk::PasswordEntry::builder()
        .placeholder_text("API hash для повторного входа")
        .visible(false)
        .build();
    let connect_retry = gtk::Button::with_label("Повторить подключение");
    connect_retry.add_css_class("suggested-action");
    connect_retry.set_visible(false);
    let login_accounts = gtk::Button::with_label("Аккаунты");
    login_accounts.add_css_class("flat");
    let login_proxy = gtk::Button::with_label("Прокси");
    login_proxy.add_css_class("flat");
    let login_updates = gtk::Button::builder()
        .label("Обновления клиента")
        .action_name("app.updates")
        .css_classes(["flat"])
        .build();
    let demo_button = gtk::Button::with_label("Открыть деморежим");
    demo_button.add_css_class("flat");
    for widget in [
        login_title.upcast_ref::<gtk::Widget>(),
        login_status.upcast_ref(),
        login_entry.upcast_ref(),
        login_password.upcast_ref(),
        login_submit.upcast_ref(),
        login_reset.upcast_ref(),
        login_qr_image.upcast_ref(),
        login_qr.upcast_ref(),
        login_phone.upcast_ref(),
        login_qr_retry.upcast_ref(),
        api_hash_input.upcast_ref(),
        connect_retry.upcast_ref(),
        login_accounts.upcast_ref(),
        login_proxy.upcast_ref(),
        login_updates.upcast_ref(),
        demo_button.upcast_ref(),
    ] {
        login_box.append(widget);
    }
    let login_scroll = gtk::ScrolledWindow::builder()
        .child(&login_box)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    login.set_content(Some(&login_scroll));
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
    let devices_button = gtk::Button::with_label("Устройства");
    devices_button.add_css_class("flat");
    let menu_body = gtk::Box::new(gtk::Orientation::Vertical, 4);
    menu_body.set_margin_start(6);
    menu_body.set_margin_end(6);
    menu_body.set_margin_top(6);
    menu_body.set_margin_bottom(6);
    let my_profile = gtk::Button::with_label("Мой профиль");
    my_profile.add_css_class("flat");
    menu_body.append(&my_profile);
    let create_chat = gtk::Button::with_label("Создать чат");
    create_chat.add_css_class("flat");
    menu_body.prepend(&create_chat);
    menu_body.append(&devices_button);
    let privacy_button = gtk::Button::with_label("Конфиденциальность");
    privacy_button.add_css_class("flat");
    menu_body.append(&privacy_button);
    let blocked_button = gtk::Button::with_label("Заблокированные");
    blocked_button.add_css_class("flat");
    menu_body.append(&blocked_button);
    let proxy_button = gtk::Button::with_label("Прокси");
    proxy_button.add_css_class("flat");
    menu_body.append(&proxy_button);
    let updates_button = gtk::Button::builder()
        .label("Обновления клиента")
        .action_name("app.updates")
        .css_classes(["flat"])
        .build();
    menu_body.append(&updates_button);
    let logout = gtk::Button::with_label("Выйти из аккаунта");
    logout.add_css_class("flat");
    logout.add_css_class("destructive-action");
    let account_button = gtk::Button::with_label("Аккаунты");
    account_button.add_css_class("flat");
    menu_body.append(&account_button);
    menu_body.append(&logout);
    let menu_popup = gtk::Popover::builder().child(&menu_body).build();
    menu_popup.set_default_widget(Some(&devices_button));
    updates_button.connect_clicked(glib::clone!(
        #[weak]
        menu_popup,
        move |_| menu_popup.popdown()
    ));
    let menu_label = gtk::Label::builder()
        .label(if matches!(account, Account::Welcome | Account::Demo(0)) {
            "tgrust".into()
        } else {
            accounts.display_name(&account)
        })
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(14)
        .build();
    let account_avatar = adw::Avatar::new(24, None, true);
    account_avatar.set_visible(false);
    let account_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    account_header.append(&account_avatar);
    account_header.append(&menu_label);
    let app_menu = gtk::MenuButton::builder()
        .child(&account_header)
        .tooltip_text("Меню приложения (F10)")
        .primary(true)
        .popover(&menu_popup)
        .build();
    app_menu.add_css_class("flat");
    app_menu.update_property(&[gtk::accessible::Property::Label("Меню приложения")]);
    side_header.set_title_widget(Some(&app_menu));
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
    let topics_button = icon_button("view-list-symbolic", "Темы");
    topics_button.set_visible(false);
    pane_header.pack_start(&topics_button);
    pane.add_top_bar(&pane_header);
    let content = gtk::Stack::new();
    let empty = adw::StatusPage::builder()
        .icon_name("mail-unread-symbolic")
        .title("Выберите чат")
        .build();
    content.add_named(&empty, Some("empty"));
    let open_topics_button = gtk::Button::with_label("Открыть темы");
    open_topics_button.set_halign(gtk::Align::Center);
    open_topics_button.add_css_class("suggested-action");
    let forum = adw::StatusPage::builder()
        .icon_name("view-list-symbolic")
        .title("Выберите тему")
        .child(&open_topics_button)
        .build();
    content.add_named(&forum, Some("forum"));
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
    attach_menu.append(Some("Опрос"), Some("win.create-poll"));
    attach_menu.append(Some("Стикеры"), Some("win.stickers"));
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
    let slow_mode_label = gtk::Label::builder()
        .xalign(0.0)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .wrap(true)
        .css_classes(["dim-label", "caption"])
        .visible(false)
        .build();
    conversation.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    conversation.append(&reply_bar);
    conversation.append(&upload_bar);
    conversation.append(&slow_mode_label);
    conversation.append(&composer);
    content.add_named(&conversation, Some("conversation"));
    pane.set_content(Some(&content));
    split.set_content(Some(&adw::NavigationPage::new(&pane, "Чат")));

    let (stop, stop_rx) = watch::channel(false);
    let ui = Rc::new(Ui {
        runtime: runtime.handle().clone(),
        stop: stop_rx,
        listener_finished: RefCell::new(None),
        session_removal: RefCell::new(None),
        accounts,
        account,
        api_hash: RefCell::new(api_hash),
        api_hash_input,
        accounts_dialog: glib::WeakRef::new(),
        logging_out: Cell::new(false),
        logout_dialog: glib::WeakRef::new(),
        window: window.downgrade(),
        root,
        login_title,
        login_status,
        login_entry,
        login_password,
        login_submit,
        login_reset,
        login_qr,
        login_phone,
        login_qr_retry,
        login_qr_image,
        qr_cancel: RefCell::new(None),
        qr_visible: Cell::new(false),
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
        demo_chat_info: Rc::new(RefCell::new(HashMap::new())),
        demo_chat_addresses: Rc::new(RefCell::new(HashMap::new())),
        demo_group_permissions: Rc::new(RefCell::new(HashMap::new())),
        demo_created_chats: Rc::new(RefCell::new(HashMap::new())),
        create_chat_dialog: glib::WeakRef::new(),
        demo_member_changes: Rc::new(RefCell::new(demo::MemberChanges::default())),
        demo_invites: Rc::new(RefCell::new(demo::InviteLinks::default())),
        demo_join_requests: Rc::new(RefCell::new(demo::JoinRequests::default())),
        own_profile_dialog: glib::WeakRef::new(),
        poll_dialog: glib::WeakRef::new(),
        stickers_dialog: glib::WeakRef::new(),
        demo_favorite_stickers: Rc::default(),
        demo_sticker_packs: Rc::default(),
        own_name: RefCell::new(None),
        own_avatar: Cell::new(None),
        account_label: menu_label,
        account_avatar,
        demo_profile: Rc::new(RefCell::new(demo::own_profile())),
        demo_photo: Rc::new(RefCell::new(None)),
        contacts_dialog: glib::WeakRef::new(),
        devices_dialog: glib::WeakRef::new(),
        demo_devices: Rc::new(RefCell::new(demo::device_sessions())),
        privacy_dialog: glib::WeakRef::new(),
        proxy_dialog: glib::WeakRef::new(),
        demo_privacy: Rc::new(RefCell::new(demo::privacy_settings())),
        blocked_dialog: glib::WeakRef::new(),
        demo_blocked: Rc::new(RefCell::new(
            std::iter::once(12).chain(10001..=10043).collect(),
        )),
        history,
        message_menu,
        menu_message: Cell::new(None),
        scroll,
        older,
        history_scroll: Cell::new(None),
        prepend_scroll: Cell::new(None),
        latest,
        input,
        input_placeholder,
        send,
        slow_mode_label,
        slow_mode_waits: RefCell::new(HashMap::new()),
        slow_mode_requests: RefCell::new(HashMap::new()),
        slow_mode_timer_running: Cell::new(false),
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
        selected_topic: RefCell::new(None),
        forum_jump: Cell::new(0),
        topic_picker: glib::WeakRef::new(),
        topics_button,
        open_topics_button,
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
        older_history_pending: Cell::new(false),
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
        demo_read_topics: Rc::new(RefCell::new(HashSet::new())),
        demo_forums: Rc::new(RefCell::new(HashMap::new())),
        demo_next_message: Cell::new(200),
        drafts: RefCell::new(HashMap::new()),
        uploads: RefCell::new(HashMap::new()),
        replies: RefCell::new(HashMap::new()),
        reacting: RefCell::new(HashSet::new()),
        voting: RefCell::new(HashSet::new()),
        poll_revision: Cell::new(0),
        recent_polls: RefCell::new(VecDeque::new()),
        images: RefCell::new(HashMap::new()),
        demo_image: OnceCell::new(),
        media_viewer: glib::WeakRef::new(),
        inline_audio: RefCell::new(None),
        audio_cards: RefCell::new(HashMap::new()),
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
    ui.login_password.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_| ui.submit_login()
    ));
    for button in [&ui.login_qr, &ui.login_qr_retry] {
        button.connect_clicked(glib::clone!(
            #[weak]
            ui,
            move |_| ui.start_qr_login()
        ));
    }
    ui.login_phone.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.return_to_phone()
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
    ui.api_hash_input.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_| ui.connect()
    ));
    demo_button.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| {
            if let Err(error) = ui.accounts.open_demo() {
                ui.toast(&error.to_string());
            }
        }
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
    ui.split.connect_show_content_notify(glib::clone!(
        #[weak]
        ui,
        move |split| {
            if split.is_collapsed() && !split.shows_content() {
                ui.inline_audio.take();
            }
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
    devices_button.connect_clicked(glib::clone!(
        #[weak]
        ui,
        #[weak]
        menu_popup,
        move |_| {
            menu_popup.popdown();
            ui.open_devices();
        }
    ));
    logout.connect_clicked(glib::clone!(
        #[weak]
        ui,
        #[weak]
        menu_popup,
        move |_| {
            menu_popup.popdown();
            ui.confirm_logout();
        }
    ));
    for button in [&account_button, &login_accounts] {
        button.connect_clicked(glib::clone!(
            #[weak]
            ui,
            #[weak]
            menu_popup,
            move |_| {
                menu_popup.popdown();
                ui.open_accounts();
            }
        ));
    }
    privacy_button.connect_clicked(glib::clone!(
        #[weak]
        ui,
        #[weak]
        menu_popup,
        move |_| {
            menu_popup.popdown();
            ui.open_privacy();
        }
    ));
    my_profile.connect_clicked(glib::clone!(
        #[weak]
        ui,
        #[weak]
        menu_popup,
        move |_| {
            menu_popup.popdown();
            ui.open_own_profile();
        }
    ));
    for button in [&login_proxy, &proxy_button] {
        button.connect_clicked(glib::clone!(
            #[weak]
            ui,
            #[weak]
            menu_popup,
            move |_| {
                menu_popup.popdown();
                ui.open_proxy();
            }
        ));
    }
    blocked_button.connect_clicked(glib::clone!(
        #[weak]
        ui,
        #[weak]
        menu_popup,
        move |_| {
            menu_popup.popdown();
            ui.open_blocked();
        }
    ));
    let devices_action = gtk::gio::SimpleAction::new("devices", None);
    devices_action.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_, _| ui.open_devices()
    ));
    window.add_action(&devices_action);
    app.set_accels_for_action("win.devices", &["<Primary><Shift>d"]);
    contacts_button.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| ui.open_contacts()
    ));
    create_chat.connect_clicked(glib::clone!(
        #[weak]
        ui,
        #[weak]
        menu_popup,
        move |_| {
            menu_popup.popdown();
            ui.open_create_chat();
        }
    ));
    ui.latest.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| {
            ui.reopen_conversation(None);
        }
    ));
    for button in [&ui.topics_button, &ui.open_topics_button] {
        button.connect_clicked(glib::clone!(
            #[weak]
            ui,
            move |_| ui.open_current_topics()
        ));
    }
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
    ui.scroll.vadjustment().connect_changed(glib::clone!(
        #[weak]
        ui,
        move |adjustment| {
            // Keep the viewport in place during GTK's first allocation of an older page.
            // The message anchor corrects changes in individual row heights afterwards.
            if let Some((upper, value)) = ui.prepend_scroll.take()
                && adjustment.upper() > upper
            {
                adjustment.set_value(value + adjustment.upper() - upper);
            }
        }
    ));
    let history_scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    history_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    history_scroll.connect_scroll(glib::clone!(
        #[weak]
        ui,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, _, dy| {
            if dy < 0.0 {
                ui.load_older_near_top();
            }
            glib::Propagation::Proceed
        }
    ));
    ui.scroll.add_controller(history_scroll);
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
    let create_poll = gtk::gio::SimpleAction::new("create-poll", None);
    create_poll.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_, _| ui.create_poll()
    ));
    window.add_action(&create_poll);
    let stickers = gtk::gio::SimpleAction::new("stickers", None);
    stickers.connect_activate(glib::clone!(
        #[weak]
        ui,
        move |_, _| ui.open_stickers()
    ));
    window.add_action(&stickers);
    remove_upload.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| {
            if let Some(target) = ui.conversation() {
                ui.uploads.borrow_mut().remove(&target);
                ui.update_send_button();
            }
        }
    ));
    remove_reply.connect_clicked(glib::clone!(
        #[weak]
        ui,
        move |_| {
            if let Some(target) = ui.conversation() {
                ui.replies.borrow_mut().remove(&target);
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
        let runtime = RefCell::new(Some(runtime));
        move |_| {
            if ui.logging_out.get() {
                return glib::Propagation::Stop;
            }
            if let Some(app) = ui.window.upgrade().and_then(|window| window.application()) {
                for id in ui.notified_messages.borrow().keys() {
                    app.withdraw_notification(&ui.notification_id(*id));
                }
            }
            if let Some(viewer) = ui.media_viewer.upgrade() {
                viewer.close();
            }
            ui.inline_audio.take();
            ui.window.set(None);
            if let Some(cancel) = ui.qr_cancel.take() {
                cancel.send_replace(true);
            }
            stop.send_replace(true);
            ui.client.take();
            if let Some(runtime) = runtime.take() {
                ui.accounts.close(
                    ui.account.clone(),
                    runtime,
                    ui.session_removal.take(),
                    ui.listener_finished.take(),
                );
            }
            glib::Propagation::Proceed
        }
    });
    window.present();
    if demo {
        ui.start_demo();
    } else {
        ui.connect();
    }
    (window, ui)
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
        self.login_password.set_visible(false);
        self.login_submit.set_visible(false);
        let Account::Saved(profile) = &self.account else {
            self.login_status.set_text(&self.accounts.startup_hint());
            return;
        };
        let hash = self.api_hash_input.text();
        if !hash.is_empty() {
            if let Err(error) = crate::accounts::validate_hash(&hash) {
                self.login_status.set_text(&error.to_string());
                self.connect_retry.set_visible(true);
                self.api_hash_input.grab_focus();
                return;
            }
            self.api_hash.replace(Some(hash.to_string()));
            self.api_hash_input.set_text("");
        }
        self.api_hash_input.set_visible(false);
        self.login_status.set_text("Подключение…");
        self.job(
            Telegram::connect(profile.clone(), self.api_hash.borrow().clone()),
            |ui, result| match result {
                Ok((client, step)) => {
                    ui.unread_overrides.borrow_mut().clear();
                    ui.reading_chats.borrow_mut().clear();
                    ui.client.replace(Some(client));
                    if step == LoginStep::Phone {
                        ui.start_qr_login();
                    } else {
                        ui.show_step(step);
                    }
                }
                Err(error) => {
                    ui.login_status
                        .set_text(&format!("Не удалось подключиться. {error}"));
                    ui.connect_retry.set_visible(true);
                    if ui.api_hash.borrow().is_none() {
                        ui.api_hash_input.set_visible(true);
                        ui.api_hash_input.grab_focus();
                    } else {
                        ui.connect_retry.grab_focus();
                    }
                }
            },
        );
    }

    fn set_login_busy(&self, busy: bool) {
        self.login_entry.set_sensitive(!busy);
        self.login_password.set_sensitive(!busy);
        self.login_submit.set_sensitive(!busy);
        self.login_reset.set_sensitive(!busy);
        self.login_qr.set_sensitive(!busy);
    }

    fn hide_qr(&self) {
        self.qr_visible.set(false);
        self.login_phone.set_visible(false);
        self.login_qr_retry.set_visible(false);
        self.login_qr_image.set_visible(false);
        self.login_qr_image
            .set_paintable(None::<&gtk::gdk::Paintable>);
    }

    fn start_qr_login(self: &Rc<Self>) {
        if self.qr_cancel.borrow().is_some() || self.is_demo.get() {
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        self.set_login_busy(true);
        self.login_entry.set_visible(false);
        self.login_password.set_visible(false);
        self.login_submit.set_visible(false);
        self.login_reset.set_visible(false);
        self.login_qr.set_visible(false);
        self.login_phone.set_visible(true);
        self.login_qr_retry.set_visible(false);
        self.login_title.set_text("Вход по QR-коду");
        self.login_status.set_text("Получаем QR-код…");
        self.qr_visible.set(true);
        let (cancel, cancelled) = watch::channel(false);
        self.qr_cancel.replace(Some(cancel));
        let (tokens, mut new_tokens) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            while let Some(token) = new_tokens.recv().await {
                let Some(ui) = weak.upgrade().filter(|ui| ui.qr_visible.get()) else { break };
                match crate::qr_login::texture(&token) {
                    Ok(image) => {
                        ui.login_qr_image.set_paintable(Some(&image));
                        ui.login_qr_image.set_visible(true);
                        ui.login_status.set_text("Откройте Telegram на телефоне, затем «Настройки» → «Устройства» → «Подключить устройство» и отсканируйте код.");
                    }
                    Err(error) => ui.login_status.set_text(&error.to_string()),
                }
            }
        });
        let except_ids = self.accounts.signed_in_ids(&self.account);
        self.job(
            async move { client.login_by_qr(except_ids, cancelled, tokens).await },
            |ui, result| {
                ui.qr_cancel.take();
                if ui.is_demo.get() {
                    return;
                }
                match result {
                    Ok(step) => ui.show_step(step),
                    Err(error) if ui.qr_visible.get() => {
                        ui.login_qr_image.set_visible(false);
                        ui.login_qr_image
                            .set_paintable(None::<&gtk::gdk::Paintable>);
                        ui.login_status
                            .set_text(&format!("Не удалось выполнить вход по QR-коду. {error}"));
                        ui.login_qr_retry.set_visible(true);
                    }
                    Err(_) => ui.show_step(LoginStep::Phone),
                }
            },
        );
    }

    fn return_to_phone(self: &Rc<Self>) {
        self.hide_qr();
        if let Some(cancel) = self.qr_cancel.borrow().as_ref() {
            self.login_status
                .set_text("Переключаемся на вход по номеру…");
            cancel.send_replace(true);
        } else {
            self.show_step(LoginStep::Phone);
        }
    }

    fn show_step(self: &Rc<Self>, step: LoginStep) {
        self.hide_qr();
        self.login_step.set(step);
        self.login_entry
            .set_visible(matches!(step, LoginStep::Phone | LoginStep::Code));
        self.login_password.set_visible(step == LoginStep::Password);
        self.login_submit.set_visible(step != LoginStep::Authorized);
        self.login_entry.set_text("");
        self.login_password.set_text("");
        self.login_qr.set_visible(step == LoginStep::Phone);
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
        if step == LoginStep::Password {
            self.login_password.set_placeholder_text(Some(placeholder));
            self.login_password.grab_focus();
        } else {
            self.login_entry
                .set_input_purpose(if step == LoginStep::Phone {
                    gtk::InputPurpose::Phone
                } else {
                    gtk::InputPurpose::Digits
                });
            self.login_entry.set_placeholder_text(Some(placeholder));
            self.login_entry.grab_focus();
        }
    }

    fn submit_login(self: &Rc<Self>) {
        if !self.login_submit.is_sensitive() {
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let value = if self.login_step.get() == LoginStep::Password {
            self.login_password.text().to_string()
        } else {
            self.login_entry.text().trim().to_owned()
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
                        ui.login_password.set_text("");
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
        let identity_client = client.clone();
        self.job(
            async move {
                tokio::time::timeout(Duration::from_secs(12), identity_client.own_profile()).await?
            },
            |ui, result| {
                if let Ok(profile) = result {
                    ui.remember_own_profile(&profile);
                }
            },
        );
        let mut changes = client.changes();
        let mut polls = client.poll_updates();
        let mut poll_stop = self.stop.clone();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            loop {
                let update = tokio::select! {
                    _ = poll_stop.changed() => break,
                    update = polls.recv() => update,
                };
                let Some(ui) = weak.upgrade() else { break };
                match update {
                    Ok(update) => ui.apply_poll_update(update),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        ui.load_history(None, false);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let stop = self.stop.clone();
        let (incoming, mut messages) = tokio::sync::mpsc::channel(32);
        let (finish, finished) = oneshot::channel();
        self.listener_finished.replace(Some(finished));
        let listener = client.clone();
        self.job(
            async move {
                let result = listener.listen(stop, incoming).await;
                let _ = finish.send(());
                result
            },
            |ui, result| {
                if result.is_err() && !ui.logging_out.get() {
                    ui.toast("Обновления Telegram остановлены. Откройте аккаунт заново, чтобы проверить сессию.");
                }
            },
        );
        let (allowed, mut desktop_messages) = tokio::sync::mpsc::channel(32);
        let notification_client = client.clone();
        self.runtime.spawn(async move {
            let mut filter = crate::telegram::NotificationFilter::default();
            while let Some(message) = messages.recv().await {
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    notification_client.desktop_notification(&mut filter, message),
                )
                .await;
                if let Ok(Ok(Some(message))) = result
                    && allowed.send(message).await.is_err()
                {
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
                let Some(client) = ui.client.borrow().clone() else {
                    break;
                };
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
        let muted = |id| {
            self.muting_chats.borrow().contains(&id)
                || self
                    .chats
                    .borrow()
                    .iter()
                    .any(|chat| chat.id == id && chat.muted)
        };
        if muted(message.chat) || message.origin_channel.is_some_and(muted) {
            return;
        }
        let Some(app) = self
            .window
            .upgrade()
            .and_then(|window| window.application())
        else {
            return;
        };
        let title = format!(
            "{} · {}",
            message.title,
            self.accounts.display_name(&self.account)
        );
        let notification = gtk::gio::Notification::new(&title);
        notification.set_body(Some("Новое сообщение"));
        notification.set_default_action_and_target_value(
            "app.open-account-chat",
            Some(&(self.account.key(), message.chat).to_variant()),
        );
        let mut notified = self.notified_messages.borrow_mut();
        if notified
            .get(&message.chat)
            .is_some_and(|id| *id >= message.message)
        {
            return;
        }
        notified.insert(message.chat, message.message);
        app.send_notification(Some(&self.notification_id(message.chat)), &notification);
    }

    fn notification_id(&self, chat: i64) -> String {
        format!("account-{}-chat-{chat}", self.account.key())
    }

    fn withdraw_chat_notification(&self, chat: i64) {
        if self.notified_messages.borrow().contains_key(&chat)
            && let Some(app) = self
                .window
                .upgrade()
                .and_then(|window| window.application())
        {
            app.withdraw_notification(&self.notification_id(chat));
        }
    }

    fn open_notified_chat(self: &Rc<Self>, id: i64) {
        if self.logging_out.get() {
            return;
        }
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
                        let previous = ui.selection.get().and_then(|id| ui.chat_view(id));
                        for chat in chats.iter().filter(|chat| chat.unread == 0 || chat.muted) {
                            ui.withdraw_chat_notification(chat.id);
                        }
                        ui.chats.replace(chats);
                        if let Some(id) = ui.selection.get()
                            && let Some(chat) = ui.chat_view(id)
                        {
                            let previous_slow_mode =
                                previous.as_ref().is_some_and(|old| old.slow_mode_enabled);
                            if !chat.slow_mode_enabled && previous_slow_mode {
                                ui.slow_mode_requests.borrow_mut().remove(&id);
                                ui.set_slow_mode_wait(id, Duration::ZERO);
                            }
                            if previous.is_some_and(|old| old.forum != chat.forum) {
                                ui.reload_forum_view(chat);
                            } else {
                                ui.set_chat_writability(chat.can_write);
                                if chat.slow_mode_enabled && !previous_slow_mode {
                                    ui.refresh_slow_mode(chat.id);
                                }
                            }
                        }
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
            &self.topic_picker,
            &self.profile_dialog,
            &self.own_profile_dialog,
            &self.poll_dialog,
            &self.stickers_dialog,
            &self.contacts_dialog,
            &self.create_chat_dialog,
            &self.devices_dialog,
            &self.privacy_dialog,
            &self.proxy_dialog,
            &self.blocked_dialog,
            &self.logout_dialog,
            &self.accounts_dialog,
        ]
        .iter()
        .any(|dialog| dialog.upgrade().is_some_and(|dialog| dialog.is_visible()))
    }

    fn open_accounts(self: &Rc<Self>) {
        if self.chat_dialog_open() {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let dialog = self.accounts.chooser(&window, &self.account);
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| ui.accounts_dialog.set(None)
        ));
        self.accounts_dialog.set(Some(&dialog));
    }

    fn confirm_logout(self: &Rc<Self>) {
        if self.logging_out.get() || self.chat_dialog_open() {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        if !demo && (client.is_none() || self.login_step.get() != LoginStep::Authorized) {
            return;
        }
        let confirm = adw::AlertDialog::builder()
            .heading("Выйти из аккаунта?")
            .body(if demo {
                "Деморежим закроется. Демонстрационные черновики будут удалены."
            } else {
                "Сеанс на этом устройстве завершится. Окно аккаунта закроется, локальная сессия и черновики будут удалены. При следующем входе понадобятся ключи приложения и код Telegram."
            })
            .close_response("cancel")
            .default_response("cancel")
            .build();
        confirm.add_response("cancel", "Отмена");
        confirm.add_response("logout", "Выйти");
        confirm.set_response_appearance("logout", adw::ResponseAppearance::Destructive);
        self.logout_dialog.set(Some(confirm.upcast_ref()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let response = confirm.choose_future(Some(&window)).await;
            let Some(ui) = weak.upgrade() else { return };
            ui.logout_dialog.set(None);
            if response != "logout" || ui.window.upgrade().is_none() {
                return;
            }
            if demo {
                window.close();
                return;
            }
            let Some(client) = client else { return };
            if ui.logging_out.replace(true) {
                return;
            }
            let spinner = gtk::Spinner::new();
            spinner.start();
            let progress = adw::AlertDialog::builder()
                .heading("Выходим из аккаунта…")
                .extra_child(&spinner)
                .can_close(false)
                .build();
            ui.logout_dialog.set(Some(progress.upcast_ref()));
            progress.present(Some(&window));
            ui.job(async move { client.logout().await }, move |ui, result| {
                ui.logging_out.set(false);
                progress.force_close();
                ui.logout_dialog.set(None);
                match result {
                    Ok(removal) => {
                        ui.session_removal.replace(Some(removal));
                        if let Some(window) = ui.window.upgrade() {
                            window.close();
                        }
                    }
                    Err(error) => {
                        let failure = adw::AlertDialog::builder()
                            .heading("Не удалось подтвердить выход")
                            .body(format!(
                                "{error}. Проверьте подключение и попробуйте снова."
                            ))
                            .build();
                        failure.add_response("close", "Закрыть");
                        failure.set_close_response("close");
                        failure.set_default_response(Some("close"));
                        failure.connect_closed(glib::clone!(
                            #[weak]
                            ui,
                            move |_| ui.logout_dialog.set(None)
                        ));
                        ui.logout_dialog.set(Some(failure.upcast_ref()));
                        failure.present(ui.window.upgrade().as_ref());
                    }
                }
            });
        });
    }

    fn open_proxy(self: &Rc<Self>) {
        if self.chat_dialog_open() {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let demo = self.is_demo.get().then(|| self.accounts.demo_proxy.clone());
        let dialog = crate::proxy_dialog::open(&window, demo);
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| {
                ui.proxy_dialog.set(None);
                ui.focus_composer();
            }
        ));
        self.proxy_dialog.set(Some(&dialog));
    }

    fn open_privacy(self: &Rc<Self>) {
        if (!self.is_demo.get() && self.login_step.get() != LoginStep::Authorized)
            || self.chat_dialog_open()
        {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let demo_mode = self.is_demo.get();
        let client = self.client.borrow().clone();
        let load_client = client.clone();
        let demo_load = self.demo_privacy.clone();
        let demo_save = self.demo_privacy.clone();
        let dialog = crate::privacy::open(
            &window,
            crate::privacy::PrivacyContext {
                runtime: self.runtime.clone(),
                load: Box::new(move |key| {
                    let local = demo_mode
                        .then(|| demo_load.borrow().get(&key).cloned())
                        .flatten();
                    let client = load_client.clone();
                    Box::pin(async move {
                        if let Some(setting) = local {
                            return Ok(setting);
                        }
                        client
                            .context("Нет подключения к Telegram")?
                            .privacy_setting(key)
                            .await
                    })
                }),
                save: Box::new(move |key, baseline, audience| {
                    let local = demo_mode.then(|| {
                        use crate::model::PrivacyChange;
                        let mut settings = demo_save.borrow_mut();
                        let setting = settings.get_mut(&key).context("Правило отсутствует")?;
                        if *setting != baseline {
                            return Ok(PrivacyChange::Conflict(setting.clone()));
                        }
                        anyhow::ensure!(
                            setting.editable && key.audiences().contains(&audience),
                            "Недоступная аудитория"
                        );
                        setting.audience = Some(audience);
                        Ok(PrivacyChange::Saved {
                            setting: setting.clone(),
                            exceptions_changed: false,
                        })
                    });
                    let client = client.clone();
                    Box::pin(async move {
                        if let Some(result) = local {
                            return result;
                        };
                        client
                            .context("Нет подключения к Telegram")?
                            .set_privacy_setting(key, &baseline, audience)
                            .await
                    })
                }),
            },
        );
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| {
                ui.privacy_dialog.set(None);
                ui.focus_composer();
            }
        ));
        self.privacy_dialog.set(Some(&dialog));
    }

    fn open_devices(self: &Rc<Self>) {
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
        let load_client = client.clone();
        let demo_load = self.demo_devices.clone();
        let demo_reset = self.demo_devices.clone();
        let dialog = crate::devices::open(
            &window,
            crate::devices::DevicesContext {
                runtime: self.runtime.clone(),
                load: Box::new(move || {
                    let snapshot = demo.then(|| demo_load.borrow().clone());
                    let client = load_client.clone();
                    Box::pin(async move {
                        if let Some(snapshot) = snapshot {
                            Ok(snapshot)
                        } else {
                            client
                                .context("Нет подключения к Telegram")?
                                .device_sessions()
                                .await
                        }
                    })
                }),
                change: Box::new(move |change| {
                    use crate::model::{DeviceChange, SESSION_TTL_DAYS};
                    let local = if demo {
                        let mut snapshot = demo_reset.borrow_mut();
                        let invalid = match change {
                            DeviceChange::Terminate(id) => {
                                id == 0
                                    || snapshot
                                        .sessions
                                        .iter()
                                        .any(|session| session.id == id && session.current)
                            }
                            DeviceChange::SetTtl(days) => !SESSION_TTL_DAYS.contains(&days),
                            DeviceChange::TerminateOthers => false,
                        };
                        if invalid {
                            Err(anyhow::anyhow!("Недопустимое изменение сеанса"))
                        } else {
                            snapshot.apply_confirmed(change);
                            Ok(())
                        }
                    } else {
                        Ok(())
                    };
                    let client = client.clone();
                    Box::pin(async move {
                        if demo {
                            local
                        } else {
                            client
                                .context("Нет подключения к Telegram")?
                                .change_device_session(change)
                                .await
                        }
                    })
                }),
            },
        );
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| ui.focus_composer()
        ));
        self.devices_dialog.set(Some(&dialog));
    }

    fn open_create_chat(self: &Rc<Self>) {
        use crate::model::NewChatResult;
        use crate::new_chat::{self, CreateContext};
        let demo = self.is_demo.get();
        if (!demo && self.login_step.get() != LoginStep::Authorized) || self.chat_dialog_open() {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let client = self.client.borrow().clone();
        let create_client = client.clone();
        let publish_client = client.clone();
        let demo_id = self
            .demo_created_chats
            .borrow()
            .keys()
            .min()
            .map_or(-200, |id| id - 1);
        let dialog = new_chat::open(
            &window,
            CreateContext {
                runtime: self.runtime.clone(),
                contacts: Box::new(move || {
                    let client = client.clone();
                    Box::pin(async move {
                        if demo {
                            return Ok(Arc::new(demo::contacts()));
                        }
                        client
                            .context("Нет подключения к Telegram")?
                            .contacts(false)
                            .await
                    })
                }),
                create: Box::new(move |draft| {
                    let client = create_client.clone();
                    Box::pin(async move {
                        if demo {
                            tokio::time::sleep(Duration::from_millis(350)).await;
                            return demo::create_chat(demo_id, draft);
                        }
                        if let Some(client) = client {
                            client.create_chat(draft).await
                        } else {
                            NewChatResult::Rejected("Нет подключения к Telegram".into())
                        }
                    })
                }),
                publish: Box::new(move |chat, username| {
                    let client = publish_client.clone();
                    Box::pin(async move {
                        if demo {
                            anyhow::ensure!(username != "demo_taken", "Это публичное имя занято.");
                            return Ok(());
                        }
                        client
                            .context("Нет подключения к Telegram")?
                            .set_created_chat_username(chat, &username)
                            .await
                    })
                }),
                opened: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |created: crate::model::CreatedChat| {
                        let id = created.chat.id;
                        if demo {
                            ui.demo_created_chats.borrow_mut().insert(id, created.draft);
                        }
                        ui.chat_settings_revision
                            .set(ui.chat_settings_revision.get().wrapping_add(1));
                        if !ui.chats.borrow().iter().any(|chat| chat.id == id) {
                            ui.chats.borrow_mut().push(created.chat.clone());
                        }
                        ui.external_chat.replace(Some(created.chat));
                        ui.filter.set_text("");
                        ui.folder_picker.set_selected(0);
                        ui.archive.set_active(false);
                        ui.render_chats();
                        ui.show_chat(id, None);
                        ui.refresh_chats();
                    }
                )),
                reconcile: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || ui.refresh_chats()
                )),
            },
        );
        self.create_chat_dialog.set(Some(&dialog));
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

    fn block_action(&self) -> crate::blocked::BlockAction {
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        let demo_blocked = self.demo_blocked.clone();
        Rc::new(move |id, blocked| {
            if demo {
                let mut ids = demo_blocked.borrow_mut();
                ids.retain(|peer| *peer != id);
                if blocked {
                    ids.insert(0, id);
                }
                return Box::pin(async { Ok(()) });
            }
            let client = client.clone();
            Box::pin(async move {
                client
                    .context("Нет подключения к Telegram")?
                    .set_blocked(id, blocked)
                    .await
            })
        })
    }

    fn open_blocked(self: &Rc<Self>) {
        if (!self.is_demo.get() && self.login_step.get() != LoginStep::Authorized)
            || self.chat_dialog_open()
        {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let client = self.client.borrow().clone();
        let changes = client.as_ref().map(|client| client.blocked_changes());
        let demo_mode = self.is_demo.get();
        let demo_blocked = self.demo_blocked.clone();
        let dialog = crate::blocked::open(
            &window,
            crate::blocked::BlockedContext {
                runtime: self.runtime.clone(),
                load: Box::new(move |offset| {
                    let page =
                        demo_mode.then(|| demo::blocked_page(&demo_blocked.borrow(), offset));
                    let client = client.clone();
                    Box::pin(async move {
                        if let Some(page) = page {
                            Ok(page)
                        } else {
                            client
                                .context("Нет подключения к Telegram")?
                                .blocked_page(offset)
                                .await
                        }
                    })
                }),
                change: self.block_action(),
                avatar: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |avatar, photo| ui.load_avatar(avatar, ChatImage::Avatar(photo))
                )),
                changes,
            },
        );
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| {
                ui.blocked_dialog.set(None);
                ui.focus_composer();
            }
        ));
        self.blocked_dialog.set(Some(&dialog));
    }

    fn remember_own_profile(self: &Rc<Self>, profile: &crate::model::OwnProfile) {
        let name = profile.details.display_name();
        let new_name = (!name.is_empty()).then_some(name.clone());
        let label = new_name.as_deref().unwrap_or("Мой аккаунт");
        self.accounts.remember_identity(
            &self.account,
            crate::model::AccountIdentity {
                user_id: profile.id,
                name,
                username: profile.username.clone(),
            },
        );
        self.account_label.set_text(label);
        self.account_avatar.set_text(Some(label));
        self.account_avatar.set_visible(true);
        let old_avatar = self.own_avatar.replace(Some(profile.avatar));
        let avatar_changed = old_avatar != Some(profile.avatar);
        if avatar_changed {
            self.account_avatar
                .set_custom_image(None::<&gtk::gdk::Paintable>);
        }
        if let Some(photo) = profile.avatar {
            self.load_chat_image(
                ChatImage::Avatar(photo),
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |texture: &gtk::gdk::Texture| {
                        if ui.own_avatar.get() == Some(Some(photo)) {
                            ui.account_avatar.set_custom_image(Some(texture));
                        }
                    }
                ),
            );
        }
        if matches!(self.account, Account::Saved(_))
            && let Some(window) = self.window.upgrade()
        {
            window.set_title(Some(&format!("tgrust — {label}")));
        }
        let name_changed = *self.own_name.borrow() != new_name;
        if name_changed {
            self.own_name.replace(new_name);
            if self.is_demo.get() {
                let history = self.demo_messages.borrow();
                for chat in self.chats.borrow_mut().iter_mut().filter(|chat| chat.group) {
                    if let Some(last) = history.get(&chat.id).and_then(|messages| messages.last())
                        && last.self_authored
                        && last.id == chat.last_message_id
                    {
                        chat.preview_sender = Some(
                            last.displayed_sender(self.own_name.borrow().as_deref())
                                .to_owned(),
                        );
                    }
                }
                drop(history);
                self.render_chats();
            } else {
                self.refresh_chats();
            }
        }
        if (name_changed || avatar_changed) && self.selection.get().is_some() {
            self.render_history(false);
            self.update_send_button();
        }
    }

    fn open_own_profile(self: &Rc<Self>) {
        if self.chat_dialog_open() {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        if !demo && (client.is_none() || self.login_step.get() != LoginStep::Authorized) {
            return;
        }
        let load_client = client.clone();
        let photo_client = client.clone();
        let load_demo = self.demo_profile.clone();
        let demo_profile = self.demo_profile.clone();
        let demo_photo_profile = self.demo_profile.clone();
        let demo_photo = self.demo_photo.clone();
        let dialog = crate::own_profile::open(
            &window,
            crate::own_profile::ProfileContext {
                runtime: self.runtime.clone(),
                load: Box::new(move || {
                    let local = demo.then(|| load_demo.borrow().clone());
                    let client = load_client.clone();
                    Box::pin(async move {
                        if let Some(local) = local {
                            Ok(local)
                        } else {
                            client
                                .context("Нет подключения к Telegram")?
                                .own_profile()
                                .await
                        }
                    })
                }),
                save: Box::new(move |original, edit| {
                    let local = demo.then(|| {
                        use crate::model::ProfileEditResult;
                        use crate::profile_edit::{Plan, prepare};
                        let mut current = demo_profile.borrow_mut();
                        match prepare(&original, &current, &edit)? {
                            Plan::Conflict => {
                                return Ok(ProfileEditResult::Conflict(current.clone()));
                            }
                            Plan::Write(patch) => patch.apply(&mut current),
                            Plan::Current => {}
                        }
                        Ok(ProfileEditResult::Saved(current.clone()))
                    });
                    let client = client.clone();
                    Box::pin(async move {
                        if let Some(local) = local {
                            local
                        } else {
                            client
                                .context("Нет подключения к Telegram")?
                                .edit_own_profile(&original, edit)
                                .await
                        }
                    })
                }),
                set_photo: Box::new(move |original, jpeg| {
                    let local = demo.then(|| {
                        let mut current = demo_photo_profile.borrow_mut();
                        if current.avatar != original.avatar {
                            return Ok(crate::model::ProfileEditResult::Conflict(current.clone()));
                        }
                        let texture =
                            gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(jpeg.clone()))?;
                        let mut avatar = current.avatar.unwrap_or(demo::OWN_AVATAR);
                        avatar.photo += 1;
                        demo_photo.replace(Some((avatar, texture)));
                        current.avatar = Some(avatar);
                        Ok(crate::model::ProfileEditResult::Saved(current.clone()))
                    });
                    let client = photo_client.clone();
                    Box::pin(async move {
                        if let Some(local) = local {
                            local
                        } else {
                            client
                                .context("Нет подключения к Telegram")?
                                .set_own_photo(&original, jpeg)
                                .await
                        }
                    })
                }),
                load_avatar: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |avatar, photo| ui.load_avatar(avatar, ChatImage::Avatar(photo))
                )),
                changed: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |profile| if ui.window.upgrade().is_some() {
                        ui.remember_own_profile(profile);
                    }
                )),
            },
        );
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| {
                ui.own_profile_dialog.set(None);
                ui.focus_composer();
            }
        ));
        self.own_profile_dialog.set(Some(&dialog));
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
        let edit_client = client.clone();
        let address_client = client.clone();
        let permissions_client = client.clone();
        let forum_client = client.clone();
        let member_client = client.clone();
        let moderation_client = client.clone();
        let restriction_loader = client.clone();
        let restriction_saver = client.clone();
        let demo = self.is_demo.get();
        let demo_chat_info = self.demo_chat_info.clone();
        let demo_chat_addresses = self.demo_chat_addresses.clone();
        let demo_group_permissions = self.demo_group_permissions.clone();
        let demo_created_chats = self.demo_created_chats.clone();
        let demo_members = self.demo_member_changes.clone();
        let demo_blocked = self.demo_blocked.clone();
        let demo_profile = self.demo_profile.clone();
        let profile_ui = Rc::downgrade(self);
        let dialog = crate::profile::open(
            &window,
            crate::profile::ProfileContext {
                runtime: self.runtime.clone(),
                load: Box::new(move || {
                    let blocked = demo && demo_blocked.borrow().contains(&id);
                    let own = demo.then(|| demo_profile.borrow().clone());
                    let client = client.clone();
                    let chat_info = demo_chat_info.borrow().get(&id).cloned();
                    let chat_addresses = demo_chat_addresses.borrow().get(&id).cloned();
                    let group_permissions = demo_group_permissions.borrow().get(&id).copied();
                    let demo_count = demo.then(|| demo::member_count(id, &demo_members.borrow()));
                    let created = demo_created_chats.borrow().get(&id).cloned();
                    let demo_forum = demo
                        && profile_ui
                            .upgrade()
                            .and_then(|ui| ui.chat_view(id))
                            .is_some_and(|chat| chat.forum);
                    let added = demo_members.borrow().invited_count(id);
                    Box::pin(async move {
                        if demo {
                            let mut profile = if let Some(draft) = &created {
                                let mut profile = demo::created_profile(id, draft);
                                profile.members = profile.members.map(|count| count + added);
                                profile
                            } else {
                                let mut profile =
                                    demo::peer_profile(id).context("Чат не найден")?;
                                profile.members = profile.members.and(demo_count);
                                profile
                            };
                            profile.blocked = profile.blocked.map(|_| blocked);
                            if let Some(mode) = &mut profile.forum_mode {
                                mode.enabled = demo_forum;
                            }
                            if let Some(addresses) = chat_addresses.or(profile.addresses.take()) {
                                profile.usernames = addresses.active_usernames();
                                profile.addresses = Some(addresses);
                            }
                            if let Some(original) = profile.group_permissions {
                                let public =
                                    profile.addresses.as_ref().is_some_and(|a| a.is_public());
                                let saved = group_permissions.unwrap_or(GroupPermissionEdit {
                                    allowed: GroupPermissions::all(),
                                    slow_mode: original.slow_mode,
                                });
                                profile.group_permissions =
                                    Some(crate::model::GroupPermissionSettings {
                                        slow_mode: saved.slow_mode,
                                        forum: demo_forum,
                                        ..demo::group_permissions(saved.allowed, public)
                                    });
                            }
                            if let Some(info) = chat_info {
                                profile.title = info.title;
                                profile.about = info.about;
                            }
                            if let Some(own) = own.filter(|_| profile.is_self) {
                                profile.title = own.details.display_name();
                                profile.about = own.details.about;
                                profile.avatar = own.avatar;
                                profile.usernames = (!own.username.is_empty())
                                    .then(|| format!("@{}", own.username))
                                    .into_iter()
                                    .collect();
                            }
                            Ok(profile)
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
                set_blocked: self.block_action(),
                edit_chat: Box::new({
                    let demo_info = self.demo_chat_info.clone();
                    move |original, proposed| {
                        let client = edit_client.clone();
                        let current = demo_info
                            .borrow()
                            .get(&id)
                            .cloned()
                            .unwrap_or_else(|| original.clone());
                        Box::pin(async move {
                            if demo {
                                let (title, about) = proposed.changes(&original, &current)?;
                                return Ok(ChatInfo {
                                    title: title.unwrap_or(current.title),
                                    about: about.unwrap_or(current.about),
                                });
                            }
                            client
                                .context("Нет подключения к Telegram")?
                                .edit_chat_info(id, original, proposed)
                                .await
                        })
                    }
                }),
                chat_saved: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |info| ui.apply_chat_info(id, info)
                )),
                set_forum_mode: Box::new(move |confirmed| {
                    let client = forum_client.clone();
                    Box::pin(async move {
                        if demo {
                            Ok(!confirmed.enabled)
                        } else {
                            client
                                .context("Нет подключения к Telegram")?
                                .set_forum_mode(id, confirmed)
                                .await
                        }
                    })
                }),
                forum_mode_changed: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |enabled| ui.apply_forum_mode(id, enabled)
                )),
                edit_addresses: Box::new({
                    let demo_addresses = self.demo_chat_addresses.clone();
                    move |original, proposed| {
                        let client = address_client.clone();
                        if demo {
                            let mut demo_addresses = demo_addresses.borrow_mut();
                            let current =
                                demo_addresses.entry(id).or_insert_with(|| original.clone());
                            let result = proposed.changes(&original, current).and_then(|changes| {
                                for change in changes {
                                    if matches!(&change, crate::model::ChatAddressChange::Primary(name) if name == "demo_taken") {
                                        anyhow::bail!("Это публичное имя занято.");
                                    }
                                    current.apply(change);
                                }
                                Ok(())
                            });
                            return Box::pin(async { result });
                        }
                        Box::pin(async move {
                            client
                                .context("Нет подключения к Telegram")?
                                .edit_chat_addresses(id, original, proposed)
                                .await
                        })
                    }
                }),
                edit_group_permissions: Box::new({
                    let demo_permissions = self.demo_group_permissions.clone();
                    move |original, requested| {
                        let client = permissions_client.clone();
                        if demo {
                            let mut permissions = demo_permissions.borrow_mut();
                            let saved = permissions.entry(id).or_insert(GroupPermissionEdit {
                                allowed: GroupPermissions::all(),
                                slow_mode: original.slow_mode,
                            });
                            let mut current = crate::model::GroupPermissionSettings {
                                allowed: saved.allowed,
                                slow_mode: saved.slow_mode,
                                ..original
                            };
                            let result =
                                current
                                    .changes(original, requested.allowed)
                                    .and_then(|changes| {
                                        let interval = slow_mode_change(
                                            current.slow_mode,
                                            original.slow_mode,
                                            requested.slow_mode,
                                        )?;
                                        current.apply(&changes);
                                        saved.allowed = current.allowed;
                                        if let Some(seconds) = interval {
                                            saved.slow_mode = Some(seconds);
                                        }
                                        Ok(())
                                    });
                            return Box::pin(async { result });
                        }
                        Box::pin(async move {
                            client
                                .context("Нет подключения к Telegram")?
                                .edit_group_permissions(id, original, requested)
                                .await
                        })
                    }
                }),
                members: Box::new({
                    let demo_members = self.demo_member_changes.clone();
                    let demo_created_chats = self.demo_created_chats.clone();
                    move |filter, offset| {
                        let client = member_client.clone();
                        let local = demo.then(|| {
                            if let Some(draft) = demo_created_chats.borrow().get(&id) {
                                demo::created_members(
                                    id,
                                    draft,
                                    &demo_members.borrow(),
                                    filter,
                                    offset,
                                )
                            } else {
                                demo::members(id, &demo_members.borrow(), filter, offset)
                            }
                        });
                        Box::pin(async move {
                            if let Some(page) = local {
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                return Ok(page);
                            }
                            client
                                .context("Нет подключения к Telegram")?
                                .chat_members(id, filter, offset)
                                .await
                        })
                    }
                }),
                moderate: Box::new({
                    let demo_members = self.demo_member_changes.clone();
                    move |member, change| {
                        let client = moderation_client.clone();
                        let local = demo.then(|| {
                            demo::change_member(&mut demo_members.borrow_mut(), id, member, change)
                        });
                        Box::pin(async move {
                            if let Some(result) = local {
                                return result;
                            }
                            client
                                .context("Нет подключения к Telegram")?
                                .change_chat_member(id, member, change)
                                .await
                        })
                    }
                }),
                member_restriction: Box::new({
                    let members = self.demo_member_changes.clone();
                    let permissions = self.demo_group_permissions.clone();
                    move |member| {
                        let client = restriction_loader.clone();
                        let local = demo.then(|| {
                            let allowed = permissions
                                .borrow()
                                .get(&id)
                                .map_or_else(GroupPermissions::all, |settings| settings.allowed);
                            demo::member_restriction(&members.borrow(), id, member, allowed)
                        });
                        Box::pin(async move {
                            if let Some(result) = local {
                                return result;
                            }
                            client
                                .context("Нет подключения к Telegram")?
                                .member_restriction(id, member)
                                .await
                        })
                    }
                }),
                edit_member_restriction: Box::new({
                    let members = self.demo_member_changes.clone();
                    let permissions = self.demo_group_permissions.clone();
                    move |member, original, edit| {
                        let client = restriction_saver.clone();
                        let local = demo.then(|| {
                            let allowed = permissions
                                .borrow()
                                .get(&id)
                                .map_or_else(GroupPermissions::all, |settings| settings.allowed);
                            demo::edit_member_restriction(
                                &mut members.borrow_mut(),
                                id,
                                member,
                                allowed,
                                original,
                                edit,
                            )
                        });
                        Box::pin(async move {
                            if let Some(result) = local {
                                return result;
                            }
                            client
                                .context("Нет подключения к Telegram")?
                                .edit_member_restriction(id, member, original, edit)
                                .await
                        })
                    }
                }),
                invite_members: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |anchor, allow_bots, changed| ui
                        .open_invite_members(id, anchor, allow_bots, changed)
                )),
                invites: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |button| ui.open_invite_links(id, button)
                )),
                join_requests: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |button| ui.open_join_requests(id, button)
                )),
                admin_log: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |button| ui.open_admin_log(id, button)
                )),
                command: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |command| {
                        if ui.selection.get() == Some(id) && ui.input.is_sensitive() {
                            let buffer = ui.input.buffer();
                            let mut end = buffer.end_iter();
                            let text = buffer.text(&buffer.start_iter(), &end, false);
                            if !text.is_empty() && !text.ends_with('\n') {
                                buffer.insert(&mut end, "\n");
                            }
                            buffer.insert(&mut end, &command);
                            buffer.place_cursor(&end);
                            ui.focus_composer();
                        }
                    }
                )),
                edit: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || {
                        ui.profile_dialog.set(None);
                        ui.open_own_profile();
                    }
                )),
            },
        );
        self.profile_dialog.set(Some(&dialog));
    }

    fn open_invite_members(
        &self,
        chat: i64,
        anchor: &gtk::Button,
        allow_bots: bool,
        changed: Box<dyn Fn()>,
    ) {
        use crate::invite_members::{self, InviteContext};
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        let write = client.clone();
        let demo_members = self.demo_member_changes.clone();
        let demo_created = self.demo_created_chats.clone();
        invite_members::open(
            anchor,
            allow_bots,
            InviteContext {
                runtime: self.runtime.clone(),
                contacts: Box::new(move || {
                    let client = client.clone();
                    Box::pin(async move {
                        if demo {
                            return Ok(Arc::new(demo::invitable_contacts()));
                        }
                        client
                            .context("Нет подключения к Telegram")?
                            .contacts(false)
                            .await
                    })
                }),
                invite: Box::new(move |contacts| {
                    let client = write.clone();
                    if demo {
                        let result = demo::invite_members(
                            chat,
                            demo_created.borrow().get(&chat),
                            contacts,
                            &mut demo_members.borrow_mut(),
                        );
                        return Box::pin(async { result });
                    }
                    Box::pin(async move {
                        client
                            .context("Нет подключения к Telegram")?
                            .invite_members(chat, contacts)
                            .await
                    })
                }),
                changed,
            },
        );
    }

    fn open_invite_links(&self, chat: i64, anchor: &gtk::Button) {
        use crate::invite_links::{self, InviteContext};
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        let write_client = client.clone();
        let links = self.demo_invites.clone();
        let edit_links = links.clone();
        invite_links::open(
            anchor,
            InviteContext {
                runtime: self.runtime.clone(),
                load: Box::new(move |revoked, cursor| {
                    let client = client.clone();
                    let local =
                        demo.then(|| links.borrow_mut().page(chat, revoked, cursor.clone()));
                    Box::pin(async move {
                        if let Some(page) = local {
                            return Ok(page);
                        }
                        client
                            .context("Нет подключения к Telegram")?
                            .invite_links(chat, revoked, cursor)
                            .await
                    })
                }),
                write: Box::new(move |action| {
                    let client = write_client.clone();
                    if demo {
                        let changed = edit_links.borrow_mut().change(chat, action);
                        return Box::pin(async { changed });
                    }
                    Box::pin(async move {
                        client
                            .context("Нет подключения к Telegram")?
                            .change_invite(chat, action)
                            .await
                    })
                }),
            },
        );
    }

    fn open_join_requests(self: &Rc<Self>, chat: i64, anchor: &gtk::Button) {
        use crate::join_requests::{self, JoinRequestsContext};
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        let write_client = client.clone();
        let updates = client.as_ref().map(|client| client.join_request_changes());
        let requests = self.demo_join_requests.clone();
        let decisions = requests.clone();
        join_requests::open(
            anchor,
            chat,
            JoinRequestsContext {
                runtime: self.runtime.clone(),
                load: Box::new(move |after| {
                    let client = client.clone();
                    let demo_page = demo.then(|| requests.borrow().page(chat, after));
                    Box::pin(async move {
                        if let Some(page) = demo_page {
                            return Ok(page);
                        }
                        client
                            .context("Нет подключения к Telegram")?
                            .join_requests(chat, after)
                            .await
                    })
                }),
                decide: Box::new(move |user, decision| {
                    let client = write_client.clone();
                    let demo_result = demo.then(|| decisions.borrow_mut().decide(chat, user));
                    Box::pin(async move {
                        if let Some(result) = demo_result {
                            return Ok(result);
                        }
                        client
                            .context("Нет подключения к Telegram")?
                            .decide_join_request(chat, user, decision)
                            .await
                    })
                }),
                avatar: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |avatar, photo| ui.load_avatar(avatar, ChatImage::Avatar(photo))
                )),
                updates,
            },
        );
    }

    fn open_admin_log(self: &Rc<Self>, chat: i64, anchor: &gtk::Button) {
        use crate::admin_log::{self, AdminLogContext};
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        admin_log::open(
            anchor,
            AdminLogContext {
                runtime: self.runtime.clone(),
                load: Box::new(move |query, before| {
                    let client = client.clone();
                    Box::pin(async move {
                        if demo {
                            tokio::time::sleep(Duration::from_millis(180)).await;
                            return Ok(demo::admin_log(chat, query, before));
                        }
                        client
                            .context("Нет подключения к Telegram")?
                            .admin_log(chat, query, before)
                            .await
                    })
                }),
                avatar: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |avatar, photo| ui.load_avatar(avatar, ChatImage::Avatar(photo))
                )),
            },
        );
    }

    fn apply_chat_info(self: &Rc<Self>, id: i64, info: ChatInfo) {
        if self.is_demo.get() {
            self.demo_chat_info.borrow_mut().insert(id, info.clone());
        }
        if let Some(chat) = self
            .chats
            .borrow_mut()
            .iter_mut()
            .find(|chat| chat.id == id)
        {
            chat.title.clone_from(&info.title);
        }
        if let Some(chat) = self
            .external_chat
            .borrow_mut()
            .as_mut()
            .filter(|chat| chat.id == id)
        {
            chat.title.clone_from(&info.title);
        }
        if self.selection.get() == Some(id) {
            self.title.set_title(&info.title);
        }
        self.chat_settings_revision
            .set(self.chat_settings_revision.get().wrapping_add(1));
        self.render_chats();
    }

    fn apply_forum_mode(self: &Rc<Self>, id: i64, enabled: bool) {
        let changed = self.chat_view(id).is_some_and(|chat| chat.forum != enabled);
        if let Some(chat) = self
            .chats
            .borrow_mut()
            .iter_mut()
            .find(|chat| chat.id == id)
        {
            chat.forum = enabled;
        }
        if let Some(chat) = self
            .external_chat
            .borrow_mut()
            .as_mut()
            .filter(|chat| chat.id == id)
        {
            chat.forum = enabled;
        }
        self.chat_settings_revision
            .set(self.chat_settings_revision.get().wrapping_add(1));
        if changed {
            if self.selection.get() == Some(id)
                && let Some(chat) = self.chat_view(id)
            {
                self.reload_forum_view(chat);
            } else {
                self.history_cache.borrow_mut().remove(id);
            }
        }
        self.render_chats();
        self.refresh_chats();
    }

    fn reload_forum_view(self: &Rc<Self>, chat: Chat) {
        self.history_cache.borrow_mut().remove(chat.id);
        self.history_refreshed.set(None);
        if let Some(picker) = self.topic_picker.upgrade() {
            picker.close();
        }
        self.display_conversation(chat, None, None, self.split.shows_content());
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
                        let forum = hit.chat.forum;
                        ui.external_chat.replace(Some(hit.chat));
                        if forum {
                            ui.open_forum_message(
                                id,
                                Some(hit.message.topic.unwrap_or(GENERAL_TOPIC)),
                                hit.message.id,
                            );
                        } else {
                            ui.show_chat(id, Some(hit.message.id));
                        }
                    }
                )),
            },
            scoped,
        );
        self.message_search.set(Some(&dialog));
    }

    fn show_chat(self: &Rc<Self>, id: i64, anchor: Option<i32>) {
        let Some(chat) = self.chat_view(id) else {
            return;
        };
        let forum = chat.forum;
        self.show_conversation(chat, None, if forum { None } else { anchor });
        if forum {
            self.open_current_topics();
        }
    }

    fn chat_view(&self, id: i64) -> Option<Chat> {
        self.chats
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
    }

    fn conversation(&self) -> Option<Conversation> {
        if self.content.visible_child_name().as_deref() != Some("conversation") {
            return None;
        }
        Some(Conversation {
            chat: self.selection.get()?,
            topic: self.selected_topic.borrow().as_ref().map(|topic| topic.id),
        })
    }

    fn reopen_conversation(self: &Rc<Self>, anchor: Option<i32>) {
        if let Some(chat) = self.selection.get().and_then(|id| self.chat_view(id)) {
            let topic = self.selected_topic.borrow().clone();
            self.show_conversation(chat, topic, anchor);
        }
    }

    fn open_current_topics(self: &Rc<Self>) {
        let Some(chat) = self
            .selection
            .get()
            .filter(|id| self.chat_view(*id).is_some_and(|chat| chat.forum))
        else {
            return;
        };
        self.choose_forum_topic(chat, true, move |ui, topic| {
            if ui.selection.get() == Some(chat)
                && let Some(chat) = ui.chat_view(chat).filter(|view| view.forum)
            {
                ui.show_conversation(chat, Some(topic), None);
            }
        });
    }

    fn choose_forum_topic(
        self: &Rc<Self>,
        chat: i64,
        manage: bool,
        choose: impl Fn(&Rc<Self>, ForumTopic) + 'static,
    ) {
        if self.chat_dialog_open() {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        let read_topics = self.demo_read_topics.clone();
        let demo_forums = self.demo_forums.clone();
        let management = manage.then(|| {
            let client = client.clone();
            let demo_forums = demo_forums.clone();
            crate::forum_topics::Management {
                write: Box::new(move |write| {
                    let client = client.clone();
                    let write = if demo {
                        let result = demo_forums
                            .borrow_mut()
                            .entry(chat)
                            .or_insert_with(|| demo::ForumTopics::for_chat(chat))
                            .write(write);
                        return Box::pin(async move { result });
                    } else {
                        write
                    };
                    Box::pin(async move {
                        client
                            .context("Нет подключения к Telegram")?
                            .write_forum_topic(chat, write)
                            .await
                    })
                }),
                changed: Box::new(glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move |result| ui.apply_forum_change(chat, result)
                )),
            }
        });
        let picker = crate::forum_topics::open(
            &window,
            self.runtime.clone(),
            move |cursor| {
                let client = client.clone();
                let sample = demo.then(|| {
                    let mut page = demo_forums
                        .borrow_mut()
                        .entry(chat)
                        .or_insert_with(|| demo::ForumTopics::for_chat(chat))
                        .page(cursor);
                    let read = read_topics.borrow();
                    for topic in &mut page.topics {
                        if read.contains(&Conversation {
                            chat,
                            topic: Some(topic.id),
                        }) {
                            topic.unread = 0;
                        }
                    }
                    page
                });
                Box::pin(async move {
                    if let Some(page) = sample {
                        return Ok(page);
                    }
                    client
                        .context("Нет подключения к Telegram")?
                        .forum_topics(chat, cursor)
                        .await
                })
            },
            management,
            glib::clone!(
                #[weak(rename_to = ui)]
                self,
                move |topic| choose(&ui, topic)
            ),
        );
        self.topic_picker.set(Some(&picker));
    }

    fn apply_forum_change(self: &Rc<Self>, chat: i64, result: &ForumWriteResult) {
        match result {
            ForumWriteResult::Updated(topic) => {
                let target = Conversation {
                    chat,
                    topic: Some(topic.id),
                };
                if self.conversation() == Some(target) {
                    self.title.set_subtitle(&topic.title);
                    self.selected_topic.replace(Some(topic.clone()));
                    self.set_chat_writability(
                        self.chat_view(chat).is_some_and(|chat| chat.can_write),
                    );
                }
            }
            ForumWriteResult::Deleted(id) => {
                let target = Conversation {
                    chat,
                    topic: Some(*id),
                };
                if self.conversation() == Some(target)
                    && let Some(chat) = self.chat_view(chat)
                {
                    self.show_conversation(chat, None, None);
                }
                self.history_cache.borrow_mut().remove_conversation(target);
                self.drafts.borrow_mut().remove(&target);
                self.replies.borrow_mut().remove(&target);
                self.uploads.borrow_mut().remove(&target);
                if self.is_demo.get() {
                    self.demo_read_topics.borrow_mut().remove(&target);
                    let mut histories = self.demo_messages.borrow_mut();
                    let history = histories.entry(chat).or_insert_with(|| demo::history(chat));
                    history.retain(|message| message.topic != target.topic);
                    if let Some(view) = self
                        .chats
                        .borrow_mut()
                        .iter_mut()
                        .find(|item| item.id == chat)
                    {
                        let last = history.last();
                        view.preview = last.map_or_else(String::new, |message| {
                            message.preview_text().into_owned()
                        });
                        view.preview_sender = last.map(|message| message.sender.clone());
                        view.preview_image = last.and_then(|message| {
                            message
                                .thumbnail
                                .map(|media| ChatImage::attachment(chat, message.id, media))
                        });
                        view.last_message_id = last.map_or(0, |message| message.id);
                    }
                    drop(histories);
                    self.render_chats();
                }
            }
            ForumWriteResult::Created(_) | ForumWriteResult::CreationUnconfirmed => {}
        }
        self.refresh_chats();
    }

    fn open_forum_message(self: &Rc<Self>, chat: i64, topic: Option<i32>, anchor: i32) {
        let Some(view) = self.chat_view(chat) else {
            return;
        };
        if self.is_demo.get() {
            let topic = topic.or_else(|| {
                let mut history = self.demo_messages.borrow_mut();
                history
                    .entry(chat)
                    .or_insert_with(|| demo::history(chat))
                    .iter()
                    .find(|message| message.id == anchor)
                    .map(|message| message.topic.unwrap_or(GENERAL_TOPIC))
            });
            let topic = topic.and_then(|id| {
                self.demo_forums
                    .borrow_mut()
                    .entry(chat)
                    .or_insert_with(|| demo::ForumTopics::for_chat(chat))
                    .topic(id)
            });
            if let Some(topic) = topic {
                self.show_conversation(view, Some(topic), Some(anchor));
            }
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let generation = self.generation.get();
        let jump = self.forum_jump.get().wrapping_add(1);
        self.forum_jump.set(jump);
        self.job(
            async move {
                tokio::time::timeout(Duration::from_secs(20), async {
                    let topic = match topic {
                        Some(topic) => topic,
                        None => client.forum_topic_for_message(chat, anchor).await?,
                    };
                    client.forum_topic(chat, topic).await
                })
                .await
                .context("Telegram не прислал тему вовремя")?
            },
            move |ui, result| {
                if ui.generation.get() != generation || ui.forum_jump.get() != jump {
                    return;
                }
                match result {
                    Ok(topic) => ui.show_conversation(view, Some(topic), Some(anchor)),
                    Err(error) => ui.toast(&format!("Не удалось открыть тему. {error}")),
                }
            },
        );
    }

    fn show_conversation(
        self: &Rc<Self>,
        chat: Chat,
        topic: Option<ForumTopic>,
        anchor: Option<i32>,
    ) {
        self.display_conversation(chat, topic, anchor, true);
    }

    fn display_conversation(
        self: &Rc<Self>,
        chat: Chat,
        topic: Option<ForumTopic>,
        anchor: Option<i32>,
        reveal: bool,
    ) {
        let id = chat.id;
        let target = Conversation {
            chat: id,
            topic: topic.as_ref().map(|topic| topic.id),
        };
        let overview = chat.forum && topic.is_none();
        let visible = reveal || !self.split.is_collapsed();
        let previous = self.conversation();
        if visible
            && self.is_demo.get()
            && anchor.is_none()
            && let Some(topic) = &topic
            && self.demo_read_topics.borrow_mut().insert(target)
            && let Some(chat) = self
                .chats
                .borrow_mut()
                .iter_mut()
                .find(|chat| chat.id == id)
        {
            chat.unread = (chat.unread - topic.unread).max(0);
        }
        if visible {
            self.withdraw_chat_notification(id);
        }
        if chat.slow_mode_enabled {
            self.refresh_slow_mode(id);
        }
        if previous != Some(target) || anchor.is_some() {
            self.inline_audio.take();
        }
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
        self.title.set_subtitle(
            topic
                .as_ref()
                .map_or(if chat.broadcast { "Канал" } else { "" }, |topic| {
                    topic.title.as_str()
                }),
        );
        self.topics_button.set_visible(chat.forum);
        if reveal {
            self.split.set_show_content(true);
        }
        let client = self.client.borrow().clone();
        if visible
            && !chat.forum
            && self.unread_overrides.borrow_mut().remove(&id)
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
        if !overview
            && previous == Some(target)
            && self.loaded.get()
            && anchor.is_none()
            && self.search_anchor.get().is_none()
        {
            self.selected_topic.replace(topic);
            self.set_chat_writability(chat.can_write);
            if visible {
                self.focus_composer();
            }
            if self.is_demo.get()
                || self.history_refreshed.get().is_some_and(|refreshed| {
                    self.history_revision
                        .get()
                        .is_fresh(refreshed, revision, now)
                })
            {
                if visible {
                    self.mark_cached_conversation_read(&chat, target);
                }
            } else {
                self.load_history(None, visible);
            }
            return;
        }
        if let Some(previous) = previous {
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
        self.selection.set(Some(id));
        self.selected_topic.replace(topic);
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
        self.older_history_pending.set(false);
        self.history_scroll.set(None);
        self.prepend_scroll.set(None);
        self.loaded.set(false);
        self.has_older.set(false);
        self.messages.borrow_mut().clear();
        self.revealed_spoilers.borrow_mut().clear();
        self.history_refreshed.set(None);
        self.content
            .set_visible_child_name(if overview { "forum" } else { "conversation" });
        let buffer = self.input.buffer();
        if overview {
            buffer.set_text("");
            self.set_chat_writability(false);
            if visible {
                self.open_topics_button.grab_focus();
            }
            return;
        }
        if let Some(draft) = self.drafts.borrow().get(&target) {
            set_editor_text(&buffer, draft);
        } else {
            buffer.set_text("");
        }
        buffer.place_cursor(&buffer.end_iter());
        self.set_chat_writability(chat.can_write);
        let cached = if anchor.is_none() {
            self.history_cache.borrow_mut().restore(target, now)
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
            if self.is_demo.get() && chat.forum {
                self.render_chats();
            }
            if fresh {
                if visible {
                    self.mark_cached_conversation_read(&chat, target);
                }
            } else {
                self.load_history(None, visible);
            }
        } else if self.is_demo.get() {
            let (messages, has_older, _) = self.demo_history(target, None, anchor);
            self.messages
                .borrow_mut()
                .extend(messages.into_iter().map(|message| (message.id, message)));
            if visible
                && !chat.forum
                && let Some(chat) = self
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
            self.load_history(None, visible);
        }
        if visible && anchor.is_none() {
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

    fn mark_cached_conversation_read(self: &Rc<Self>, chat: &Chat, target: Conversation) {
        let Some(topic) = target.topic else {
            self.mark_cached_read(chat);
            return;
        };
        if self.unread_overrides.borrow().contains(&chat.id) {
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let Some((&max_id, _)) = self.messages.borrow().last_key_value() else {
            return;
        };
        self.job(
            async move { client.read_forum_topic(target.chat, topic, max_id).await },
            move |ui, result| {
                if result.is_ok_and(|read| read) {
                    ui.refresh_chats();
                }
            },
        );
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
        target: Conversation,
        before: Option<i32>,
        anchor: Option<i32>,
    ) -> (Vec<Message>, bool, bool) {
        let mut history = self.demo_messages.borrow_mut();
        let messages = history
            .entry(target.chat)
            .or_insert_with(|| demo::history(target.chat));
        let messages: Vec<_> = messages
            .iter()
            .filter(|message| {
                target
                    .topic
                    .is_none_or(|topic| message.topic.unwrap_or(GENERAL_TOPIC) == topic)
            })
            .collect();
        let end = if let Some(before) = before {
            messages.partition_point(|message| message.id < before)
        } else if let Some(anchor) = anchor {
            (messages.partition_point(|message| message.id <= anchor) + HISTORY_PAGE_SIZE / 2)
                .min(messages.len())
        } else {
            messages.len()
        };
        let start = end.saturating_sub(HISTORY_PAGE_SIZE);
        (
            messages[start..end]
                .iter()
                .map(|message| (*message).clone())
                .collect(),
            start > 0,
            false,
        )
    }

    fn load_older_near_top(self: &Rc<Self>) {
        if self.history_scroll.get().is_some() || !self.has_older.get() || !self.scroll.is_mapped()
        {
            return;
        }
        let adjustment = self.scroll.vadjustment();
        if adjustment.value() - adjustment.lower() <= 180.0 {
            if self.history_refresh.get() {
                self.older_history_pending.set(true);
            } else {
                let before = self.messages.borrow().first_key_value().map(|(&id, _)| id);
                if before.is_some() {
                    self.load_history(before, false);
                }
            }
        }
    }

    fn continue_history(self: &Rc<Self>) {
        if self.history_refresh.get() || self.history_scroll.get().is_some() {
            return;
        }
        if self.older_history_pending.replace(false) {
            self.load_older_near_top();
            if self.history_refresh.get() {
                return;
            }
        }
        if let Some(mark_read) = self.latest_history_pending.take() {
            self.load_history(
                None,
                mark_read && (!self.split.is_collapsed() || self.split.shows_content()),
            );
        }
    }

    fn history_position(&self) -> HistoryScroll {
        let value = self.scroll.vadjustment().value();
        let mut child = self.history.first_child();
        while let Some(widget) = child {
            if let Some(id) = widget
                .widget_name()
                .strip_prefix("message-")
                .and_then(|id| id.parse().ok())
                && let Some(bounds) = widget.compute_bounds(&self.history)
                && f64::from(bounds.y() + bounds.height()) > value
            {
                return HistoryScroll::Message {
                    id,
                    offset: f64::from(bounds.y()) - value,
                };
            }
            child = widget.next_sibling();
        }
        HistoryScroll::Value(value)
    }

    fn history_at_bottom(&self) -> bool {
        let adjustment = self.scroll.vadjustment();
        adjustment.upper() - adjustment.page_size() - adjustment.value() < 36.0
    }

    fn restore_history_position(&self, position: HistoryScroll) {
        let adjustment = self.scroll.vadjustment();
        match position {
            HistoryScroll::Bottom => adjustment.set_value(adjustment.upper()),
            HistoryScroll::Value(value) => adjustment.set_value(value),
            HistoryScroll::Message { id, offset } => {
                let nearest = || {
                    let messages = self.messages.borrow();
                    messages
                        .range(id..)
                        .next()
                        .or_else(|| messages.last_key_value())
                        .map(|(&id, _)| id)
                };
                if let Some(bounds) = self
                    .message_bubble(id)
                    .or_else(|| nearest().and_then(|id| self.message_bubble(id)))
                    .and_then(|widget| widget.compute_bounds(&self.history))
                {
                    adjustment.set_value(f64::from(bounds.y()) - offset);
                }
            }
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
        let Some(target) = self.conversation() else {
            return;
        };
        let id = target.chat;
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
        let poll_revision = self.poll_revision.get();
        let revision = client
            .as_ref()
            .map_or_else(HistoryRevision::default, |client| {
                client.history_revision(id)
            });
        let demo_page = demo.then(|| self.demo_history(target, before, anchor));
        let visible = if before.is_none()
            && anchor.is_none()
            && self.loaded.get()
            && !self.history_at_bottom()
        {
            match self
                .history_scroll
                .get()
                .unwrap_or_else(|| self.history_position())
            {
                HistoryScroll::Message { id, .. } => Some(id),
                _ => None,
            }
        } else {
            None
        };
        let keep_older = revision.can_keep_older(self.history_revision.get());
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
                    if before.is_some() {
                        tokio::time::sleep(Duration::from_millis(180)).await;
                    }
                    return Ok(page);
                }
                let client = client.expect("Telegram history requires a client");
                tokio::time::timeout(Duration::from_secs(25), async {
                    if target.topic.is_none()
                        && before.is_none()
                        && let Some(anchor) = anchor
                    {
                        let (messages, older) = client.history_around(id, anchor).await?;
                        return Ok((messages, older, false));
                    }
                    let (messages, mut older, marked) = if let Some(topic) = target.topic {
                        client
                            .forum_history(
                                id,
                                topic,
                                before,
                                anchor.filter(|_| before.is_none()),
                                read.is_some(),
                            )
                            .await?
                    } else {
                        client.history(id, before, read).await?
                    };
                    let needs_more = visible.filter(|visible| {
                        messages.first().is_some_and(|first| {
                            *visible < first.id
                                && (!keep_older || requested_through.is_none_or(|id| id < first.id))
                        })
                    });
                    let Some(visible) = needs_more else {
                        return Ok((messages, older, marked));
                    };
                    // A burst of updates can move the latest page past the loaded range.
                    // Refill the gap before replacing the part of history being read.
                    let mut pages = vec![messages];
                    while let Some(oldest) = pages.last().and_then(|page| page.first())
                        && older
                        && oldest.id > visible
                    {
                        let (page, has_older, _) = if let Some(topic) = target.topic {
                            client
                                .forum_history(id, topic, Some(oldest.id), None, false)
                                .await?
                        } else {
                            client.history(id, Some(oldest.id), None).await?
                        };
                        older = has_older && !page.is_empty();
                        pages.push(page);
                    }
                    Ok((pages.into_iter().rev().flatten().collect(), older, marked))
                })
                .await
                .context("Telegram не прислал историю вовремя")?
            },
            move |ui, result| {
                if generation != ui.generation.get() {
                    return;
                }
                ui.history_refresh.set(false);
                ui.older.set_sensitive(true);
                match result {
                    Ok((mut messages, has_older, marked)) => {
                        // A result pushed while the history request was in flight can be newer than the page.
                        let recent = ui.recent_polls.borrow();
                        for message in &mut messages {
                            if let Some(poll) = &mut message.poll {
                                for (revision, update) in &*recent {
                                    if *revision > poll_revision {
                                        update.apply(id, message.id, poll);
                                    }
                                }
                            }
                        }
                        drop(recent);
                        let was_loaded = ui.loaded.replace(true);
                        let at_bottom = ui.history_at_bottom();
                        let mut current = ui.messages.borrow_mut();
                        if let Some(before) = before {
                            if ui.history_scroll.get().is_none() {
                                let adjustment = ui.scroll.vadjustment();
                                ui.prepend_scroll
                                    .set(Some((adjustment.upper(), adjustment.value())));
                            }
                            ui.has_older.set(prepend_history_page(
                                &mut current,
                                messages,
                                before,
                                has_older,
                            ));
                        } else {
                            ui.history_revision.set(revision);
                            if anchor.is_none() {
                                ui.history_refreshed.set(Some(Instant::now()));
                            }
                            let older_loaded = merge_latest_history(
                                &mut current,
                                messages,
                                has_older && keep_older,
                                requested_through,
                            );
                            if !older_loaded {
                                ui.has_older.set(has_older);
                            }
                        }
                        drop(current);
                        ui.render_history(
                            anchor.is_none() && before.is_none() && (!was_loaded || at_bottom),
                        );
                        if marked && has_unread {
                            ui.refresh_chats();
                        }
                    }
                    Err(error) => {
                        ui.older_history_pending.set(false);
                        if !ui.loaded.get() {
                            while let Some(child) = ui.history.first_child() {
                                ui.history.remove(&child);
                            }
                            let message = label("Не удалось загрузить историю", "dim-label");
                            message.set_halign(gtk::Align::Center);
                            ui.history.append(&message);
                            let retry = gtk::Button::with_label("Повторить загрузку");
                            retry.set_halign(gtk::Align::Center);
                            retry.connect_clicked(glib::clone!(
                                #[weak]
                                ui,
                                move |_| ui.load_history(None, true)
                            ));
                            ui.history.append(&retry);
                        }
                        ui.toast(&format!("Не удалось загрузить историю. {error}"));
                    }
                }
                ui.continue_history();
            },
        );
    }

    fn render_history(self: &Rc<Self>, bottom: bool) {
        let position = if bottom {
            HistoryScroll::Bottom
        } else {
            self.history_scroll
                .get()
                .unwrap_or_else(|| self.history_position())
        };
        self.set_history_position(position);
        if self.inline_audio.borrow().as_ref().is_some_and(|audio| {
            self.messages
                .borrow()
                .get(&audio.id)
                .is_none_or(|message| message.ephemeral || message.playback != Some(audio.media))
        }) {
            self.inline_audio.take();
        }
        self.audio_cards.borrow_mut().clear();
        while let Some(child) = self.history.first_child() {
            self.history.remove(&child);
        }
        if self.has_older.get() {
            self.history.append(&self.older);
        }
        let messages = self.messages.borrow();
        let own_name = self.own_name.borrow();
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
            let sender = message.displayed_sender(own_name.as_deref());
            let avatar = adw::Avatar::new(24, Some(sender), true);
            let photo = if message.self_authored {
                self.own_avatar.get().unwrap_or(message.sender_avatar)
            } else {
                message.sender_avatar
            };
            if let Some(photo) = photo {
                self.load_avatar(&avatar, ChatImage::Avatar(photo));
            }
            author.append(&avatar);
            let name = label(sender, "message-name");
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
                        .and_then(|item| item.displayed_sender(own_name.as_deref()))
                        .or_else(|| {
                            preview.and_then(|item| item.displayed_sender(own_name.as_deref()))
                        })
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
                        if let Some(poll) = &item.poll {
                            bubble.append(&self.poll_card(chat, item.id, poll));
                        }
                        if let Some(sticker) = &item.sticker {
                            bubble.append(&self.sticker_card(
                                chat,
                                item.id,
                                sticker,
                                item.playback,
                            ));
                        }
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
                self.history_scroll.set(Some(HistoryScroll::Message {
                    id: message.id,
                    offset: self.scroll.vadjustment().page_size() / 3.0,
                }));
            }
        }
    }

    fn set_history_position(self: &Rc<Self>, position: HistoryScroll) {
        if self.history_scroll.replace(Some(position)).is_some() {
            return;
        }
        if let Some(viewport) = self.history.parent().and_downcast::<gtk::Viewport>() {
            viewport.set_scroll_to_focus(false);
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
                        ui.prepend_scroll.set(None);
                        if let Some(position) = ui.history_scroll.get() {
                            ui.restore_history_position(position);
                        }
                        ui.history_scroll.set(None);
                        if let Some(viewport) = ui.history.parent().and_downcast::<gtk::Viewport>()
                        {
                            viewport.set_scroll_to_focus(true);
                            // GTK can consume an allocation queued by the adjustment during the
                            // current layout. Apply the final offset even if the value is unchanged.
                            viewport.queue_allocate();
                        }
                        if adjustment.upper() - adjustment.page_size() <= adjustment.lower() {
                            ui.older_history_pending.set(true);
                        }
                        ui.continue_history();
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
            if self.selected_topic.borrow().is_some() {
                self.open_forum_message(chat, None, id);
            } else {
                self.reopen_conversation(Some(id));
            }
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
        if message.sticker.is_some() {
            return None;
        }
        let id = message.id;
        let playback = message.playback.filter(|_| !message.ephemeral);
        if let Some(media @ (PlayableMedia::Audio | PlayableMedia::Voice)) = playback {
            let (card, content, cover) = crate::audio::AudioCard::new(
                media,
                message.audio.as_ref().unwrap_or(&Default::default()),
                message.attachment.as_deref(),
                media == PlayableMedia::Audio && message.thumbnail.is_some(),
            );
            if media == PlayableMedia::Audio
                && let Some(thumbnail) = message.thumbnail
            {
                self.load_chat_image(
                    ChatImage::attachment(chat, id, thumbnail),
                    glib::clone!(
                        #[weak]
                        cover,
                        move |texture: &gtk::gdk::Texture| cover.set_paintable(Some(texture))
                    ),
                );
            }
            card.connect_play(glib::clone!(
                #[weak(rename_to = ui)]
                self,
                move || ui.open_playback(chat, id, media)
            ));
            card.connect_seek(glib::clone!(
                #[weak(rename_to = ui)]
                self,
                move |fraction| {
                    if let Some(audio) = ui.inline_audio.borrow().as_ref()
                        && audio.chat == chat
                        && audio.id == id
                    {
                        audio.player.seek(fraction);
                    }
                }
            ));
            content.set_widget_name(&format!("attachment-{id}"));
            if let Some(audio) = self.inline_audio.borrow().as_ref()
                && audio.chat == chat
                && audio.id == id
            {
                audio.player.attach(card.clone());
            }
            self.audio_cards.borrow_mut().insert(id, card);
            return Some(content.upcast());
        }
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
                    #[weak(rename_to = ui)]
                    self,
                    move |texture: &gtk::gdk::Texture| {
                        if preview.is_mapped() && ui.history_scroll.get().is_none() {
                            let position = if ui.history_at_bottom() {
                                HistoryScroll::Bottom
                            } else {
                                ui.history_position()
                            };
                            ui.set_history_position(position);
                        }
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
            let preview_area = gtk::Overlay::new();
            preview_area.set_child(Some(&preview));
            if let Some(PlayableMedia::Video { duration, size, .. }) = playback {
                let duration = duration.map(|seconds| {
                    crate::playback_controls::time_label(i64::from(seconds) * 1_000_000)
                });
                let size = (size > 0).then(|| glib::format_size(size).to_string());
                let info = duration
                    .into_iter()
                    .chain(size)
                    .collect::<Vec<_>>()
                    .join(" · ");
                if !info.is_empty() {
                    let details = label(&info, "video-meta");
                    details.set_halign(gtk::Align::Start);
                    details.set_valign(gtk::Align::End);
                    details.set_can_target(false);
                    preview_area.add_overlay(&details);
                }
            }
            let button = gtk::Button::builder().child(&preview_area).build();
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
            if matches!(image, ChatImage::Sticker(_)) {
                show(&crate::stickers::demo_texture());
                return;
            }
            if let Some((avatar, texture)) = self.demo_photo.borrow().as_ref()
                && image == ChatImage::Avatar(*avatar)
            {
                show(texture);
                return;
            }
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
                                ChatImage::Avatar(_) | ChatImage::Sticker(_) => Some(texture),
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
                let item = bubble_item_id(bubble.pick(x, y, gtk::PickFlags::DEFAULT));
                ui.open_message_menu(&bubble, chat, item.unwrap_or(id), x, y);
            }
        ));
        bubble.add_controller(click);

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
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
                    let item = bubble_item_id(focused.clone());
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
        let text = (!message.ephemeral)
            .then(|| message_copy_text(bubble, id))
            .flatten();
        let Some(menu) = self.message_menu_content(chat, &message, text) else {
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

    fn message_menu_content(
        self: &Rc<Self>,
        chat: i64,
        message: &Message,
        copy: Option<(String, bool)>,
    ) -> Option<gtk::Box> {
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let id = message.id;
        let target = self.conversation();
        if let Some((text, selected)) = copy {
            add_menu_button(
                &menu,
                if selected {
                    "Копировать выделение"
                } else {
                    "Копировать текст"
                },
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || {
                        if let Some(window) = ui.window.upgrade() {
                            window.clipboard().set_text(&text);
                        }
                    }
                ),
            );
        }
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
                            if let Some(target) =
                                target.filter(|target| ui.conversation() == Some(*target))
                            {
                                ui.replies.borrow_mut().insert(target, reply.clone());
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
        if let Some(media) = message
            .playback
            .filter(|_| !message.ephemeral && message.sticker.is_none())
        {
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

    fn set_chat_writability(&self, writable: bool) {
        let closed = self
            .selected_topic
            .borrow()
            .as_ref()
            .is_some_and(|topic| topic.closed);
        self.input
            .set_sensitive(writable && !closed && self.conversation().is_some());
        self.input_placeholder.set_text(if closed {
            "Тема закрыта"
        } else if writable {
            "Сообщение"
        } else {
            "Канал только для чтения"
        });
        self.update_send_button();
    }

    fn update_send_button(&self) {
        let text = self.composer_text();
        self.input_placeholder.set_visible(text.is_empty());
        let uploads = self.uploads.borrow();
        let pending = self.conversation().and_then(|target| uploads.get(&target));
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
        let reply = self.conversation().and_then(|target| replies.get(&target));
        self.reply_bar.set_visible(reply.is_some());
        if let Some(reply) = reply {
            self.reply_sender.set_text(
                reply
                    .preview
                    .displayed_sender(self.own_name.borrow().as_deref())
                    .unwrap_or("Исходное сообщение"),
            );
            self.reply_label.set_text(&reply.preview.text);
        }
        self.attach
            .set_sensitive(self.input.is_sensitive() && !self.sending.get());
        let wait = self
            .selection
            .get()
            .and_then(|chat| self.slow_mode_remaining(chat));
        if let Some(wait) = wait {
            self.slow_mode_label.set_text(&slow_mode_status(wait));
        }
        self.slow_mode_label.set_visible(wait.is_some());
        self.send.set_sensitive(
            self.input.is_sensitive()
                && (!text.trim().is_empty() || pending.is_some())
                && text.trim().chars().count() <= 4096
                && !self.sending.get()
                && wait.is_none(),
        );
    }

    fn slow_mode_remaining(&self, chat: i64) -> Option<Duration> {
        self.slow_mode_waits
            .borrow()
            .get(&chat)
            .and_then(|until| until.checked_duration_since(Instant::now()))
            .filter(|remaining| !remaining.is_zero())
    }

    fn refresh_slow_mode_if_enabled(self: &Rc<Self>, chat: i64) {
        let tracked = self
            .chats
            .borrow()
            .iter()
            .any(|item| item.id == chat && item.slow_mode_enabled)
            || self
                .external_chat
                .borrow()
                .as_ref()
                .is_some_and(|item| item.id == chat && item.slow_mode_enabled);
        if tracked {
            self.refresh_slow_mode(chat);
        }
    }

    fn refresh_slow_mode(self: &Rc<Self>, chat: i64) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let request = Rc::new(());
        self.slow_mode_requests
            .borrow_mut()
            .insert(chat, request.clone());
        self.job(
            async move { client.slow_mode_remaining(chat).await },
            move |ui, result| {
                {
                    let mut requests = ui.slow_mode_requests.borrow_mut();
                    if !requests
                        .get(&chat)
                        .is_some_and(|current| Rc::ptr_eq(current, &request))
                    {
                        return;
                    }
                    requests.remove(&chat);
                }
                if let Ok(wait) = result {
                    ui.set_slow_mode_wait(chat, wait);
                }
            },
        );
    }

    fn set_slow_mode_wait(self: &Rc<Self>, chat: i64, wait: Duration) {
        if wait.is_zero() {
            self.slow_mode_waits.borrow_mut().remove(&chat);
        } else {
            self.slow_mode_waits
                .borrow_mut()
                .insert(chat, Instant::now() + wait);
            if !self.slow_mode_timer_running.replace(true) {
                let weak = Rc::downgrade(self);
                glib::timeout_add_local(Duration::from_millis(250), move || {
                    let Some(ui) = weak.upgrade() else {
                        return glib::ControlFlow::Break;
                    };
                    let now = Instant::now();
                    ui.slow_mode_waits
                        .borrow_mut()
                        .retain(|_, until| *until > now);
                    let selected = ui
                        .selection
                        .get()
                        .is_some_and(|id| ui.slow_mode_waits.borrow().contains_key(&id));
                    if selected || ui.slow_mode_label.is_visible() {
                        ui.update_send_button();
                    }
                    if ui.slow_mode_waits.borrow().is_empty() {
                        ui.slow_mode_timer_running.set(false);
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                });
            }
        }
        if self.selection.get() == Some(chat) {
            self.update_send_button();
        }
    }

    fn note_slow_mode_error(self: &Rc<Self>, chat: i64, error: &anyhow::Error) -> Option<String> {
        let wait = crate::telegram::slow_mode_wait(error)?;
        self.slow_mode_requests.borrow_mut().remove(&chat);
        self.set_slow_mode_wait(chat, wait);
        Some(slow_mode_status(wait))
    }

    fn start_demo_slow_mode(self: &Rc<Self>, chat: i64) {
        if chat == demo::SLOW_MODE_GROUP {
            self.set_slow_mode_wait(chat, demo::SLOW_MODE_DELAY);
        }
    }

    fn choose_upload(self: &Rc<Self>, kind: UploadKind) {
        if !self.attach.is_sensitive() {
            return;
        }
        let (Some(window), Some(target)) = (self.window.upgrade(), self.conversation()) else {
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
                ui.uploads.borrow_mut().insert(target, selected);
            }
            ui.update_send_button();
        });
    }

    fn sticker_card(
        self: &Rc<Self>,
        chat: i64,
        id: i32,
        sticker: &crate::model::Sticker,
        playback: Option<PlayableMedia>,
    ) -> gtk::Widget {
        let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let image = gtk::Stack::new();
        image.set_size_request(160, 160);
        image.set_tooltip_text(Some(&sticker.label()));
        let fallback = label(
            if sticker.emoji.is_empty() {
                "✦"
            } else {
                &sticker.emoji
            },
            "title-1",
        );
        fallback.set_halign(gtk::Align::Center);
        image.add_named(&fallback, Some("fallback"));
        let picture = gtk::Picture::builder()
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Contain)
            .build();
        picture.update_property(&[gtk::accessible::Property::Label(&sticker.label())]);
        image.add_named(&picture, Some("image"));
        if matches!(playback, Some(PlayableMedia::Video { .. })) {
            let demo = self.is_demo.get();
            let client = self.client.borrow().clone();
            body.append(&crate::inline_sticker::build(
                &image,
                self.runtime.clone(),
                Box::new(move || {
                    let client = client.clone();
                    Box::pin(async move {
                        let (progress, _) = tokio::sync::watch::channel(Default::default());
                        if demo {
                            demo::sticker_video_preview(progress, true).await
                        } else {
                            client
                                .context("Нет подключения к Telegram")?
                                .sticker_video_preview(chat, id, progress, true)
                                .await
                        }
                    })
                }),
            ));
        } else {
            body.append(&image);
        }
        self.load_chat_image(
            ChatImage::Sticker(sticker.id),
            glib::clone!(
                #[weak]
                picture,
                #[weak]
                image,
                move |texture| {
                    picture.set_paintable(Some(texture));
                    image.set_visible_child_name("image");
                }
            ),
        );
        body.upcast()
    }

    fn open_stickers(self: &Rc<Self>) {
        if !self.attach.is_sensitive() || self.chat_dialog_open() {
            return;
        }
        let (Some(window), Some(target)) = (self.window.upgrade(), self.conversation()) else {
            return;
        };
        let reply = self.replies.borrow().get(&target).cloned();
        let weak = Rc::downgrade(self);
        let dialog = crate::stickers::open(
            &window,
            crate::stickers::ContextData {
                runtime: self.runtime.clone(),
                client: self.client.borrow().clone(),
                demo_favorites: self.demo_favorite_stickers.clone(),
                demo_packs: self.demo_sticker_packs.clone(),
                send: Box::new(move |sticker, random_id| {
                    let weak = weak.clone();
                    let reply = reply.clone();
                    Box::pin(async move {
                        let ui = weak.upgrade().context("Аккаунт закрыт")?;
                        anyhow::ensure!(
                            ui.conversation() == Some(target),
                            "Чат изменился. Откройте стикеры в нужном чате заново"
                        );
                        anyhow::ensure!(
                            ui.input.is_sensitive(),
                            "Сейчас нельзя отправить сообщение в этот чат"
                        );
                        anyhow::ensure!(!ui.sending.get(), "Дождитесь текущей отправки");
                        if let Some(wait) = ui.slow_mode_remaining(target.chat) {
                            anyhow::bail!("{}", slow_mode_status(wait));
                        }
                        let message = if ui.is_demo.get() {
                            demo::sticker_message(ui.next_demo_message(), sticker)
                        } else {
                            let client = ui
                                .client
                                .borrow()
                                .clone()
                                .context("Нет подключения к Telegram")?;
                            ui.sending.set(true);
                            ui.update_send_button();
                            let reply_to = reply.as_ref().map(|reply| reply.id);
                            let task = ui.runtime.spawn(async move {
                                tokio::time::timeout(
                                    Duration::from_secs(35),
                                    client.send_sticker(target, sticker.id, reply_to, random_id),
                                )
                                .await
                                .context("Telegram не подтвердил отправку вовремя")?
                            });
                            drop(ui);
                            let result = task
                                .await
                                .map_err(anyhow::Error::from)
                                .and_then(|result| result);
                            let ui = weak.upgrade().context("Аккаунт закрыт")?;
                            ui.sending.set(false);
                            ui.update_send_button();
                            result.map_err(|error| {
                                ui.note_slow_mode_error(target.chat, &error)
                                    .map(anyhow::Error::msg)
                                    .unwrap_or(error)
                            })?
                        };
                        if let Some(ui) = weak.upgrade() {
                            ui.complete_sticker_send(target, message, reply);
                        }
                        Ok(())
                    })
                }),
            },
        );
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| {
                ui.stickers_dialog.set(None);
                ui.focus_composer();
            }
        ));
        self.stickers_dialog.set(Some(&dialog));
    }

    fn complete_sticker_send(
        self: &Rc<Self>,
        target: Conversation,
        mut message: Message,
        reply: Option<Reply>,
    ) {
        if self.is_demo.get() {
            message.topic = target.topic;
            message.reply_to = reply.as_ref().map(|reply| reply.id);
        }
        if message.reply_preview.is_none()
            && message.reply_to == reply.as_ref().map(|reply| reply.id)
        {
            message.reply_preview = reply.as_ref().map(|reply| reply.preview.clone());
        }
        if self.replies.borrow().get(&target) == reply.as_ref() {
            self.replies.borrow_mut().remove(&target);
        }
        self.history_cache.borrow_mut().remove_conversation(target);
        if self.is_demo.get() {
            self.add_demo_messages(target, vec![message]);
            self.start_demo_slow_mode(target.chat);
        } else {
            self.refresh_slow_mode_if_enabled(target.chat);
            if self.conversation() == Some(target) {
                if self.search_anchor.get().is_some() || !self.loaded.get() {
                    self.reopen_conversation(None);
                } else {
                    self.messages.borrow_mut().insert(message.id, message);
                    self.render_history(true);
                }
            }
            self.refresh_chats();
        }
        self.update_send_button();
    }

    fn create_poll(self: &Rc<Self>) {
        if !self.attach.is_sensitive() || self.chat_dialog_open() {
            return;
        }
        let (Some(window), Some(target)) = (self.window.upgrade(), self.conversation()) else {
            return;
        };
        let chat = target.chat;
        let weak = Rc::downgrade(self);
        let dialog = crate::poll_dialog::create(&window, move |draft, random_id| {
            let weak = weak.clone();
            Box::pin(async move {
                let ui = weak.upgrade().context("Аккаунт закрыт")?;
                if let Some(wait) = ui.slow_mode_remaining(chat) {
                    anyhow::bail!("{}", slow_mode_status(wait));
                }
                if ui.is_demo.get() {
                    ui.add_demo_messages(
                        target,
                        vec![Message {
                            topic: target.topic,
                            ..demo::poll_message(ui.next_demo_message(), draft)
                        }],
                    );
                    ui.start_demo_slow_mode(chat);
                    return Ok(());
                }
                let client = ui
                    .client
                    .borrow()
                    .clone()
                    .context("Нет подключения к Telegram")?;
                let task = ui.runtime.spawn(async move {
                    tokio::time::timeout(
                        Duration::from_secs(35),
                        client.send_poll(chat, draft, random_id, target.topic),
                    )
                    .await
                    .context("Telegram не подтвердил отправку вовремя")?
                });
                drop(ui);
                let result = task.await?;
                let message = result.map_err(|error| {
                    if let Some(ui) = weak.upgrade()
                        && let Some(wait) = ui.note_slow_mode_error(chat, &error)
                    {
                        return anyhow::anyhow!(wait);
                    }
                    error
                })?;
                if let Some(ui) = weak.upgrade() {
                    ui.refresh_slow_mode_if_enabled(chat);
                    ui.history_cache.borrow_mut().remove_conversation(target);
                    if ui.conversation() == Some(target) && ui.search_anchor.get().is_none() {
                        if let Some(message) = message {
                            ui.messages.borrow_mut().insert(message.id, message);
                            ui.render_history(true);
                        } else {
                            ui.load_history(None, false);
                        }
                    }
                    ui.refresh_chats();
                }
                Ok(())
            })
        });
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| {
                ui.poll_dialog.set(None);
                ui.focus_composer();
            }
        ));
        self.poll_dialog.set(Some(&dialog));
    }

    fn poll_card(self: &Rc<Self>, chat: i64, id: i32, poll: &Poll) -> gtk::Box {
        let card = crate::poll_dialog::card(poll);
        if !poll.can_vote() {
            return card;
        }
        let button = gtk::Button::with_label(if poll.has_vote() {
            "Изменить голос"
        } else {
            "Голосовать"
        });
        button.set_halign(gtk::Align::Start);
        button.set_sensitive(!self.voting.borrow().contains(&(chat, id)));
        button.connect_clicked(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| {
                let Some(window) = ui.window.upgrade() else {
                    return;
                };
                let current = ui.messages.borrow();
                let Some(poll) = current.get(&id).and_then(|message| message.poll.as_ref()) else {
                    return;
                };
                if ui.selection.get() != Some(chat) || ui.chat_dialog_open() || !poll.can_vote() {
                    return;
                }
                let expected = poll.id;
                crate::poll_dialog::vote(
                    &window,
                    poll,
                    glib::clone!(
                        #[weak]
                        ui,
                        move |options| ui.vote(chat, id, expected, options)
                    ),
                );
            }
        ));
        card.append(&button);
        card
    }

    fn vote(self: &Rc<Self>, chat: i64, id: i32, expected: i64, options: Vec<Vec<u8>>) {
        if self.is_demo.get() {
            let Some(poll) = self
                .demo_messages
                .borrow()
                .get(&chat)
                .and_then(|messages| messages.iter().find(|message| message.id == id))
                .and_then(|message| message.poll.clone())
                .filter(|poll| poll.id == expected && poll.accepts(&options))
            else {
                self.toast("Этот голос больше недоступен");
                return;
            };
            let total = poll.total.unwrap_or(0) - i32::from(poll.has_vote())
                + i32::from(!options.is_empty());
            let results = poll
                .choices
                .iter()
                .enumerate()
                .map(|(index, choice)| {
                    let chosen = options.contains(&choice.option);
                    let votes =
                        choice.votes.unwrap_or(0) - i32::from(choice.chosen) + i32::from(chosen);
                    PollVoteCount {
                        option: choice.option.clone(),
                        votes: Some(votes.max(0)),
                        chosen,
                        correct: poll.quiz && index == 0,
                    }
                })
                .collect();
            self.apply_poll_update(PollUpdate {
                id: expected,
                target: None,
                definition: None,
                results: Some(results),
                total: Some(total.max(0)),
                solution: poll.quiz.then_some("Первый вариант правильный.".into()),
                partial: false,
            });
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        if !self.voting.borrow_mut().insert((chat, id)) {
            return;
        }
        self.render_poll_change();
        self.job(
            async move {
                tokio::time::timeout(
                    Duration::from_secs(30),
                    client.vote(chat, id, expected, options),
                )
                .await
                .context("Telegram не подтвердил голос вовремя")?
            },
            move |ui, result| {
                ui.voting.borrow_mut().remove(&(chat, id));
                match result {
                    Ok(Some(update)) => ui.apply_poll_update(update),
                    Ok(None) => {
                        ui.toast(
                            "Telegram принял голос. Результаты обновятся после синхронизации.",
                        );
                        if ui.selection.get() == Some(chat) {
                            ui.load_history(None, false);
                        }
                    }
                    Err(error) => ui.toast(&format!("Не удалось отправить голос. {error}")),
                }
                if ui.selection.get() == Some(chat) {
                    ui.render_poll_change();
                }
            },
        );
    }

    fn apply_poll_update(self: &Rc<Self>, update: PollUpdate) {
        self.history_cache
            .borrow_mut()
            .update_polls(|chat, id, poll| {
                update.apply(chat, id, poll);
            });
        if self.is_demo.get() {
            for (&chat, messages) in &mut *self.demo_messages.borrow_mut() {
                for message in messages {
                    if let Some(poll) = &mut message.poll {
                        update.apply(chat, message.id, poll);
                    }
                }
            }
        }
        let Some(chat) = self.selection.get() else {
            return;
        };
        let mut changed = false;
        for message in self.messages.borrow_mut().values_mut() {
            if let Some(poll) = &mut message.poll {
                changed |= update.apply(chat, message.id, poll);
            }
        }
        let revision = self.poll_revision.get() + 1;
        self.poll_revision.set(revision);
        let mut recent = self.recent_polls.borrow_mut();
        if recent.len() >= 64 {
            recent.pop_front();
        }
        recent.push_back((revision, update));
        drop(recent);
        if changed {
            self.render_poll_change();
        }
    }

    fn render_poll_change(self: &Rc<Self>) {
        self.render_history(false);
    }

    fn send_message(self: &Rc<Self>) {
        if !self.send.is_sensitive() {
            return;
        }
        let Some(target) = self.conversation() else {
            return;
        };
        let chat = target.chat;
        let original = editor_text(&self.input.buffer());
        let text = original.trimmed();
        let uploads = self
            .uploads
            .borrow()
            .get(&target)
            .cloned()
            .unwrap_or_default();
        let reply = self.replies.borrow().get(&target).cloned();
        let reply_to = reply.as_ref().map(|reply| reply.id);
        if self.is_demo.get() {
            let id = self.next_demo_message();
            let message = Message {
                id,
                topic: target.topic,
                text: text.text,
                styles: text.spans,
                links: Vec::new(),
                sender: demo::OWN_NAME.into(),
                sender_avatar: Some(demo::OWN_AVATAR),
                self_authored: true,
                outgoing: true,
                timestamp: demo::now(),
                attachment: None,
                photo_preview: false,
                playback: None,
                audio: None,
                thumbnail: None,
                poll: None,
                sticker: None,
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
            self.uploads.borrow_mut().remove(&target);
            self.replies.borrow_mut().remove(&target);
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
            self.add_demo_messages(target, messages);
            self.start_demo_slow_mode(chat);
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
            async move {
                client
                    .send(chat, text, request_uploads, reply_to, target.topic)
                    .await
            },
            move |ui, result| {
                ui.sending.set(false);
                match result {
                    Ok(mut messages) => {
                        ui.refresh_slow_mode_if_enabled(chat);
                        for message in &mut messages {
                            if message.reply_to == reply_to && message.reply_preview.is_none() {
                                message.reply_preview =
                                    reply.as_ref().map(|reply| reply.preview.clone());
                            }
                        }
                        if ui.uploads.borrow().get(&target) == Some(&uploads) {
                            ui.uploads.borrow_mut().remove(&target);
                        }
                        if ui.replies.borrow().get(&target) == reply.as_ref() {
                            ui.replies.borrow_mut().remove(&target);
                        }
                        if ui.conversation() == Some(target) {
                            if editor_text(&ui.input.buffer()) == original {
                                ui.input.buffer().set_text("");
                            }
                            if ui.search_anchor.get().is_some() || !ui.loaded.get() {
                                ui.history_cache.borrow_mut().remove_conversation(target);
                                ui.reopen_conversation(None);
                            } else {
                                ui.messages.borrow_mut().extend(
                                    messages.into_iter().map(|message| (message.id, message)),
                                );
                                ui.render_history(true);
                            }
                        } else {
                            ui.history_cache.borrow_mut().remove_conversation(target);
                            if ui.drafts.borrow().get(&target) == Some(&original) {
                                ui.drafts.borrow_mut().remove(&target);
                            }
                        }
                        ui.refresh_chats();
                        if ui.conversation() == Some(target)
                            && ui.search_anchor.get().is_none()
                            && uploads.len() > 1
                        {
                            ui.load_history(None, false);
                        }
                    }
                    Err(error) => {
                        if let Some(wait) = ui.note_slow_mode_error(chat, &error) {
                            if ui.selection.get() != Some(chat) {
                                ui.toast(&wait);
                            }
                        } else {
                            ui.toast(&format!("Не удалось отправить сообщение. {error}"));
                        }
                    }
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

    fn add_demo_messages(self: &Rc<Self>, target: Conversation, messages: Vec<Message>) {
        let chat = target.chat;
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
        self.history_cache.borrow_mut().remove_conversation(target);
        if let Some(chat) = self
            .chats
            .borrow_mut()
            .iter_mut()
            .find(|item| item.id == chat)
        {
            chat.preview = messages
                .iter()
                .find(|message| !message.text.is_empty() || message.poll.is_some())
                .map(|message| message.preview_text().into_owned())
                .unwrap_or_else(|| "Вложение".into());
            chat.last_activity = last.timestamp;
            chat.last_message_id = last.id;
            chat.preview_sender = chat.group.then(|| {
                last.displayed_sender(self.own_name.borrow().as_deref())
                    .to_owned()
            });
            chat.preview_image = last
                .thumbnail
                .map(|media| ChatImage::attachment(chat.id, last.id, media));
        }
        self.demo_messages
            .borrow_mut()
            .entry(chat)
            .or_insert_with(|| demo::history(chat))
            .extend(messages.iter().cloned());
        if self.conversation() == Some(target) {
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
        for chat in self.chats.borrow().iter().filter(|chat| chat.can_write) {
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
            if ui.chat_view(chat).is_some_and(|chat| chat.forum) {
                ui.choose_forum_topic(chat, false, move |ui, topic| {
                    if topic.closed {
                        ui.toast("Тема закрыта");
                    } else {
                        ui.forward_to(
                            source,
                            message.clone(),
                            Conversation {
                                chat,
                                topic: Some(topic.id),
                            },
                        );
                    }
                });
            } else {
                ui.forward_to(source, message, chat.into());
            }
        });
    }

    fn forward_to(self: &Rc<Self>, source: i64, message: Message, target: Conversation) {
        let chat = target.chat;
        if self.sending.get() {
            self.toast("Дождитесь текущей отправки");
            return;
        }
        if let Some(wait) = self.slow_mode_remaining(chat) {
            self.toast(&slow_mode_status(wait));
            return;
        }
        if self.is_demo.get() {
            self.add_demo_messages(
                target,
                vec![Message {
                    id: self.next_demo_message(),
                    topic: target.topic,
                    timestamp: demo::now(),
                    sender: demo::OWN_NAME.into(),
                    sender_avatar: Some(demo::OWN_AVATAR),
                    self_authored: true,
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
            self.start_demo_slow_mode(chat);
            self.toast("Сообщение переслано");
            return;
        }
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        self.sending.set(true);
        self.update_send_button();
        self.job(
            async move { client.forward_message(source, message.id, target).await },
            move |ui, result| {
                ui.sending.set(false);
                ui.update_send_button();
                match result {
                    Ok(message) => {
                        ui.refresh_slow_mode_if_enabled(chat);
                        ui.history_cache.borrow_mut().remove_conversation(target);
                        if ui.conversation() == Some(target) && ui.search_anchor.get().is_none() {
                            ui.messages.borrow_mut().insert(message.id, message);
                            ui.render_history(true);
                        }
                        ui.refresh_chats();
                        ui.toast("Сообщение переслано");
                    }
                    Err(error) => {
                        let error = ui
                            .note_slow_mode_error(chat, &error)
                            .unwrap_or_else(|| error.to_string());
                        ui.toast(&format!("Не удалось переслать сообщение. {error}"));
                    }
                }
            },
        );
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
                message.links.clear();
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
            message.links.clear();
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
        self.history_cache.borrow_mut().remove(chat);
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
                chat.preview_sender = last.filter(|_| chat.group).map(|message| {
                    message
                        .displayed_sender(self.own_name.borrow().as_deref())
                        .to_owned()
                });
                chat.preview_image = last.and_then(|message| {
                    message
                        .thumbnail
                        .map(|media| ChatImage::attachment(chat.id, message.id, media))
                });
            }
            self.render_chats();
        }
        self.replies
            .borrow_mut()
            .retain(|target, reply| target.chat != chat || reply.id != id);
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
        self.history_cache.borrow_mut().remove(chat);
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
        self.history_cache.borrow_mut().remove(chat);
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
        self.save_file_from(window.upcast(), chat, id, name);
    }

    fn save_file_from(self: &Rc<Self>, window: gtk::Window, chat: i64, id: i32, name: String) {
        let toasts = window
            .downcast_ref::<adw::Window>()
            .and_then(|window| window.content().and_downcast::<adw::ToastOverlay>())
            .unwrap_or_else(|| self.toasts.clone());
        if self.downloading.replace(true) {
            toasts.add_toast(adw::Toast::new("Дождитесь текущей загрузки"));
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
                toasts.add_toast(adw::Toast::new("Выберите локальный каталог"));
                return;
            };
            let client = ui.client.borrow().clone();
            let fixture = ui
                .is_demo
                .get()
                .then(|| match (chat, id) {
                    (1, 83) => Some(demo::FILE_CONTENT.as_bytes().to_vec()),
                    (5, 2) => Some(include_bytes!("../data/demo-video.webm").to_vec()),
                    (1, 84) | (3, 1) => Some(
                        ui.demo_image
                            .get_or_init(demo_texture)
                            .save_to_png_bytes()
                            .to_vec(),
                    ),
                    _ => ui
                        .demo_messages
                        .borrow()
                        .get(&chat)
                        .and_then(|messages| messages.iter().find(|message| message.id == id))
                        .and_then(|message| message.sticker.as_ref())
                        .and_then(|sticker| match sticker.kind {
                            crate::model::StickerKind::Static => {
                                Some(include_bytes!("../data/demo-sticker.webp").to_vec())
                            }
                            crate::model::StickerKind::Video => {
                                Some(include_bytes!("../data/demo-sticker-video.webm").to_vec())
                            }
                            _ => None,
                        }),
                })
                .flatten();
            let progress = adw::Toast::new("Загрузка вложения…");
            toasts.add_toast(progress.clone());
            ui.job(
                async move {
                    if let Some(client) = client {
                        client.save(chat, id, path).await
                    } else {
                        let bytes = fixture.context("Демонстрационное вложение недоступно")?;
                        let mut output = PendingSave::new(path)?;
                        output.write(&bytes).await?;
                        output.finish(Some(bytes.len())).await
                    }
                },
                move |ui, result| {
                    progress.dismiss();
                    ui.downloading.set(false);
                    let message = match result {
                        Ok(()) => "Вложение сохранено".into(),
                        Err(error) => format!("Не удалось сохранить вложение. {error}"),
                    };
                    toasts.add_toast(adw::Toast::new(&message));
                },
            );
        });
    }

    fn viewer_save(
        self: &Rc<Self>,
        chat: i64,
        id: i32,
        fallback: &str,
    ) -> crate::media_viewer::SaveAttachment {
        let name = self
            .messages
            .borrow()
            .get(&id)
            .and_then(|message| message.attachment.clone())
            .unwrap_or_else(|| fallback.to_owned());
        Box::new(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |viewer| ui.save_file_from(viewer.clone().upcast(), chat, id, name.clone())
        ))
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
        if matches!(media, PlayableMedia::Audio | PlayableMedia::Voice) {
            if self.selection.get() != Some(chat) {
                return;
            }
            let active = self
                .inline_audio
                .borrow()
                .as_ref()
                .filter(|audio| audio.chat == chat && audio.id == id)
                .map(|audio| audio.player.clone());
            if let Some(player) = active {
                if player.is_loading() {
                    self.inline_audio.take();
                } else {
                    player.toggle();
                }
                return;
            }
        }
        self.inline_audio.take();
        self.begin_media_preview();
        let demo = self.is_demo.get();
        let client = self.client.borrow().clone();
        let context = crate::media_viewer::PlaybackContext {
            runtime: self.runtime.clone(),
            media,
            muted: demo,
            load: Box::new(move |progress, whole| {
                let client = client.clone();
                Box::pin(async move {
                    if demo {
                        demo::playback_preview(media, progress, whole).await
                    } else {
                        client
                            .context("Нет подключения к Telegram")?
                            .playback_preview(chat, id, progress, whole)
                            .await
                    }
                })
            }),
        };
        if matches!(media, PlayableMedia::Audio | PlayableMedia::Voice) {
            let Some(card) = self.audio_cards.borrow().get(&id).cloned() else {
                return;
            };
            let player = crate::audio::InlinePlayer::open(context, card, self.toasts.clone());
            self.inline_audio.replace(Some(InlineAudio {
                chat,
                id,
                media,
                player,
            }));
        } else {
            let viewer = crate::media_viewer::open(
                &window,
                context,
                Some(self.viewer_save(chat, id, "Видео")),
            );
            self.media_viewer.set(Some(&viewer));
        }
    }

    fn open_photo_preview(self: &Rc<Self>, chat: i64, id: i32) {
        let request = self.begin_media_preview();
        let save = self.viewer_save(chat, id, "Фотография.png");
        if self.is_demo.get() {
            self.present_photo(self.demo_image.get_or_init(demo_texture).upcast_ref(), save);
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
                        Ok(texture) => ui.present_photo(&texture, save),
                        Err(error) => ui.toast(&format!("Не удалось открыть фотографию. {error}")),
                    }
                },
            );
        }
    }

    fn present_photo(
        &self,
        texture: &gtk::gdk::Texture,
        save: crate::media_viewer::SaveAttachment,
    ) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let screen = crate::media_viewer::monitor_size(&window);
        let viewer = crate::media_viewer::viewer_window(&window, "Фотография");
        self.media_viewer.set(Some(&viewer));
        let toolbar = adw::ToolbarView::new();
        let toasts = adw::ToastOverlay::new();
        let header = adw::HeaderBar::builder()
            .show_title(false)
            .decoration_layout(":close")
            .build();
        let save_button =
            crate::media_viewer::action_button("document-save-symbolic", "Сохранить вложение");
        save_button.connect_clicked(glib::clone!(
            #[weak]
            viewer,
            move |_| save(&viewer)
        ));
        header.pack_start(&save_button);
        let copy_button =
            crate::media_viewer::action_button("edit-copy-symbolic", "Скопировать изображение");
        copy_button.connect_clicked(glib::clone!(
            #[strong]
            texture,
            #[weak]
            toasts,
            move |button| {
                button.clipboard().set_texture(&texture);
                toasts.add_toast(adw::Toast::new("Изображение скопировано"));
            }
        ));
        header.pack_start(&copy_button);
        toolbar.add_top_bar(&header);
        toasts.set_child(Some(&toolbar));
        viewer.set_content(Some(&toasts));
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
            screen,
            (header_width, header_height),
            crate::media_viewer::frame_spacing(&viewer),
        );
        let content = gtk::Overlay::new();
        content.add_overlay(&picture);
        toolbar.set_content(Some(&content));
        viewer.set_default_size(width.max(header_width), height + header_height);

        crate::media_viewer::constrain_to_workspace(&viewer, screen);
        let close = gtk::EventControllerKey::new();
        close.set_propagation_phase(gtk::PropagationPhase::Capture);
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
        label.set_widget_name(&format!("message-text-{}", message.id));
        label.set_selectable(!message.ephemeral);
        let formatted = message.formatted_text();
        let hidden = formatted
            .spans
            .iter()
            .any(|span| span.style == TextStyle::Spoiler)
            && self.revealed_spoilers.borrow().get(&message.id) != Some(&formatted);
        let links = message.links.clone();
        render_message(&label, &formatted, &links, hidden);
        let id = message.id;
        let generation = self.generation.get();
        label.connect_activate_link(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            #[upgrade_or]
            glib::Propagation::Stop,
            move |label, uri| {
                if uri != SPOILER_LINK {
                    ui.open_message_link(uri);
                    return glib::Propagation::Stop;
                }
                let formatted = formatted.clone();
                let links = links.clone();
                glib::idle_add_local_once(glib::clone!(
                    #[weak]
                    ui,
                    #[weak]
                    label,
                    move || {
                        if ui.generation.get() == generation && label.is_mapped() {
                            render_message(&label, &formatted, &links, false);
                            ui.revealed_spoilers.borrow_mut().insert(id, formatted);
                        }
                    }
                ));
                glib::Propagation::Stop
            }
        ));
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_max_width_chars(64);
        bubble.append(&label);
    }

    fn open_message_link(self: &Rc<Self>, uri: &str) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(uri) = crate::links::normalize(uri) else {
            return;
        };
        let request = gtk::UriLauncher::new(&uri).launch_future(Some(&window));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            if let Err(error) = request.await
                && !error.matches(gtk::DialogError::Dismissed)
                && let Some(ui) = weak.upgrade()
            {
                ui.toast(&format!("Не удалось открыть ссылку. {error}"));
            }
        });
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }
}

fn bubble_item_id(mut widget: Option<gtk::Widget>) -> Option<i32> {
    while let Some(item) = widget {
        let name = item.widget_name();
        if let Some(id) = name
            .strip_prefix("attachment-")
            .or_else(|| name.strip_prefix("message-text-"))
        {
            return id.parse().ok();
        }
        widget = item.parent();
    }
    None
}

fn message_copy_text(bubble: &gtk::Box, id: i32) -> Option<(String, bool)> {
    let name = format!("message-text-{id}");
    let label = named_child(bubble, &name).and_downcast::<gtk::Label>()?;
    let text = label.text();
    if let Some((from, to)) = label.selection_bounds() {
        return Some((
            text.chars()
                .skip(from as usize)
                .take((to - from) as usize)
                .collect(),
            true,
        ));
    }
    (!text.is_empty()).then(|| (text.into(), false))
}

fn named_child(parent: &impl IsA<gtk::Widget>, name: &str) -> Option<gtk::Widget> {
    std::iter::successors(parent.first_child(), |child| child.next_sibling())
        .find(|child| child.widget_name() == name)
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

fn slow_mode_status(wait: Duration) -> String {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    let minutes = seconds / 60;
    let remainder = seconds % 60;
    let duration = if minutes < 60 {
        format!("{minutes:02}:{remainder:02}")
    } else {
        format!("{}:{:02}:{remainder:02}", minutes / 60, minutes % 60)
    };
    format!("Следующее сообщение через {duration}")
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
    use super::*;

    #[test]
    #[ignore = "requires a graphical display"]
    fn message_menu_copies_selected_characters_or_the_visible_label() {
        gtk::test_synced(|| {
            let bubble = gtk::Box::new(gtk::Orientation::Vertical, 0);
            bubble.append(&gtk::Button::with_label("Вложение"));
            let caption = gtk::Label::new(Some("😀 текст ▰▰▰"));
            caption.set_widget_name("message-text-12");
            caption.set_selectable(true);
            bubble.append(&caption);
            assert_eq!(
                message_copy_text(&bubble, 12),
                Some(("😀 текст ▰▰▰".into(), false))
            );
            caption.select_region(2, 7);
            assert_eq!(message_copy_text(&bubble, 12), Some(("текст".into(), true)));
            assert_eq!(message_copy_text(&bubble, 2), None);
        });
    }

    #[test]
    #[ignore = "requires a graphical display"]
    fn completed_message_actions_survive_switching_away_and_back() {
        gtk::test_synced(|| {
            let app = crate::application(true);
            app.set_application_id(Some("io.github.skae798.tgrust.CacheTest"));
            app.set_flags(gtk::gio::ApplicationFlags::NON_UNIQUE);
            app.register(None::<&gtk::gio::Cancellable>).unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let (window, ui) = build_window(
                &app,
                Accounts::new(&app, true),
                Account::Demo(0),
                None,
                runtime,
            );
            window.set_default_size(360, 720);
            let context = glib::MainContext::default();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !(340..=360).contains(&window.width()) && Instant::now() < deadline {
                context.iteration(false);
                std::thread::sleep(Duration::from_millis(10));
            }
            // X11 client-side shadows can subtract 10 px from the requested width.
            assert!((340..=360).contains(&window.width()));
            ui.show_chat(1, None);
            while !ui.input.is_mapped() && Instant::now() < deadline {
                context.iteration(false);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(ui.input.grab_focus());
            assert_eq!(
                gtk::prelude::RootExt::focus(&window),
                Some(ui.input.clone().upcast())
            );
            let id = *ui.messages.borrow().last_key_value().unwrap().0;
            ui.input.buffer().set_text("Черновик первого чата");
            ui.show_chat(2, None);
            ui.input.buffer().set_text("Черновик второго чата");
            ui.set_message_pinned(1, id, true);
            ui.show_chat(1, None);
            assert!(
                ui.messages.borrow()[&id].pinned,
                "Completed pin was lost to a cached page"
            );
            ui.show_chat(2, None);
            ui.toggle_reaction(1, id, "👍".into());
            ui.show_chat(1, None);
            assert!(
                ui.messages.borrow()[&id]
                    .reactions
                    .iter()
                    .any(|reaction| reaction.emoji == "👍" && reaction.chosen),
                "Completed reaction was lost to a cached page"
            );
            ui.show_chat(2, None);
            ui.apply_deletion(1, id);
            ui.show_chat(1, None);
            assert!(
                !ui.messages.borrow().contains_key(&id),
                "Deleted message reappeared from a cached page"
            );
            assert_eq!(
                editor_text(&ui.input.buffer()).text,
                "Черновик первого чата"
            );
            ui.show_chat(2, None);
            assert_eq!(
                editor_text(&ui.input.buffer()).text,
                "Черновик второго чата"
            );
            window.close();
        });
    }

    #[test]
    #[ignore = "requires a graphical display"]
    fn sticker_completion_routes_to_its_chat_and_topic_without_consuming_the_text_draft() {
        gtk::test_synced(|| {
            let app = crate::application(true);
            app.set_application_id(Some("io.github.skae798.tgrust.StickerRoutingTest"));
            app.set_flags(gtk::gio::ApplicationFlags::NON_UNIQUE);
            app.register(None::<&gtk::gio::Cancellable>).unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let (window, ui) = build_window(
                &app,
                Accounts::new(&app, true),
                Account::Demo(0),
                None,
                runtime,
            );
            window.set_default_size(360, 720);
            ui.show_chat(1, None);
            let video = ui.messages.borrow()[&86].clone();
            let menu = ui.message_menu_content(1, &video, None).unwrap();
            let mut child = menu.first_child();
            while let Some(button) = child {
                let text = button
                    .first_child()
                    .and_downcast::<gtk::Label>()
                    .map(|label| label.text());
                assert_ne!(
                    text.as_deref(),
                    Some(video.playback.unwrap().open_label()),
                    "Sticker menu must not open a separate video viewer"
                );
                child = button.next_sibling();
            }
            ui.input
                .buffer()
                .set_text("Текст вместе с подготовленным файлом");
            let upload = Upload {
                path: "/synthetic/notes.txt".into(),
                name: "notes.txt".into(),
                kind: UploadKind::File,
            };
            ui.uploads
                .borrow_mut()
                .insert(1.into(), vec![upload.clone()]);
            let reply = Reply {
                id: 84,
                preview: ui.messages.borrow()[&84].reply_preview(),
            };
            ui.replies.borrow_mut().insert(1.into(), reply.clone());
            ui.show_chat(2, None);
            ui.input.buffer().set_text("Другой черновик");
            let sticker =
                crate::stickers::demo_stickers(crate::model::StickerSource::Recent).remove(0);
            let message = demo::sticker_message(1000, sticker.clone());
            ui.complete_sticker_send(1.into(), message, Some(reply));
            assert!(!ui.messages.borrow().contains_key(&1000));
            assert_eq!(editor_text(&ui.input.buffer()).text, "Другой черновик");
            assert_eq!(ui.uploads.borrow()[&1.into()][0].path, upload.path);
            assert!(!ui.replies.borrow().contains_key(&1.into()));
            ui.show_chat(1, None);
            assert_eq!(ui.messages.borrow()[&1000].reply_to, Some(84));
            assert_eq!(ui.messages.borrow()[&1000].sticker, Some(sticker.clone()));
            assert_eq!(
                editor_text(&ui.input.buffer()).text,
                "Текст вместе с подготовленным файлом"
            );
            let chat = ui.chat_view(demo::FORUM_CHAT).unwrap();
            let first = Conversation {
                chat: chat.id,
                topic: Some(100),
            };
            ui.show_conversation(
                chat.clone(),
                demo::ForumTopics::for_chat(demo::FORUM_CHAT).topic(100),
                None,
            );
            ui.input.buffer().set_text("Черновик темы 100");
            ui.show_conversation(
                chat.clone(),
                demo::ForumTopics::for_chat(demo::FORUM_CHAT).topic(200),
                None,
            );
            ui.input.buffer().set_text("Черновик темы 200");
            ui.complete_sticker_send(first, demo::sticker_message(1001, sticker), None);
            assert!(!ui.messages.borrow().contains_key(&1001));
            assert_eq!(editor_text(&ui.input.buffer()).text, "Черновик темы 200");
            ui.show_conversation(
                chat,
                demo::ForumTopics::for_chat(demo::FORUM_CHAT).topic(100),
                None,
            );
            assert_eq!(ui.messages.borrow()[&1001].topic, Some(100));
            assert_eq!(editor_text(&ui.input.buffer()).text, "Черновик темы 100");
            window.close();
        });
    }

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
