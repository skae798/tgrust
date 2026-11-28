use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const MAX_ALBUM_ITEMS: usize = 10;
pub const HISTORY_PAGE_SIZE: usize = 40;
const CACHED_CHATS: usize = 150;
const CACHED_MESSAGES: usize = 400;
const CACHE_RETENTION: Duration = Duration::from_hours(6);
const CACHE_FRESHNESS: Duration = Duration::from_mins(15);

#[derive(Clone)]
pub struct Chat {
    pub id: i64,
    pub pinned: bool,
    pub last_activity: i64,
    pub title: String,
    pub preview: String,
    pub preview_sender: Option<String>,
    pub preview_image: Option<ChatImage>,
    pub group: bool,
    pub forum: bool,
    pub unread: i32,
    pub unread_mark: bool,
    pub last_message_id: i32,
    pub broadcast: bool,
    pub can_write: bool,
    pub slow_mode_enabled: bool,
    pub saved_messages: bool,
    pub archived: bool,
    pub kind: ChatKind,
    pub muted: bool,
    pub avatar: Option<Avatar>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatKind {
    Contact,
    User,
    Group,
    Broadcast,
    Bot,
}

#[derive(Clone)]
pub struct Contact {
    pub id: i64,
    pub name: String,
    pub usernames: Vec<String>,
    pub phone: Option<String>,
    pub avatar: Option<Avatar>,
    pub kind: ChatKind,
    pub is_self: bool,
    pub available: bool,
}

impl Contact {
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let text = format!("{} {}", self.name, self.usernames.join(" ")).to_lowercase();
        if query.split_whitespace().all(|word| text.contains(word)) {
            return true;
        }
        if query
            .chars()
            .all(|c| c.is_ascii_digit() || " +()- .".contains(c))
        {
            let digits: String = query.chars().filter(char::is_ascii_digit).collect();
            return !digits.is_empty()
                && self.phone.as_ref().is_some_and(|phone| {
                    phone
                        .chars()
                        .filter(char::is_ascii_digit)
                        .collect::<String>()
                        .contains(&digits)
                });
        }
        false
    }

    pub fn chat(&self) -> Chat {
        Chat {
            id: self.id,
            pinned: false,
            last_activity: 0,
            title: if self.is_self {
                "Избранное".to_owned()
            } else {
                self.name.clone()
            },
            preview: String::new(),
            preview_sender: None,
            preview_image: None,
            group: false,
            forum: false,
            unread: 0,
            unread_mark: false,
            last_message_id: 0,
            broadcast: false,
            can_write: true,
            slow_mode_enabled: false,
            saved_messages: self.is_self,
            archived: false,
            kind: self.kind,
            muted: false,
            avatar: self.avatar,
        }
    }
}

#[derive(Clone, Copy)]
pub struct ReadRequest {
    pub revision: u64,
    pub clear_mark: bool,
}

impl Chat {
    pub fn is_unread(&self) -> bool {
        self.unread > 0 || self.unread_mark
    }

    pub fn read_through(&mut self, max_id: i32) {
        self.unread_mark = false;
        if max_id > 0 && self.last_message_id <= max_id {
            self.unread = 0;
        }
    }
}

pub fn order_chats(chats: &mut [&Chat], folder: Option<&ChatFolder>) {
    chats.sort_by_key(|chat| {
        let rank = folder.map_or(usize::from(!chat.pinned), |folder| {
            folder
                .pinned
                .iter()
                .position(|id| *id == chat.id)
                .unwrap_or(usize::MAX)
        });
        let pinned = folder.map_or(chat.pinned, |folder| folder.pinned.contains(&chat.id));
        (
            rank,
            std::cmp::Reverse(if pinned { 0 } else { chat.last_activity }),
        )
    });
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountProfile {
    pub api_id: i32,
    name: Option<String>,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProxySettings {
    pub enabled: bool,
    pub host: String,
    pub port: String,
    pub username: String,
    pub password: String,
}

impl AccountProfile {
    pub fn legacy(api_id: i32) -> Self {
        Self { api_id, name: None }
    }

    pub fn named(api_id: i32, name: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(api_id > 0, "API ID должен быть положительным");
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 24
                && name.bytes().all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || matches!(c, b'-' | b'_')),
            "Метка аккаунта должна содержать от 1 до 24 строчных латинских букв, цифр, дефисов или подчёркиваний"
        );
        Ok(Self {
            api_id,
            name: Some(name.to_owned()),
        })
    }

    pub fn parse(key: &str) -> Option<Self> {
        let (id, name) = key
            .split_once('-')
            .map_or((key, None), |(id, name)| (id, Some(name)));
        let api_id: i32 = id.parse().ok()?;
        if api_id <= 0 || id != api_id.to_string() {
            return None;
        }
        match name {
            Some(name) => Self::named(api_id, name).ok(),
            None => Some(Self::legacy(api_id)),
        }
    }

    pub fn key(&self) -> String {
        match &self.name {
            Some(name) => format!("{}-{name}", self.api_id),
            None => self.api_id.to_string(),
        }
    }

    pub fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("API {}", self.api_id))
    }
}

#[derive(Clone)]
pub struct DeviceSession {
    pub id: i64,
    pub current: bool,
    pub device: String,
    pub platform: String,
    pub system_version: String,
    pub application: String,
    pub app_version: String,
    pub created: i64,
    pub active: i64,
    pub ip: String,
    pub country: String,
    pub region: String,
    pub official: bool,
    pub password_pending: bool,
    pub unconfirmed: bool,
}

impl DeviceSession {
    pub fn can_terminate(&self) -> bool {
        !self.current && self.id != 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivacyKey {
    PhoneNumber,
    AddedByPhone,
    LastSeen,
    ProfilePhoto,
}

impl PrivacyKey {
    pub const ALL: [Self; 4] = [
        Self::PhoneNumber,
        Self::AddedByPhone,
        Self::LastSeen,
        Self::ProfilePhoto,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::PhoneNumber => "Номер телефона",
            Self::AddedByPhone => "Поиск по номеру",
            Self::LastSeen => "Время последней активности",
            Self::ProfilePhoto => "Фото профиля",
        }
    }

    pub fn question(self) -> &'static str {
        match self {
            Self::PhoneNumber => "Кто видит мой номер",
            Self::AddedByPhone => "Кто может найти меня по номеру",
            Self::LastSeen => "Кто видит точное время",
            Self::ProfilePhoto => "Кто видит мои фотографии",
        }
    }

    pub fn audiences(self) -> &'static [PrivacyAudience] {
        use PrivacyAudience::*;
        match self {
            Self::AddedByPhone => &[Everyone, Contacts],
            _ => &[Everyone, Contacts, Nobody],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivacyAudience {
    Everyone,
    Contacts,
    Nobody,
}

impl PrivacyAudience {
    pub fn label(self) -> &'static str {
        match self {
            Self::Everyone => "Все",
            Self::Contacts => "Мои контакты",
            Self::Nobody => "Никто",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PrivacySetting {
    pub audience: Option<PrivacyAudience>,
    pub exception_rules: usize,
    pub editable: bool,
    pub(crate) revision: Vec<u8>,
}

pub enum PrivacyChange {
    Saved {
        setting: PrivacySetting,
        exceptions_changed: bool,
    },
    Conflict(PrivacySetting),
}

pub const SESSION_TTL_DAYS: [i32; 5] = [7, 30, 90, 180, 365];

#[derive(Clone, Copy)]
pub enum DeviceChange {
    Terminate(i64),
    TerminateOthers,
    SetTtl(i32),
}

#[derive(Clone)]
pub struct DeviceSessions {
    pub sessions: Vec<DeviceSession>,
    pub inactive_ttl_days: i32,
}

impl DeviceSessions {
    pub fn apply_confirmed(&mut self, change: DeviceChange) {
        match change {
            DeviceChange::Terminate(id) => self
                .sessions
                .retain(|session| session.id != id || session.current),
            DeviceChange::TerminateOthers => self.sessions.retain(|session| session.current),
            DeviceChange::SetTtl(days) => self.inactive_ttl_days = days,
        }
    }
}

#[derive(Clone)]
pub struct PeerProfile {
    pub id: i64,
    pub title: String,
    pub kind: ChatKind,
    pub is_self: bool,
    pub blocked: Option<bool>,
    pub avatar: Option<Avatar>,
    pub about: String,
    pub usernames: Vec<String>,
    pub phone: Option<String>,
    pub members: Option<u32>,
    pub online_members: Option<u32>,
    pub verified: bool,
    pub scam: bool,
    pub fake: bool,
    pub can_edit_chat: bool,
    pub can_view_members: bool,
    pub moderation: Option<MemberModeration>,
    pub admin_policy: Option<AdminPolicy>,
    pub can_manage_invites: bool,
    pub can_add_members: bool,
    pub can_read_admin_log: bool,
    pub addresses: Option<ChatAddresses>,
    pub group_permissions: Option<GroupPermissionSettings>,
    pub forum_mode: Option<ForumMode>,
    pub bot_commands: Vec<BotCommand>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForumMode {
    pub enabled: bool,
    pub linked_discussion: bool,
    pub hidden_history: bool,
}

impl ForumMode {
    pub fn needs_change(self, confirmed: Self) -> anyhow::Result<bool> {
        let enable = !confirmed.enabled;
        if self.enabled == enable {
            return Ok(false);
        }
        if enable {
            anyhow::ensure!(
                !self.linked_discussion,
                "Темы недоступны в группе обсуждения, привязанной к каналу"
            );
            anyhow::ensure!(
                !self.hidden_history || confirmed.hidden_history,
                "Видимость истории изменилась. Откройте настройки тем снова"
            );
        }
        Ok(true)
    }
}

pub const ADMIN_LOG_PAGE_SIZE: usize = 25;

pub const GENERAL_TOPIC: i32 = 1;
pub const FORUM_PAGE_SIZE: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Conversation {
    pub chat: i64,
    pub topic: Option<i32>,
}

impl From<i64> for Conversation {
    fn from(chat: i64) -> Self {
        Self { chat, topic: None }
    }
}

#[derive(Clone)]
pub struct ForumTopic {
    pub id: i32,
    pub title: String,
    pub unread: i32,
    pub closed: bool,
    pub hidden: bool,
    pub pinned: bool,
    pub mine: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ForumCursor {
    pub date: i32,
    pub message: i32,
    pub topic: i32,
}

pub struct ForumPage {
    pub topics: Vec<ForumTopic>,
    pub next: Option<ForumCursor>,
    pub permissions: Option<ForumPermissions>,
}

#[derive(Clone, Copy, Default)]
pub struct ForumPermissions {
    pub create: bool,
    pub manage: bool,
    pub delete_messages: bool,
}

impl ForumPermissions {
    pub fn allows(self, topic: &ForumTopic, change: &ForumTopicChange) -> bool {
        match change {
            ForumTopicChange::Rename(_) | ForumTopicChange::Closed(_) => self.manage || topic.mine,
            ForumTopicChange::Hidden(_) => self.manage && topic.id == GENERAL_TOPIC,
            ForumTopicChange::Pinned(_) => self.manage,
            ForumTopicChange::Delete => {
                topic.id != GENERAL_TOPIC && (self.delete_messages || topic.mine)
            }
        }
    }
}

#[derive(Clone)]
pub enum ForumTopicChange {
    Rename(String),
    Closed(bool),
    Hidden(bool),
    Pinned(bool),
    Delete,
}

impl ForumTopicChange {
    pub fn needs_write(&self, original: &ForumTopic, current: &ForumTopic) -> anyhow::Result<bool> {
        use ForumTopicChange::*;
        anyhow::ensure!(current.id == original.id, "Telegram вернул другую тему");
        match self {
            Rename(title) => {
                let title = forum_topic_title(title)?;
                anyhow::ensure!(
                    current.title == original.title || current.title == title,
                    "Название изменилось. Откройте тему снова"
                );
                Ok(current.title != title)
            }
            Closed(closed) => Ok(current.closed != *closed),
            Hidden(hidden) => {
                anyhow::ensure!(
                    current.id == GENERAL_TOPIC,
                    "Скрывать можно только общую тему"
                );
                Ok(current.hidden != *hidden)
            }
            Pinned(pinned) => Ok(current.pinned != *pinned),
            Delete => {
                anyhow::ensure!(current.id != GENERAL_TOPIC, "Общую тему удалить нельзя");
                anyhow::ensure!(
                    current.title == original.title,
                    "Название изменилось. Проверьте тему перед удалением"
                );
                Ok(true)
            }
        }
    }
}

pub enum ForumWrite {
    Create {
        title: String,
        random_id: i64,
    },
    Change {
        original: ForumTopic,
        change: ForumTopicChange,
    },
}

pub enum ForumWriteResult {
    Created(ForumTopic),
    Updated(ForumTopic),
    Deleted(i32),
    CreationUnconfirmed,
}

pub fn forum_topic_title(title: &str) -> anyhow::Result<String> {
    let title = title.trim();
    anyhow::ensure!(!title.is_empty(), "Введите название темы");
    anyhow::ensure!(
        title.len() <= 128 && !title.chars().any(char::is_control),
        "Введите название без переносов, до 128 байт в UTF-8"
    );
    Ok(title.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupPermission {
    Text,
    Photos,
    Videos,
    VideoNotes,
    Music,
    Voice,
    Documents,
    StickersAndBots,
    LinkPreviews,
    Polls,
    Reactions,
    Invite,
    Pin,
    ChangeInfo,
    Topics,
    EditTag,
}

impl GroupPermission {
    pub const OPTIONS: [(Self, &str); 16] = {
        use GroupPermission::*;
        [
            (Text, "Отправлять текст"),
            (Photos, "Отправлять фотографии"),
            (Videos, "Отправлять видео"),
            (VideoNotes, "Отправлять видеосообщения"),
            (Music, "Отправлять музыку"),
            (Voice, "Отправлять голосовые"),
            (Documents, "Отправлять файлы"),
            (
                StickersAndBots,
                "Отправлять стикеры, GIF, игры и ответы ботов",
            ),
            (LinkPreviews, "Показывать предпросмотр ссылок"),
            (Polls, "Создавать опросы"),
            (Reactions, "Ставить реакции"),
            (Invite, "Добавлять участников"),
            (Pin, "Закреплять сообщения"),
            (ChangeInfo, "Изменять сведения о группе"),
            (Topics, "Создавать темы"),
            (EditTag, "Менять свой тег"),
        ]
    };

    pub fn is_sending(self) -> bool {
        !matches!(
            self,
            Self::Invite | Self::Pin | Self::ChangeInfo | Self::Topics | Self::EditTag
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupPermissions(u32);

impl GroupPermissions {
    pub fn all() -> Self {
        Self((1 << GroupPermission::OPTIONS.len()) - 1)
    }

    pub fn has(self, permission: GroupPermission) -> bool {
        self.0 & (1 << permission as u32) != 0
    }

    pub fn with(self, permission: GroupPermission, enabled: bool) -> Self {
        let bit = 1 << permission as u32;
        Self((self.0 & !bit) | if enabled { bit } else { 0 })
    }

    pub fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupPermissionSettings {
    pub allowed: GroupPermissions,
    pub editable: GroupPermissions,
    pub forum: bool,
    pub slow_mode: Option<i32>,
}

pub const SLOW_MODE_OPTIONS: [(i32, &str); 8] = [
    (0, "Выключен"),
    (5, "5 секунд"),
    (10, "10 секунд"),
    (30, "30 секунд"),
    (60, "1 минута"),
    (300, "5 минут"),
    (900, "15 минут"),
    (3600, "1 час"),
];

#[derive(Clone, Copy)]
pub struct GroupPermissionEdit {
    pub allowed: GroupPermissions,
    pub slow_mode: Option<i32>,
}

pub fn slow_mode_change(
    current: Option<i32>,
    original: Option<i32>,
    requested: Option<i32>,
) -> anyhow::Result<Option<i32>> {
    if requested == original {
        return Ok(None);
    }
    let seconds = requested.filter(|seconds| {
        SLOW_MODE_OPTIONS
            .iter()
            .any(|(option, _)| option == seconds)
    });
    anyhow::ensure!(
        original.is_some() && current.is_some() && seconds.is_some(),
        "Настройка медленного режима недоступна. Откройте разрешения группы снова"
    );
    Ok(seconds.filter(|_| seconds != current))
}

impl GroupPermissionSettings {
    pub fn changes(
        self,
        original: Self,
        requested: GroupPermissions,
    ) -> anyhow::Result<Vec<(GroupPermission, bool)>> {
        use GroupPermission::{LinkPreviews, Text};
        let text_changed = requested.has(Text) != original.allowed.has(Text);
        let enabling_text = text_changed && requested.has(Text) && !self.allowed.has(Text);
        let requested = requested.with(
            LinkPreviews,
            requested.has(Text) && requested.has(LinkPreviews),
        );
        let mut changes = Vec::new();
        for (permission, _) in GroupPermission::OPTIONS {
            let desired = requested.has(permission);
            let changed = desired != original.allowed.has(permission)
                || (permission == LinkPreviews && text_changed);
            let needs_write = desired != self.allowed.has(permission)
                || (permission == LinkPreviews && enabling_text);
            if changed && needs_write {
                anyhow::ensure!(
                    self.editable.has(permission) && original.editable.has(permission),
                    "Право на изменение разрешений изменилось. Откройте редактор снова"
                );
                changes.push((permission, desired));
            }
        }
        if changes.contains(&(LinkPreviews, true)) {
            anyhow::ensure!(
                requested.has(Text) && (self.allowed.has(Text) || changes.contains(&(Text, true))),
                "Для предпросмотра ссылок разрешите отправку текста"
            );
        }
        Ok(changes)
    }

    pub fn apply(&mut self, changes: &[(GroupPermission, bool)]) {
        for &(permission, enable) in changes {
            self.allowed = self.allowed.with(permission, enable);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatAddresses {
    pub primary: String,
    pub aliases: Vec<AttachedAddress>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachedAddress {
    pub name: String,
    pub active: bool,
    pub can_toggle: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatAddressChange {
    Primary(String),
    Alias(AttachedAddress),
}

impl ChatAddresses {
    pub fn is_public(&self) -> bool {
        !self.primary.is_empty() || self.aliases.iter().any(|address| address.active)
    }

    pub fn active_usernames(&self) -> Vec<String> {
        std::iter::once(self.primary.as_str())
            .filter(|name| !name.is_empty())
            .chain(
                self.aliases
                    .iter()
                    .filter(|address| address.active)
                    .map(|address| address.name.as_str()),
            )
            .map(|name| format!("@{name}"))
            .collect()
    }

    pub fn changes(
        &self,
        original: &Self,
        current: &Self,
    ) -> anyhow::Result<Vec<ChatAddressChange>> {
        let primary = if self.primary == original.primary {
            original.primary.clone()
        } else {
            chat_username(&self.primary)?
        };
        let make_private = original.is_public()
            && primary.is_empty()
            && self.aliases.iter().all(|address| !address.active);
        let changed_primary = !primary.eq_ignore_ascii_case(&original.primary)
            && !primary.eq_ignore_ascii_case(&current.primary);
        if changed_primary {
            anyhow::ensure!(
                original.primary.eq_ignore_ascii_case(&current.primary),
                "Основное имя уже изменилось. Откройте редактор снова"
            );
            anyhow::ensure!(
                !current
                    .aliases
                    .iter()
                    .any(|address| address.name.eq_ignore_ascii_case(&primary)),
                "Это коллекционное имя. Измените его состояние в списке ниже"
            );
        }

        let mut enable = Vec::new();
        let mut disable = Vec::new();
        for desired in &self.aliases {
            let same_name =
                |address: &&AttachedAddress| address.name.eq_ignore_ascii_case(&desired.name);
            let baseline = original.aliases.iter().find(same_name);
            if baseline.is_some_and(|address| address.active == desired.active) {
                continue;
            }
            let latest = current
                .aliases
                .iter()
                .find(same_name)
                .filter(|_| baseline.is_some())
                .ok_or_else(|| {
                    anyhow::anyhow!("Список коллекционных имён изменился. Откройте редактор снова")
                })?;
            if desired.active != latest.active {
                anyhow::ensure!(
                    latest.can_toggle && baseline.is_some_and(|address| address.can_toggle),
                    "Telegram не разрешает менять состояние этого имени"
                );
                let change = ChatAddressChange::Alias(AttachedAddress {
                    name: latest.name.clone(),
                    active: desired.active,
                    can_toggle: latest.can_toggle,
                });
                if desired.active {
                    enable.push(change);
                } else {
                    disable.push(change);
                }
            }
        }

        let mut changes = Vec::new();
        if changed_primary && !primary.is_empty() {
            changes.push(ChatAddressChange::Primary(primary.clone()));
        }
        let has_active_remaining = current.aliases.iter().any(|address| {
            address.active && !disable.iter().any(|change| {
                matches!(change, ChatAddressChange::Alias(removed) if removed.name == address.name)
            })
        });
        // Free slots before activating other aliases when an address can stay active.
        if has_active_remaining || !current.primary.is_empty() || !changes.is_empty() {
            changes.append(&mut disable);
        }
        changes.append(&mut enable);
        if changed_primary && primary.is_empty() {
            changes.push(ChatAddressChange::Primary(primary));
        }
        changes.append(&mut disable);
        if make_private {
            let mut remaining = current.clone();
            for change in &changes {
                remaining.apply(change.clone());
            }
            anyhow::ensure!(
                !remaining.is_public(),
                "Список публичных адресов изменился. Откройте редактор снова"
            );
        }
        Ok(changes)
    }

    pub fn apply(&mut self, change: ChatAddressChange) {
        match change {
            ChatAddressChange::Primary(name) => self.primary = name,
            ChatAddressChange::Alias(desired) => {
                if let Some(address) = self
                    .aliases
                    .iter_mut()
                    .find(|address| address.name == desired.name)
                {
                    address.active = desired.active;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AdminLogFilter {
    #[default]
    All,
    Members,
    Permissions,
    Messages,
    Settings,
    Invites,
    Topics,
    Calls,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdminLogQuery {
    pub text: String,
    pub filter: AdminLogFilter,
    pub actor: Option<i64>,
}

pub struct AdminLogActor {
    pub filter_id: Option<i64>,
    pub name: String,
    pub avatar: Option<Avatar>,
}

pub struct AdminLogEntry {
    pub id: i64,
    pub date: i32,
    pub actor: AdminLogActor,
    pub action: String,
    pub details: Option<String>,
}

pub struct AdminLogPage {
    pub entries: Vec<AdminLogEntry>,
    pub next: Option<i64>,
}

impl AdminLogPage {
    pub fn new(mut entries: Vec<AdminLogEntry>, before: Option<i64>) -> Self {
        entries.retain(|entry| entry.id > 0 && before.is_none_or(|before| entry.id < before));
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.id));
        entries.dedup_by_key(|entry| entry.id);
        let next = entries.last().map(|entry| entry.id).filter(|&id| id > 1);
        Self { entries, next }
    }
}

pub const INVITE_PAGE_SIZE: i32 = 25;

#[derive(Clone, PartialEq, Eq)]
pub enum MemberInvitation {
    InChat,
    Rejected(String),
    Unconfirmed,
    NotSent,
}

pub type MemberInvitations = Vec<(String, MemberInvitation)>;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct InviteCursor {
    pub date: i32,
    pub link: String,
}

#[derive(Clone)]
pub struct InviteLink {
    pub cursor: InviteCursor,
    pub title: String,
    pub permanent: bool,
    pub revoked: bool,
    pub expires: Option<i32>,
    pub limit: Option<i32>,
    pub uses: i32,
    pub approval: bool,
    pub pending: i32,
    pub paid: bool,
}

impl InviteLink {
    pub fn editable(&self) -> bool {
        !self.revoked && !self.permanent
    }

    pub fn expired(&self, now: i64) -> bool {
        self.expires
            .is_some_and(|expires| i64::from(expires) <= now)
    }

    pub fn exhausted(&self) -> bool {
        self.limit.is_some_and(|limit| self.uses >= limit)
    }
}

pub struct InvitePage {
    pub links: Vec<InviteLink>,
    pub next: Option<InviteCursor>,
}

pub struct InviteDraft {
    pub title: String,
    pub hours: Option<i32>,
    pub limit: Option<i32>,
    pub approval: bool,
}

impl InviteDraft {
    pub fn normalize(mut self) -> anyhow::Result<Self> {
        self.title = self.title.trim().to_owned();
        validate_invite_fields(&self.title, self.limit, self.approval)?;
        anyhow::ensure!(
            self.hours.is_none_or(|hours| (1..=168).contains(&hours)),
            "Неверный срок действия"
        );
        Ok(self)
    }

    pub fn expiration(&self, now: u64) -> anyhow::Result<Option<i32>> {
        self.hours
            .map(|hours| {
                now.checked_add(hours as u64 * 3600)
                    .and_then(|expires| i32::try_from(expires).ok())
                    .ok_or_else(|| anyhow::anyhow!("Невозможно установить срок действия"))
            })
            .transpose()
    }
}

fn validate_invite_fields(title: &str, limit: Option<i32>, approval: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        title.chars().count() <= 32,
        "Название должно содержать до 32 символов"
    );
    validate_invite_mode(limit, approval)
}

fn validate_invite_mode(limit: Option<i32>, approval: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        limit.is_none_or(|limit| (1..=99_999).contains(&limit)),
        "Укажите от 1 до 99 999 вступлений или оставьте поле пустым"
    );
    anyhow::ensure!(
        !approval || limit.is_none(),
        "Для ссылок с одобрением лимит вступлений недоступен"
    );
    Ok(())
}

pub struct InviteEdit {
    pub title: String,
    pub expires: Option<i32>,
    pub limit: Option<i32>,
    pub approval: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct InviteChanges {
    pub title: Option<String>,
    pub expire_date: Option<i32>,
    pub usage_limit: Option<i32>,
    pub request_needed: Option<bool>,
}

impl InviteEdit {
    pub fn changes(
        &self,
        original: &InviteLink,
        current: &InviteLink,
    ) -> anyhow::Result<InviteChanges> {
        anyhow::ensure!(
            current.editable()
                && original.cursor.link == current.cursor.link
                && original.paid == current.paid,
            "Эту ссылку нельзя редактировать"
        );
        let title = if self.title == original.title {
            self.title.as_str()
        } else {
            self.title.trim()
        };
        let title = invite_field_change(title, original.title.as_str(), current.title.as_str())?;
        if let Some(title) = title {
            anyhow::ensure!(
                title.chars().count() <= 32,
                "Название должно содержать до 32 символов"
            );
        }
        let expiry = invite_field_change(self.expires, original.expires, current.expires)?;
        let mode = invite_field_change(
            (self.approval, self.limit),
            (original.approval, original.limit),
            (current.approval, current.limit),
        )?;
        anyhow::ensure!(
            !current.paid || (expiry.is_none() && mode.is_none()),
            "Для подписки можно менять только название"
        );
        if let Some((approval, limit)) = mode {
            validate_invite_mode(limit, approval)?;
        }
        Ok(InviteChanges {
            title: title.map(str::to_owned),
            expire_date: expiry.map(Option::unwrap_or_default),
            usage_limit: mode.map(|(_, limit)| limit.unwrap_or_default()),
            request_needed: mode.map(|(approval, _)| approval),
        })
    }
}

fn invite_field_change<T: PartialEq>(
    requested: T,
    original: T,
    current: T,
) -> anyhow::Result<Option<T>> {
    if requested == original || requested == current {
        Ok(None)
    } else if current == original {
        Ok(Some(requested))
    } else {
        anyhow::bail!("Ссылка изменилась в другом клиенте. Откройте редактор заново")
    }
}

impl InviteChanges {
    pub fn confirmed_by(&self, link: &InviteLink) -> bool {
        self.title.as_ref().is_none_or(|title| title == &link.title)
            && self
                .expire_date
                .is_none_or(|date| date == link.expires.unwrap_or_default())
            && self
                .usage_limit
                .is_none_or(|limit| limit == link.limit.unwrap_or_default())
            && self
                .request_needed
                .is_none_or(|approval| approval == link.approval)
    }
}

pub enum InviteAction {
    Create(InviteDraft),
    Edit(InviteLink, InviteEdit),
    Revoke(String),
}

pub const JOIN_REQUEST_PAGE_SIZE: i32 = 25;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct JoinRequestCursor {
    pub date: i32,
    pub user: i64,
}

pub struct JoinRequest {
    pub cursor: JoinRequestCursor,
    pub name: String,
    pub about: String,
    pub avatar: Option<Avatar>,
    pub actionable: bool,
}

pub struct JoinRequestPage {
    pub requests: Vec<JoinRequest>,
    pub next: Option<JoinRequestCursor>,
    pub total: usize,
}

#[derive(Clone, Copy)]
pub enum JoinDecision {
    Approve,
    Decline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinDecisionOutcome {
    Applied,
    AlreadyHandled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatInfo {
    pub title: String,
    pub about: String,
}

impl ChatInfo {
    pub fn changes(
        &self,
        original: &Self,
        current: &Self,
    ) -> anyhow::Result<(Option<String>, Option<String>)> {
        let title = self.title.trim();
        anyhow::ensure!(
            !title.is_empty() && !title.contains(['\r', '\n']),
            "Введите название чата"
        );
        let field = |proposed: &str, before: &str, now: &str| {
            if proposed == before || proposed == now {
                Ok(None)
            } else if before == now {
                Ok(Some(proposed.to_owned()))
            } else {
                anyhow::bail!("Данные чата изменились. Откройте редактор снова")
            }
        };
        Ok((
            field(title, &original.title, &current.title)?,
            field(&self.about, &original.about, &current.about)?,
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewChatKind {
    Group,
    Supergroup,
    Channel,
}

#[derive(Clone)]
pub struct NewChatDraft {
    pub kind: NewChatKind,
    pub forum: bool,
    pub info: ChatInfo,
    pub username: String,
    pub members: Vec<Contact>,
}

impl NewChatDraft {
    pub fn validate(mut self) -> anyhow::Result<Self> {
        self.info.title = self.info.title.trim().to_owned();
        self.info.about = self.info.about.trim().to_owned();
        anyhow::ensure!(
            !self.info.title.is_empty() && !self.info.title.contains(['\r', '\n']),
            "Введите название чата"
        );
        anyhow::ensure!(
            self.info.title.chars().count() <= 128 && self.info.about.chars().count() <= 255,
            "Название должно содержать до 128 символов, описание до 255"
        );
        self.username = chat_username(&self.username)?;
        let mut seen = HashSet::new();
        self.members.retain(|member| {
            member.id > 0 && !member.is_self && member.available && seen.insert(member.id)
        });
        anyhow::ensure!(
            self.kind != NewChatKind::Group || self.username.is_empty(),
            "Публичное имя доступно для супергрупп и каналов"
        );
        anyhow::ensure!(
            !self.forum || self.kind == NewChatKind::Supergroup,
            "Темы доступны в супергруппах"
        );
        anyhow::ensure!(
            self.kind != NewChatKind::Group || !self.members.is_empty(),
            "Выберите хотя бы одного участника"
        );
        anyhow::ensure!(
            self.kind != NewChatKind::Channel
                || self
                    .members
                    .iter()
                    .all(|member| member.kind != ChatKind::Bot),
            "Ботов можно добавлять в канал через назначение администратора"
        );
        Ok(self)
    }

    pub fn chat(&self, id: i64, date: i64) -> Chat {
        Chat {
            id,
            pinned: false,
            last_activity: date,
            title: self.info.title.clone(),
            preview: String::new(),
            preview_sender: None,
            preview_image: None,
            group: self.kind != NewChatKind::Channel,
            forum: self.forum,
            unread: 0,
            unread_mark: false,
            last_message_id: 0,
            broadcast: self.kind == NewChatKind::Channel,
            can_write: true,
            slow_mode_enabled: false,
            saved_messages: false,
            avatar: None,
            archived: false,
            kind: if self.kind == NewChatKind::Channel {
                ChatKind::Broadcast
            } else {
                ChatKind::Group
            },
            muted: false,
        }
    }
}

pub fn chat_username(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    let username = value.strip_prefix('@').unwrap_or(value);
    anyhow::ensure!(
        value.is_empty()
            || ((5..=32).contains(&username.len())
                && username.starts_with(|c: char| c.is_ascii_alphabetic())
                && username.ends_with(|c: char| c.is_ascii_alphanumeric())
                && !username.contains("__")
                && username
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_')),
        "Публичное имя должно содержать от 5 до 32 латинских букв, цифр или знаков подчёркивания"
    );
    Ok(username.to_owned())
}

pub struct CreatedChat {
    pub chat: Chat,
    pub draft: NewChatDraft,
    pub warnings: Vec<String>,
    pub public_error: Option<String>,
}

pub enum NewChatResult {
    Created(Box<CreatedChat>),
    Rejected(String),
    Unconfirmed(String),
}

impl PeerProfile {
    pub fn chat_info(&self) -> ChatInfo {
        ChatInfo {
            title: self.title.clone(),
            about: self.about.clone(),
        }
    }
}

pub const MEMBER_PAGE_SIZE: i32 = 40;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MemberFilter {
    All,
    Admins,
    Bots,
    Restricted,
    Banned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberRole {
    Owner,
    Administrator,
    Member,
    Restricted,
    RestrictedFormer,
    Banned,
}

impl MemberRole {
    pub fn restricted(self) -> bool {
        matches!(self, Self::Restricted | Self::RestrictedFormer)
    }

    pub fn in_chat(self) -> bool {
        matches!(
            self,
            Self::Owner | Self::Administrator | Self::Member | Self::Restricted
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberModeration {
    Remove,
    Ban,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberChange {
    Remove,
    Ban,
    Unban,
    SetAdmin {
        original: AdminState,
        requested: AdminState,
    },
}

#[derive(Clone, Copy)]
pub enum AdminRight {
    ChangeInfo,
    PostMessages,
    EditMessages,
    DeleteMessages,
    BanUsers,
    InviteUsers,
    PinMessages,
    AddAdmins,
    Anonymous,
    ManageCall,
    Other,
    ManageTopics,
    PostStories,
    EditStories,
    DeleteStories,
    ManageDirectMessages,
    ManageRanks,
    ManageLinkedPeers,
    ManageWelcomeMessages,
}

impl AdminRight {
    pub const OPTIONS: [(Self, &'static str); 18] = {
        use AdminRight::*;
        [
            (ChangeInfo, "Изменять информацию"),
            (PostMessages, "Публиковать сообщения"),
            (EditMessages, "Редактировать сообщения"),
            (DeleteMessages, "Удалять сообщения"),
            (BanUsers, "Блокировать участников"),
            (InviteUsers, "Приглашать участников"),
            (PinMessages, "Закреплять сообщения"),
            (ManageTopics, "Управлять темами"),
            (ManageCall, "Управлять видеочатами"),
            (Anonymous, "Сохранять анонимность"),
            (PostStories, "Публиковать истории"),
            (EditStories, "Редактировать истории"),
            (DeleteStories, "Удалять истории"),
            (ManageDirectMessages, "Управлять личными сообщениями канала"),
            (ManageRanks, "Менять теги участников"),
            (ManageLinkedPeers, "Управлять связанными чатами"),
            (ManageWelcomeMessages, "Менять приветственные сообщения"),
            (AddAdmins, "Назначать администраторов"),
        ]
    };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdminRights(u32);

impl AdminRights {
    pub fn from_rights(rights: impl IntoIterator<Item = AdminRight>) -> Self {
        rights
            .into_iter()
            .fold(Self::default(), |mask, right| mask.with(right, true))
    }

    pub fn has(self, right: AdminRight) -> bool {
        self.0 & (1 << right as u32) != 0
    }

    pub fn with(self, right: AdminRight, enabled: bool) -> Self {
        let bit = 1 << right as u32;
        Self((self.0 & !bit) | if enabled { bit } else { 0 })
    }

    pub fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub fn channel(broadcast: bool, forum: bool, direct: bool) -> Self {
        use AdminRight::*;
        Self::from_rights([
            ChangeInfo,
            DeleteMessages,
            BanUsers,
            InviteUsers,
            AddAdmins,
            ManageCall,
            Other,
            PostStories,
            EditStories,
            DeleteStories,
            ManageWelcomeMessages,
        ])
        .with(PostMessages, broadcast)
        .with(EditMessages, broadcast)
        .with(ManageDirectMessages, broadcast && direct)
        .with(PinMessages, !broadcast)
        .with(Anonymous, !broadcast)
        .with(ManageTopics, !broadcast && forum)
        .with(ManageRanks, !broadcast)
        .with(ManageLinkedPeers, !broadcast)
    }
}

#[derive(Clone, Copy)]
pub enum AdminPolicy {
    Basic,
    Channel(AdminRights),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminState {
    Member,
    Administrator(AdminRights),
}

impl AdminState {
    pub fn rights(self) -> AdminRights {
        match self {
            Self::Member => AdminRights::default(),
            Self::Administrator(rights) => rights,
        }
    }

    pub fn channel_rights(self) -> AdminRights {
        self.rights().with(AdminRight::Other, self != Self::Member)
    }
}

impl AdminPolicy {
    pub fn needs_change(
        self,
        original: AdminState,
        current: AdminState,
        requested: AdminState,
    ) -> anyhow::Result<bool> {
        let equal = |left: AdminState, right: AdminState| match self {
            Self::Basic => left == right,
            Self::Channel(_) => left.channel_rights() == right.channel_rights(),
        };
        if equal(requested, current) {
            return Ok(false);
        }
        anyhow::ensure!(
            equal(current, original),
            "Роль или права уже изменились. Обновите список участников"
        );
        match (self, requested) {
            (Self::Basic, AdminState::Administrator(rights)) => anyhow::ensure!(
                rights == AdminRights::default(),
                "В обычной группе используется общий набор прав администратора"
            ),
            (Self::Channel(allowed), AdminState::Administrator(_)) => {
                let changed = requested.channel_rights().0 ^ current.channel_rights().0;
                anyhow::ensure!(
                    changed & !allowed.0 == 0,
                    "Набор прав превышает доступные для изменения права"
                );
            }
            (_, AdminState::Member) => {}
        }
        Ok(true)
    }
}

impl MemberModeration {
    pub fn action(self, role: MemberRole) -> Option<MemberChange> {
        match (self, role) {
            (Self::Remove, MemberRole::Member) => Some(MemberChange::Remove),
            (Self::Ban, MemberRole::Member | MemberRole::Restricted) => Some(MemberChange::Ban),
            (Self::Ban, MemberRole::Banned) => Some(MemberChange::Unban),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct ChatMember {
    pub id: i64,
    pub name: String,
    pub avatar: Option<Avatar>,
    pub role: MemberRole,
    pub bot: bool,
    pub can_update: bool,
    pub admin: Option<AdminState>,
}

impl MemberFilter {
    pub fn includes(self, member: &ChatMember) -> bool {
        match self {
            Self::All => member.role.in_chat(),
            Self::Admins => matches!(member.role, MemberRole::Owner | MemberRole::Administrator),
            Self::Bots => member.bot && member.role.in_chat(),
            Self::Restricted => member.role.restricted(),
            Self::Banned => member.role == MemberRole::Banned,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemberRestriction {
    pub allowed: GroupPermissions,
    pub until: i32,
    pub other_flags: bool,
}

impl Default for MemberRestriction {
    fn default() -> Self {
        Self {
            allowed: GroupPermissions::all(),
            until: 0,
            other_flags: false,
        }
    }
}

impl MemberRestriction {
    pub fn active(self) -> bool {
        self.other_flags || self.allowed != GroupPermissions::all()
    }
}

#[derive(Clone, Copy)]
pub struct MemberRestrictionSettings {
    pub group: GroupPermissionSettings,
    pub personal: MemberRestriction,
}

#[derive(Clone, Copy)]
pub enum MemberRestrictionEdit {
    Save {
        allowed: GroupPermissions,
        until: Option<i32>,
    },
    Clear,
}

impl MemberRestrictionSettings {
    pub fn view(self) -> GroupPermissionSettings {
        use GroupPermission::{LinkPreviews, Text, Topics};
        let allowed = self.group.allowed.intersect(self.personal.allowed);
        GroupPermissionSettings {
            allowed: allowed.with(LinkPreviews, allowed.has(Text) && allowed.has(LinkPreviews)),
            editable: self
                .group
                .allowed
                .with(Topics, self.group.forum && self.group.allowed.has(Topics)),
            slow_mode: None,
            ..self.group
        }
    }

    pub fn prepare(
        self,
        original: Self,
        edit: MemberRestrictionEdit,
        now: i32,
    ) -> anyhow::Result<Option<MemberRestriction>> {
        use GroupPermission::{LinkPreviews, Text};
        let mut next = self.personal;
        match edit {
            MemberRestrictionEdit::Clear => {
                if self.personal.active() {
                    anyhow::ensure!(
                        original.personal == self.personal,
                        "Ограничения участника изменились. Откройте редактор снова"
                    );
                }
                next = MemberRestriction::default();
            }
            MemberRestrictionEdit::Save { allowed, until } => {
                let original_view = original.view();
                let current_view = self.view();
                let text_changed = original_view.allowed.has(Text) != allowed.has(Text);
                for (permission, _) in GroupPermission::OPTIONS {
                    let enabled = allowed.has(permission);
                    let changed = original_view.allowed.has(permission) != enabled;
                    let set_previews = permission == LinkPreviews
                        && text_changed
                        && allowed.has(Text)
                        && original_view.editable.has(LinkPreviews);
                    if !changed && !set_previews {
                        continue;
                    }
                    if permission == LinkPreviews && !allowed.has(Text) && !enabled {
                        continue;
                    }
                    anyhow::ensure!(
                        original_view.editable.has(permission)
                            && current_view.editable.has(permission),
                        "Общие разрешения изменились. Откройте редактор снова"
                    );
                    next.allowed = next.allowed.with(permission, enabled);
                }
                if self.personal.allowed.has(Text) && !next.allowed.has(Text) {
                    next.allowed = next.allowed.with(LinkPreviews, false);
                }
                if allowed.has(LinkPreviews)
                    && !current_view.allowed.has(LinkPreviews)
                    && original_view.allowed.has(LinkPreviews) != allowed.has(LinkPreviews)
                {
                    anyhow::ensure!(
                        next.allowed.has(Text) && self.group.allowed.has(Text),
                        "Для предпросмотра ссылок нужна отправка текста"
                    );
                }
                if !next.active() {
                    next = MemberRestriction::default();
                } else if let Some(until) = until {
                    next.until = until;
                }
            }
        }
        if next == self.personal {
            return Ok(None);
        }
        anyhow::ensure!(
            next.until == 0
                || (30..=366 * 86_400).contains(&(i64::from(next.until) - i64::from(now))),
            "Укажите новый срок. Telegram принимает от 30 секунд до 366 дней"
        );
        Ok(Some(next))
    }
}

pub fn restriction_now() -> i32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|now| i32::try_from(now.as_secs()).ok())
        .unwrap_or(i32::MAX)
}

pub struct MemberPage {
    pub members: Vec<ChatMember>,
    pub next: Option<i32>,
}

#[derive(Clone)]
pub struct BotCommand {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct ProfileDetails {
    pub first_name: String,
    pub last_name: String,
    pub about: String,
}

impl ProfileDetails {
    pub fn display_name(&self) -> String {
        if self.last_name.is_empty() {
            self.first_name.clone()
        } else if self.first_name.is_empty() {
            self.last_name.clone()
        } else {
            format!("{} {}", self.first_name, self.last_name)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnProfile {
    pub id: i64,
    pub details: ProfileDetails,
    pub username: String,
    pub avatar: Option<Avatar>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountIdentity {
    pub user_id: i64,
    pub name: String,
    pub username: String,
}

pub enum ProfileEdit {
    Details(ProfileDetails),
    Username(String),
}

pub enum ProfileEditResult {
    Saved(OwnProfile),
    Conflict(OwnProfile),
    Confirmed(OwnProfile),
}

pub const BLOCKED_PAGE_SIZE: i32 = 40;

pub struct BlockedPeer {
    pub id: i64,
    pub name: String,
    pub username: Option<String>,
    pub avatar: Option<Avatar>,
    pub available: bool,
}

pub struct BlockedPage {
    pub peers: Vec<BlockedPeer>,
    pub next_offset: Option<i32>,
}

#[derive(Clone, Default)]
pub struct ChatFolder {
    pub id: i32,
    pub title: String,
    pub pinned: Vec<i64>,
    pub included: Vec<i64>,
    pub excluded: Vec<i64>,
    pub kinds: Vec<ChatKind>,
    pub exclude_muted: bool,
    pub exclude_read: bool,
    pub exclude_archived: bool,
}

impl ChatFolder {
    pub fn contains(&self, chat: &Chat) -> bool {
        if self.excluded.contains(&chat.id) {
            return false;
        }
        if self.pinned.contains(&chat.id) || self.included.contains(&chat.id) {
            return true;
        }
        self.kinds.contains(&chat.kind)
            && (!self.exclude_muted || !chat.muted)
            && (!self.exclude_read || chat.is_unread())
            && (!self.exclude_archived || !chat.archived)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StickerSource {
    Recent,
    Favorites,
    Set(i64),
}

pub fn sticker_emoji_query(input: &str) -> anyhow::Result<Option<String>> {
    anyhow::ensure!(input.len() <= 512, "Слишком длинный запрос стикеров");
    let query = input.trim();
    if query.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        query.encode_utf16().count() <= 64 && !query.chars().any(char::is_control),
        "Введите короткую последовательность эмодзи без управляющих символов"
    );
    Ok(Some(query.into()))
}

#[derive(Clone, Debug)]
pub struct StickerCollection {
    pub source: StickerSource,
    pub title: String,
    pub installed: Option<bool>,
}

pub struct StickerPackChange {
    pub collection: StickerCollection,
    pub archived: Option<usize>,
}

pub fn sticker_pack_name(input: &str) -> anyhow::Result<String> {
    let input = input.trim();
    let name =
        if input.contains("://") || input.starts_with("t.me/") || input.starts_with("telegram.me/")
        {
            let link = if input.contains("://") {
                input.to_owned()
            } else {
                format!("https://{input}")
            };
            let url = url::Url::parse(&link)?;
            anyhow::ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && url.port().is_none()
                    && url.fragment().is_none(),
                "Введите имя набора или ссылку Telegram"
            );
            if url.scheme() == "tg" && url.host_str() == Some("addstickers") {
                let pairs: Vec<_> = url.query_pairs().collect();
                anyhow::ensure!(
                    url.path().is_empty() && pairs.len() == 1 && pairs[0].0 == "set",
                    "Некорректная ссылка на набор"
                );
                pairs[0].1.to_string()
            } else {
                anyhow::ensure!(
                    url.scheme() == "https"
                        && matches!(url.host_str(), Some("t.me" | "telegram.me"))
                        && url.query().is_none(),
                    "Введите ссылку t.me/addstickers/имя"
                );
                let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
                anyhow::ensure!(
                    parts.len() == 2 && parts[0] == "addstickers",
                    "Введите ссылку t.me/addstickers/имя"
                );
                parts[1].to_owned()
            }
        } else {
            input.to_owned()
        };
    anyhow::ensure!(
        (1..=64).contains(&name.len())
            && name.as_bytes()[0].is_ascii_alphabetic()
            && name
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_'),
        "Имя набора должно содержать до 64 латинских букв, цифр и подчёркиваний и начинаться с буквы"
    );
    Ok(name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StickerKind {
    Static,
    Animated,
    Video,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sticker {
    pub favorite: bool,
    pub id: i64,
    pub emoji: String,
    pub kind: StickerKind,
}

impl Sticker {
    pub fn label(&self) -> String {
        let kind = match self.kind {
            StickerKind::Static => "Стикер",
            StickerKind::Animated => "Анимированный стикер",
            StickerKind::Video => "Видеостикер",
        };
        if self.emoji.is_empty() {
            kind.into()
        } else {
            format!("{} {kind}", self.emoji)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Avatar {
    pub peer: i64,
    pub photo: i64,
    pub dc: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttachmentThumbnail {
    Photo(i64),
    Document(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChatImage {
    Sticker(i64),
    Avatar(Avatar),
    Attachment {
        chat: i64,
        message: i32,
        media: AttachmentThumbnail,
    },
}

impl ChatImage {
    pub fn attachment(chat: i64, message: i32, media: AttachmentThumbnail) -> Self {
        Self::Attachment {
            chat,
            message,
            media,
        }
    }
}

#[derive(Clone)]
pub struct SearchHit {
    pub chat: Chat,
    pub message: Message,
}

pub const SEARCH_PAGE_SIZE: usize = 50;

pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub has_more: bool,
}

pub struct SearchHandle {
    pub requests:
        tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<anyhow::Result<SearchPage>>>,
}

impl SearchHandle {
    pub async fn next_page(&self) -> anyhow::Result<SearchPage> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.requests.send(sender).await?;
        receiver.await?
    }
}

#[derive(Default)]
pub struct SearchBuffer {
    pending: VecDeque<SearchHit>,
    seen: HashSet<(i64, i32)>,
    pub exhausted: bool,
}

impl SearchBuffer {
    pub fn push(&mut self, hit: SearchHit) {
        if self.seen.insert((hit.chat.id, hit.message.id)) {
            self.pending.push_back(hit);
        }
    }

    pub fn needs_more(&self) -> bool {
        !self.exhausted && self.pending.len() <= SEARCH_PAGE_SIZE
    }

    pub fn take_page(&mut self) -> SearchPage {
        SearchPage {
            hits: self
                .pending
                .drain(..self.pending.len().min(SEARCH_PAGE_SIZE))
                .collect(),
            has_more: !self.pending.is_empty() || !self.exhausted,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TextStyle {
    Bold,
    Italic,
    Underline,
    Strike,
    Code,
    Spoiler,
}

pub const SPOILER_PLACEHOLDER: &str = "▰▰▰";

pub fn spoiler_ranges(spans: &[TextSpan]) -> Vec<Range<usize>> {
    let mut ranges: Vec<_> = spans
        .iter()
        .filter(|span| span.style == TextStyle::Spoiler)
        .map(|span| span.range.clone())
        .collect();
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && last.end >= range.start
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

pub fn redact_spoilers<'a>(text: &'a str, spans: &[TextSpan]) -> Cow<'a, str> {
    let ranges = spoiler_ranges(spans);
    if ranges.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut redacted = String::new();
    let mut end = 0;
    for range in ranges {
        redacted.push_str(&text[end..range.start]);
        redacted.push_str(SPOILER_PLACEHOLDER);
        end = range.end;
    }
    redacted.push_str(&text[end..]);
    Cow::Owned(redacted)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextSpan {
    // Byte offsets into the UTF-8 message text.
    pub range: Range<usize>,
    pub style: TextStyle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextLink {
    pub range: Range<usize>,
    pub uri: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormattedText {
    pub text: String,
    pub spans: Vec<TextSpan>,
}

impl FormattedText {
    pub fn trimmed(&self) -> Self {
        let text = self.text.trim();
        let start = self.text.len() - self.text.trim_start().len();
        let end = start + text.len();
        let spans = self
            .spans
            .iter()
            .filter_map(|span| {
                let from = span.range.start.max(start);
                let to = span.range.end.min(end);
                (from < to).then(|| TextSpan {
                    range: from - start..to - start,
                    style: span.style,
                })
            })
            .collect();
        Self {
            text: text.to_owned(),
            spans,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayableMedia {
    Video {
        width: i32,
        height: i32,
        duration: Option<u32>,
        size: u64,
    },
    Audio,
    Voice,
}

impl PlayableMedia {
    pub fn title(self) -> &'static str {
        match self {
            Self::Video { .. } => "Видео",
            Self::Audio => "Аудио",
            Self::Voice => "Голосовое сообщение",
        }
    }

    pub fn open_label(self) -> &'static str {
        match self {
            Self::Video { .. } => "Открыть видео",
            Self::Audio => "Воспроизвести аудио",
            Self::Voice => "Воспроизвести голосовое сообщение",
        }
    }

    pub fn dimensions(self) -> (i32, i32) {
        match self {
            Self::Video { width, height, .. } if width > 0 && height > 0 => (width, height),
            Self::Video { .. } => (640, 360),
            Self::Audio | Self::Voice => (360, 96),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioInfo {
    pub title: Option<String>,
    pub performer: Option<String>,
    pub duration: Option<u32>,
    pub waveform: Vec<u8>,
}

#[derive(Clone, Copy, Default)]
pub struct PlaybackProgress {
    pub received: u64,
    pub total: u64,
}

pub struct PlaybackFile {
    pub file: std::fs::File,
    pub media: PlayableMedia,
}

impl PlaybackFile {
    pub fn path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
    }
}

pub enum PlaybackSource {
    File(PlaybackFile),
    Stream(PlayableMedia, crate::media_stream::StreamSource),
}

#[derive(Clone, Copy, Debug)]
pub struct StickerAnimationTiming {
    pub frames: usize,
    pub fps: f64,
    pub width: usize,
    pub height: usize,
}

pub enum StickerAnimationSource {
    Video(PlaybackSource),
    Tgs(Vec<u8>),
}

impl PlaybackSource {
    pub fn media(&self) -> PlayableMedia {
        match self {
            Self::File(file) => file.media,
            Self::Stream(media, _) => *media,
        }
    }
}

#[derive(Clone)]
pub struct Message {
    pub id: i32,
    pub topic: Option<i32>,
    pub sender: String,
    pub sender_avatar: Option<Avatar>,
    pub self_authored: bool,
    pub timestamp: i64,
    pub text: String,
    pub styles: Vec<TextSpan>,
    pub links: Vec<TextLink>,
    pub outgoing: bool,
    pub attachment: Option<String>,
    pub photo_preview: bool,
    pub playback: Option<PlayableMedia>,
    pub audio: Option<AudioInfo>,
    pub thumbnail: Option<AttachmentThumbnail>,
    pub poll: Option<Poll>,
    pub sticker: Option<Sticker>,
    pub reply_to: Option<i32>,
    pub reply_preview: Option<ReplyPreview>,
    pub can_reply: bool,
    pub can_edit: bool,
    pub can_delete: bool,
    pub can_close_poll: bool,
    pub can_forward: bool,
    pub forwarded: bool,
    pub pinned: bool,
    pub reactions: Vec<Reaction>,
    pub edited: bool,
    pub ephemeral: bool,
    pub album: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Poll {
    pub open_answers: bool,
    pub id: i64,
    pub question: String,
    pub creator: bool,
    pub hide_results_until_close: bool,
    pub choices: Vec<PollChoice>,
    pub closed: bool,
    pub public: bool,
    pub multiple: bool,
    pub quiz: bool,
    pub revoting_disabled: bool,
    pub total: Option<i32>,
    pub solution: Option<String>,
    pub closes_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollChoice {
    pub option: Vec<u8>,
    pub label: String,
    pub votes: Option<i32>,
    pub chosen: bool,
    pub correct: bool,
    pub addition: Option<PollAnswerAddition>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollAnswerAddition {
    pub author: i64,
    pub date: i32,
}

impl Poll {
    pub fn can_list_voters(&self) -> bool {
        self.public
            && (self.creator || self.closed || (!self.hide_results_until_close && self.has_vote()))
            && self.total.is_some_and(|total| total > 0)
    }

    pub fn has_vote(&self) -> bool {
        self.choices.iter().any(|choice| choice.chosen)
    }

    pub fn can_vote(&self) -> bool {
        self.can_vote_at(unix_now())
    }

    pub fn can_vote_at(&self, now: i64) -> bool {
        !self.closed
            && self.closes_at.is_none_or(|deadline| deadline > now)
            && (!self.revoting_disabled || !self.has_vote())
            && self.choices.len() >= 2
    }

    pub fn results_visible(&self) -> bool {
        !self.hide_results_until_close || self.creator || self.closed
    }

    pub fn accepts(&self, options: &[Vec<u8>]) -> bool {
        self.can_vote()
            && (!options.is_empty() || self.has_vote())
            && (self.multiple || options.len() <= 1)
            && options.iter().enumerate().all(|(index, option)| {
                !options[..index].contains(option)
                    && self.choices.iter().any(|choice| &choice.option == option)
            })
    }

    pub fn answers_open(&self) -> bool {
        self.answers_open_at(unix_now())
    }

    fn answers_open_at(&self, now: i64) -> bool {
        self.open_answers
            && !self.quiz
            && !self.closed
            && self.closes_at.is_none_or(|deadline| deadline > now)
    }

    pub fn can_delete_answer(
        &self,
        choice: &PollChoice,
        own_id: Option<i64>,
        now: i32,
        period: i32,
    ) -> bool {
        self.answers_open_at(i64::from(now))
            && self.choices.len() > 2
            && choice.addition.is_some_and(|addition| {
                addition.date > 0
                    && (self.creator
                        || (own_id == Some(addition.author)
                            && (0..period).contains(&now.saturating_sub(addition.date))))
            })
    }
}

pub const POLL_ANSWER_DELETE_PERIOD: i32 = 300;

pub enum PollAnswerChange {
    Add(String),
    Delete(Vec<u8>),
}

pub fn normalized_poll_answer(text: &str) -> anyhow::Result<String> {
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "Введите вариант ответа");
    anyhow::ensure!(
        text.chars().count() <= 100,
        "Вариант должен быть до 100 символов"
    );
    Ok(text.to_owned())
}

pub const POLL_VOTERS_PAGE_SIZE: i32 = 24;

pub struct PollVoter {
    pub peer: i64,
    pub name: String,
    pub avatar: Option<Avatar>,
    pub answers: Vec<String>,
}

pub struct PollVoterPage {
    pub voters: Vec<PollVoter>,
    pub total: i32,
    pub next: Option<String>,
}

#[derive(Clone)]
pub struct PollUpdate {
    pub id: i64,
    pub target: Option<(i64, i32)>,
    pub definition: Option<Poll>,
    pub results: Option<Vec<PollVoteCount>>,
    pub total: Option<i32>,
    pub solution: Option<String>,
    pub partial: bool,
}

#[derive(Clone)]
pub struct PollVoteCount {
    pub option: Vec<u8>,
    pub votes: Option<i32>,
    pub chosen: bool,
    pub correct: bool,
}

impl PollUpdate {
    pub fn apply(&self, chat: i64, message: i32, poll: &mut Poll) -> bool {
        if poll.id != self.id
            || self.target.is_some_and(|target| target != (chat, message))
            || (poll.closed
                && self
                    .definition
                    .as_ref()
                    .is_some_and(|incoming| !incoming.closed))
        {
            return false;
        }
        let mut next = self.definition.clone().unwrap_or_else(|| poll.clone());
        next.closed |= poll.closed;
        next.closes_at = next.closes_at.or(poll.closes_at);
        for choice in &mut next.choices {
            if let Some(old) = poll.choices.iter().find(|old| old.option == choice.option) {
                choice.chosen = old.chosen;
                choice.correct = old.correct;
                choice.votes = old.votes;
            }
            if let Some(results) = &self.results {
                let result = results.iter().find(|result| result.option == choice.option);
                if let Some(count) = result.and_then(|result| result.votes) {
                    choice.votes = Some(count);
                }
                if !self.partial {
                    choice.chosen = result.is_some_and(|result| result.chosen);
                    choice.correct = result.is_some_and(|result| result.correct);
                }
            }
        }
        next.total = self.total.or(poll.total);
        next.solution = self.solution.clone().or_else(|| poll.solution.clone());
        if next == *poll {
            return false;
        }
        *poll = next;
        true
    }
}

pub const MAX_POLL_CHOICES: usize = 12;
pub const POLL_MIN_SECONDS: i32 = 60;
pub const POLL_DEFAULT_MAX_SECONDS: i32 = 2_628_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollDeadline {
    After(i32),
    At(i64),
}

impl PollDeadline {
    pub fn closes_at(self, now: i64) -> i64 {
        match self {
            Self::After(seconds) => now.saturating_add(i64::from(seconds)),
            Self::At(time) => time,
        }
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct PollDraft {
    pub deadline: Option<PollDeadline>,
    pub hide_results_until_close: bool,
    pub question: String,
    pub open_answers: bool,
    pub choices: Vec<String>,
    pub multiple: bool,
    pub revoting_disabled: bool,
    pub public: bool,
    pub quiz: Option<PollQuizDraft>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollQuizDraft {
    pub correct: Vec<usize>,
    pub solution: String,
}

impl PollDraft {
    pub fn close_fields(&self, now: i64, max: i32) -> anyhow::Result<(Option<i32>, Option<i32>)> {
        let Some(deadline) = self.deadline else {
            anyhow::ensure!(
                !self.hide_results_until_close,
                "Для скрытия результатов задайте срок опроса"
            );
            return Ok((None, None));
        };
        let seconds = deadline.closes_at(now).saturating_sub(now);
        anyhow::ensure!(
            seconds >= i64::from(POLL_MIN_SECONDS),
            "До завершения должна оставаться хотя бы минута"
        );
        anyhow::ensure!(
            seconds <= i64::from(max),
            "Срок опроса превышает лимит Telegram ({max} с)"
        );
        match deadline {
            PollDeadline::After(seconds) => Ok((Some(seconds), None)),
            PollDeadline::At(time) => Ok((
                None,
                Some(
                    i32::try_from(time)
                        .map_err(|_| anyhow::anyhow!("Дата завершения вне диапазона Telegram"))?,
                ),
            )),
        }
    }

    pub fn validate(self) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !self.open_answers || (self.public && self.quiz.is_none()),
            "Добавление вариантов доступно в обычном публичном опросе"
        );
        let question = self.question.trim().to_owned();
        anyhow::ensure!(!question.is_empty(), "Введите вопрос");
        anyhow::ensure!(
            question.chars().count() <= 300,
            "Вопрос должен быть до 300 символов"
        );
        let (positions, choices): (Vec<_>, Vec<_>) = self
            .choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                let choice = choice.trim();
                (!choice.is_empty()).then(|| (index, choice.to_owned()))
            })
            .unzip();
        anyhow::ensure!(
            (2..=MAX_POLL_CHOICES).contains(&choices.len()),
            "Добавьте от 2 до {MAX_POLL_CHOICES} вариантов"
        );
        anyhow::ensure!(
            choices.iter().all(|choice| choice.chars().count() <= 100),
            "Вариант должен быть до 100 символов"
        );
        anyhow::ensure!(
            choices
                .iter()
                .enumerate()
                .all(|(index, choice)| !choices[..index].contains(choice)),
            "Варианты должны различаться"
        );
        let mut quiz = self.quiz;
        if let Some(quiz) = &mut quiz {
            anyhow::ensure!(!quiz.correct.is_empty(), "Отметьте правильный ответ");
            anyhow::ensure!(
                self.multiple || quiz.correct.len() == 1,
                "Отметьте только один правильный ответ"
            );
            let correct: Vec<_> = positions
                .iter()
                .enumerate()
                .filter_map(|(index, position)| quiz.correct.contains(position).then_some(index))
                .collect();
            anyhow::ensure!(
                correct.len() == quiz.correct.len(),
                "Отметьте заполненные варианты ответа"
            );
            quiz.correct = correct;
            quiz.solution = quiz.solution.trim().to_owned();
            anyhow::ensure!(
                quiz.solution.chars().count() <= 200 && quiz.solution.lines().count() <= 3,
                "Пояснение должно быть до 200 символов и трёх строк"
            );
        }
        let revoting_disabled = self.revoting_disabled || quiz.is_some();
        Ok(Self {
            question,
            choices,
            quiz,
            revoting_disabled,
            ..self
        })
    }

    pub fn into_poll(self, id: i64) -> Poll {
        Poll {
            open_answers: self.open_answers,
            id,
            question: self.question,
            creator: true,
            hide_results_until_close: false,
            choices: self
                .choices
                .into_iter()
                .enumerate()
                .map(|(index, label)| PollChoice {
                    option: vec![index as u8],
                    label,
                    votes: None,
                    chosen: false,
                    correct: false,
                    addition: None,
                })
                .collect(),
            closed: false,
            public: self.public,
            multiple: self.multiple,
            quiz: self.quiz.is_some(),
            revoting_disabled: self.revoting_disabled || self.quiz.is_some(),
            total: None,
            solution: None,
            closes_at: None,
        }
    }
}

#[derive(Clone)]
pub struct Reaction {
    pub emoji: String,
    pub count: i32,
    pub chosen: bool,
}

pub const COMMON_REACTIONS: &[&str] = &["👍", "❤️", "🔥", "😂", "👏", "😢"];

#[derive(Clone, PartialEq, Eq)]
pub struct Reply {
    pub id: i32,
    pub preview: ReplyPreview,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ReplyPreview {
    pub sender: Option<String>,
    pub text: String,
    pub self_authored: bool,
}

impl Message {
    pub fn open_poll_to_close(&self) -> Option<&Poll> {
        self.poll.as_ref().filter(|poll| {
            self.can_close_poll && !self.forwarded && !self.ephemeral && !poll.closed
        })
    }

    pub fn displayed_sender<'a>(&'a self, own_name: Option<&'a str>) -> &'a str {
        if self.self_authored
            && let Some(name) = own_name
        {
            return name;
        }
        &self.sender
    }

    pub fn formatted_text(&self) -> FormattedText {
        FormattedText {
            text: self.text.clone(),
            spans: self.styles.clone(),
        }
    }

    pub fn reply_preview(&self) -> ReplyPreview {
        let preview = self.preview_text();
        let text = preview.lines().find(|line| !line.trim().is_empty());
        ReplyPreview {
            sender: Some(self.sender.clone()),
            self_authored: self.self_authored,
            text: text
                .or(self.attachment.as_deref())
                .unwrap_or("Сообщение")
                .to_owned(),
        }
    }

    pub fn reply_summary(&self) -> String {
        let preview = self.preview_text();
        let text = preview.lines().find(|line| !line.trim().is_empty());
        match text.or(self.attachment.as_deref()) {
            Some(text) => format!("{} · {text}", self.sender),
            None => self.sender.clone(),
        }
    }

    pub fn preview_text(&self) -> Cow<'_, str> {
        if let Some(sticker) = &self.sticker {
            return Cow::Owned(sticker.label());
        }
        if let Some(poll) = &self.poll {
            let kind = if poll.quiz {
                "Викторина"
            } else {
                "Опрос"
            };
            return Cow::Owned(format!("{kind} · {}", poll.question));
        }
        if self.text.is_empty() && !self.ephemeral {
            match self.playback {
                Some(PlayableMedia::Voice) => return Cow::Borrowed("Голосовое сообщение"),
                Some(PlayableMedia::Audio) => {
                    return Cow::Borrowed(
                        self.audio
                            .as_ref()
                            .and_then(|audio| audio.title.as_deref())
                            .or(self.attachment.as_deref())
                            .unwrap_or("Аудиозапись"),
                    );
                }
                _ => {}
            }
        }
        redact_spoilers(&self.text, &self.styles)
    }
}

impl ReplyPreview {
    pub fn displayed_sender<'a>(&'a self, own_name: Option<&'a str>) -> Option<&'a str> {
        if self.self_authored && own_name.is_some() {
            own_name
        } else {
            self.sender.as_deref()
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct UpdateStatus {
    pub revision: u64,
    pub folders_revision: u64,
    pub reconnecting: bool,
}

#[derive(Clone)]
pub enum ClientUpdateStatus {
    Disabled(&'static str),
    Idle,
    Checking,
    Downloading(u8),
    Current,
    Installed(u64),
    Failed(String),
}

impl ClientUpdateStatus {
    pub fn can_check(&self) -> bool {
        matches!(self, Self::Idle | Self::Current | Self::Failed(_))
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryRevision {
    pub all: u64,
    pub chat: u64,
    pub rewritten: u64,
}

impl HistoryRevision {
    pub fn is_fresh(self, refreshed: Instant, current: Self, now: Instant) -> bool {
        self == current && now.duration_since(refreshed) < CACHE_FRESHNESS
    }

    pub fn can_keep_older(self, previous: Self) -> bool {
        self.all == previous.all && self.rewritten == previous.rewritten
    }
}

#[derive(Default)]
pub struct HistoryCache {
    chats: HashMap<Conversation, CachedHistory>,
}

pub struct CachedHistory {
    pub messages: BTreeMap<i32, Message>,
    pub has_older: bool,
    pub revision: HistoryRevision,
    pub refreshed: Instant,
    used: Instant,
}

impl HistoryCache {
    pub fn open_poll_id(&self, chat: i64, id: i32) -> Option<i64> {
        self.chats
            .iter()
            .filter(|(target, _)| target.chat == chat)
            .find_map(|(_, cached)| {
                cached
                    .messages
                    .get(&id)?
                    .poll
                    .as_ref()
                    .filter(|poll| poll.open_answers)
                    .map(|poll| poll.id)
            })
    }

    pub fn update_polls(&mut self, mut update: impl FnMut(i64, i32, &mut Poll)) {
        for (&chat, history) in &mut self.chats {
            for (&id, message) in &mut history.messages {
                if let Some(poll) = &mut message.poll {
                    update(chat.chat, id, poll);
                }
            }
        }
    }

    pub fn remove(&mut self, chat: i64) {
        self.chats.retain(|target, _| target.chat != chat);
    }

    pub fn remove_conversation(&mut self, target: Conversation) {
        self.chats.remove(&target);
    }

    pub fn restore(
        &mut self,
        chat: impl Into<Conversation>,
        now: Instant,
    ) -> Option<CachedHistory> {
        self.expire(now);
        self.chats.remove(&chat.into())
    }

    pub fn store(
        &mut self,
        chat: impl Into<Conversation>,
        mut messages: BTreeMap<i32, Message>,
        has_older: bool,
        revision: HistoryRevision,
        refreshed: Instant,
        now: Instant,
    ) {
        self.expire(now);
        // Keep a contiguous range. A page with a timer stays in the open chat only.
        let uncached = messages.len().saturating_sub(CACHED_MESSAGES);
        let mut boundary_album = None;
        for _ in 0..uncached {
            boundary_album = messages.pop_first().and_then(|(_, message)| message.album);
        }
        if let Some(album) = boundary_album {
            while messages
                .first_key_value()
                .is_some_and(|(_, message)| message.album == Some(album))
            {
                messages.pop_first();
            }
        }
        if messages.values().any(|message| message.ephemeral) {
            return;
        }
        if self.chats.len() == CACHED_CHATS
            && let Some((&oldest, _)) = self.chats.iter().min_by_key(|(_, history)| history.used)
        {
            self.chats.remove(&oldest);
        }
        self.chats.insert(
            chat.into(),
            CachedHistory {
                messages,
                has_older: has_older || uncached > 0,
                revision,
                refreshed,
                used: now,
            },
        );
    }

    fn expire(&mut self, now: Instant) {
        self.chats
            .retain(|_, history| now.duration_since(history.used) < CACHE_RETENTION);
    }
}

pub struct IncomingMessage {
    pub chat: i64,
    pub origin_channel: Option<i64>,
    pub message: i32,
    pub title: String,
    pub kind: ChatKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UploadKind {
    File,
    Photo,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Upload {
    pub path: PathBuf,
    pub name: String,
    pub kind: UploadKind,
}
/// Replace the refreshed range. Retain older pages only when the ranges meet,
/// and preserve messages delivered by a send that completed during the request.
pub fn merge_latest_history(
    current: &mut BTreeMap<i32, Message>,
    latest: Vec<Message>,
    has_older: bool,
    requested_through: Option<i32>,
) -> bool {
    let oldest = latest.first().map(|message| message.id);
    let newest = latest.last().map(|message| message.id);
    let keep_older = has_older
        && oldest
            .zip(requested_through)
            .is_some_and(|(oldest, previous)| oldest <= previous);
    let older_loaded = keep_older
        && current
            .first_key_value()
            .zip(oldest)
            .is_some_and(|((&first, _), oldest)| first < oldest);

    current.retain(|&id, _| {
        (keep_older && oldest.is_some_and(|oldest| id < oldest))
            || (requested_through.is_none_or(|previous| id > previous)
                && newest.is_none_or(|newest| id > newest))
    });
    current.extend(latest.into_iter().map(|message| (message.id, message)));
    older_loaded
}

pub fn prepend_history_page(
    current: &mut BTreeMap<i32, Message>,
    earlier: Vec<Message>,
    before: i32,
    has_older: bool,
) -> bool {
    let mut advanced = false;
    for message in earlier.into_iter().filter(|message| message.id < before) {
        if let std::collections::btree_map::Entry::Vacant(entry) = current.entry(message.id) {
            entry.insert(message);
            advanced = true;
        }
    }
    has_older && advanced
}

#[cfg(test)]
mod tests {
    #[test]
    fn sticker_emoji_search_preserves_sequences_and_bounds_pasted_input() {
        for query in ["🚀", "👩🏽‍💻", "🇺🇦", "❤️", "#️⃣"] {
            assert_eq!(
                super::sticker_emoji_query(&format!(" {query} "))
                    .unwrap()
                    .as_deref(),
                Some(query)
            );
        }
        assert_eq!(super::sticker_emoji_query(" \n\t").unwrap(), None);
        assert!(super::sticker_emoji_query("🚀\n🐱").is_err());
        assert!(super::sticker_emoji_query("🚀\0").is_err());
        assert!(super::sticker_emoji_query(&"🚀".repeat(33)).is_err());
        assert!(super::sticker_emoji_query(&" ".repeat(513)).is_err());
    }

    use super::*;

    fn chat_addresses() -> ChatAddresses {
        ChatAddresses {
            primary: "demo_primary".into(),
            aliases: vec![
                AttachedAddress {
                    name: "demo_first".into(),
                    active: true,
                    can_toggle: true,
                },
                AttachedAddress {
                    name: "demo_second".into(),
                    active: false,
                    can_toggle: true,
                },
            ],
        }
    }

    #[test]
    fn sticker_pack_links_accept_only_telegram_sets_and_valid_short_names() {
        for input in [
            "  Cat_Set  ",
            "https://t.me/addstickers/Cat_Set",
            "t.me/addstickers/Cat_Set/",
            "https://telegram.me/addstickers/Cat_Set",
            "tg://addstickers?set=Cat_Set",
        ] {
            assert_eq!(sticker_pack_name(input).unwrap(), "Cat_Set");
        }
        for input in [
            "",
            "1cat",
            "Кот",
            "cat/set",
            "https://example.org/addstickers/Cat_Set",
            "https://t.me.evil.test/addstickers/Cat_Set",
            "https://user@t.me/addstickers/Cat_Set",
            "https://t.me/addstickers/cat?other=1",
            "https://t.me/addstickers/cat/more",
            "tg://addstickers?set=cat&set=dog",
            "tg://addstickers?other=cat",
            "tg://addemoji?set=cat",
        ] {
            assert!(sticker_pack_name(input).is_err(), "{input}");
        }
        assert!(sticker_pack_name(&"a".repeat(64)).is_ok());
        assert!(sticker_pack_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn chat_address_edits_reconcile_retries_and_preserve_unmodified_remote_names() {
        let original = chat_addresses();
        let mut proposed = original.clone();
        proposed.primary = "@demo_renamed".into();
        let mut current = original.clone();
        current.aliases[1].active = true;
        let changes = proposed.changes(&original, &current).unwrap();
        assert_eq!(changes, [ChatAddressChange::Primary("demo_renamed".into())]);
        current.apply(changes.into_iter().next().unwrap());
        assert!(current.aliases[1].active);
        assert!(proposed.changes(&original, &current).unwrap().is_empty());
        current.primary = "someone_renamed".into();
        assert!(proposed.changes(&original, &current).is_err());
    }

    #[test]
    fn making_chat_private_refuses_to_leave_an_unseen_address_active() {
        let original = chat_addresses();
        let mut proposed = original.clone();
        proposed.primary.clear();
        proposed.aliases[0].active = false;
        let changes = proposed.changes(&original, &original).unwrap();
        let mut output = original.clone();
        for change in changes {
            output.apply(change);
        }
        assert!(!output.is_public());
        assert_eq!(output.aliases.len(), 2);
        assert!(proposed.changes(&original, &output).unwrap().is_empty());
        let mut current = original.clone();
        current.aliases[1].active = true;
        assert!(proposed.changes(&original, &current).is_err());
    }

    #[test]
    fn exchanging_the_only_active_address_activates_the_replacement_first() {
        let mut original = chat_addresses();
        original.primary.clear();
        let mut proposed = original.clone();
        proposed.aliases[0].active = false;
        proposed.aliases[1].active = true;
        let edits = proposed.changes(&original, &original).unwrap();
        assert_eq!(
            edits,
            [
                ChatAddressChange::Alias(proposed.aliases[1].clone()),
                ChatAddressChange::Alias(proposed.aliases[0].clone()),
            ]
        );
        let mut current = original.clone();
        current.aliases.remove(1);
        assert!(proposed.changes(&original, &current).is_err());
        let mut current = original.clone();
        current.aliases[1].can_toggle = false;
        assert!(proposed.changes(&original, &current).is_err());
    }

    #[test]
    fn chat_creation_validates_type_address_and_unique_contacts() {
        let mut draft = NewChatDraft {
            kind: NewChatKind::Group,
            forum: false,
            info: ChatInfo {
                title: "  План  ".into(),
                about: " Справка ".into(),
            },
            username: String::new(),
            members: Vec::new(),
        };
        assert!(draft.clone().validate().is_err());
        let member = Contact {
            id: 42,
            name: "Пример".into(),
            usernames: Vec::new(),
            phone: None,
            avatar: None,
            kind: ChatKind::Contact,
            is_self: false,
            available: true,
        };
        draft.members = vec![member.clone(), member];
        let group = draft.clone().validate().unwrap();
        assert_eq!(group.info.title, "План");
        assert_eq!(group.info.about, "Справка");
        assert_eq!(group.members.len(), 1);
        draft.forum = true;
        assert!(draft.clone().validate().is_err());
        draft.kind = NewChatKind::Supergroup;
        assert!(
            draft
                .clone()
                .validate()
                .unwrap()
                .chat(-1000000000234, 0)
                .forum
        );
        draft.kind = NewChatKind::Group;
        draft.forum = false;
        draft.username = " @project_1 ".into();
        assert!(draft.clone().validate().is_err());
        draft.kind = NewChatKind::Channel;
        let channel = draft.clone().validate().unwrap();
        assert_eq!(channel.username, "project_1");
        assert!(channel.chat(-1000000000234, 0).can_write);
        draft.forum = true;
        assert!(draft.clone().validate().is_err());
        draft.forum = false;
        draft.members[0].kind = ChatKind::Bot;
        assert!(draft.validate().is_err());
        for invalid in [
            "@",
            "@@project",
            "проект",
            "12345",
            "valid_",
            "test__project",
        ] {
            assert!(chat_username(invalid).is_err());
        }
    }

    #[test]
    fn admin_log_pages_skip_overlap_and_stop_at_zero_or_no_progress() {
        let entry = |id| AdminLogEntry {
            id,
            date: 100,
            actor: AdminLogActor {
                name: "Пример".into(),
                filter_id: None,
                avatar: None,
            },
            action: "Изменено название".into(),
            details: None,
        };
        let page = AdminLogPage::new([40, 35, 48, 39, 35, 0].map(entry).into(), Some(40));
        assert_eq!(
            page.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            [39, 35]
        );
        assert_eq!(page.next, Some(35));
        assert_eq!(AdminLogPage::new(vec![entry(1)], Some(35)).next, None);
        assert_eq!(AdminLogPage::new(vec![entry(40)], Some(40)).next, None);
    }

    #[test]
    fn channel_admin_changes_preserve_locked_rights_and_reject_stale_roles() {
        use AdminRight::*;
        let allowed = AdminRights::from_rights([DeleteMessages, Other]);
        let policy = AdminPolicy::Channel(allowed);
        let original = AdminState::Administrator(AdminRights::from_rights([InviteUsers]));
        let updated = AdminState::Administrator(original.rights().with(DeleteMessages, true));
        let invalid = AdminState::Administrator(AdminRights::from_rights([DeleteMessages]));
        assert!(policy.needs_change(original, original, updated).unwrap());
        assert!(policy.needs_change(original, original, invalid).is_err());
        assert!(
            policy
                .needs_change(original, AdminState::Member, updated)
                .is_err()
        );
        assert!(
            policy
                .needs_change(original, updated, AdminState::Member)
                .is_err()
        );
        let confirmed = AdminState::Administrator(updated.rights().with(Other, true));
        assert!(!policy.needs_change(original, confirmed, updated).unwrap());
    }

    #[test]
    fn administrator_options_follow_the_chat_type_and_basic_group_role() {
        use AdminRight::*;
        let forum = AdminRights::channel(false, true, false);
        let broadcast = AdminRights::channel(true, false, true);
        assert!(forum.has(ManageTopics) && forum.has(PinMessages) && forum.has(Anonymous));
        assert!(!forum.has(PostMessages) && !forum.has(ManageDirectMessages));
        assert!(broadcast.has(PostMessages) && broadcast.has(ManageDirectMessages));
        assert!(!broadcast.has(Anonymous) && !broadcast.has(ManageTopics));
        assert!(!AdminRights::channel(false, false, false).has(ManageTopics));
        let policy = AdminPolicy::Basic;
        let original = AdminState::Member;
        let admin = AdminState::Administrator(AdminRights::default());
        assert!(admin.channel_rights().has(Other));
        assert_eq!(original.channel_rights(), AdminRights::default());
        assert!(policy.needs_change(original, original, admin).unwrap());
        assert!(!policy.needs_change(original, admin, admin).unwrap());
        assert!(policy.needs_change(admin, admin, original).unwrap());
        assert!(
            policy
                .needs_change(original, original, AdminState::Administrator(forum))
                .is_err()
        );
    }

    #[test]
    fn invite_draft_validates_limits_and_calculates_expiration_at_send_time() {
        let draft = InviteDraft {
            title: "  Экскурсия  ".into(),
            hours: Some(24),
            limit: Some(3),
            approval: false,
        }
        .normalize()
        .unwrap();
        assert_eq!(draft.title, "Экскурсия");
        assert_eq!(
            draft.expiration(1_700_000_000).unwrap(),
            Some(1_700_086_400)
        );
        assert!(draft.expiration(i32::MAX as u64).is_err());
        assert!(
            InviteDraft {
                title: String::new(),
                hours: None,
                limit: Some(0),
                approval: false,
            }
            .normalize()
            .is_err()
        );
    }

    #[test]
    fn approval_invites_require_no_usage_limit() {
        let draft = |limit| InviteDraft {
            title: String::new(),
            hours: Some(24),
            limit,
            approval: true,
        };
        assert!(draft(None).normalize().unwrap().approval);
        assert!(draft(Some(3)).normalize().is_err());
    }

    fn example_invite() -> InviteLink {
        InviteLink {
            cursor: InviteCursor {
                date: 100,
                link: "https://example.invalid/+invite".into(),
            },
            title: "Команда".into(),
            permanent: false,
            revoked: false,
            expires: Some(200),
            limit: Some(5),
            uses: 2,
            approval: false,
            pending: 0,
            paid: false,
        }
    }

    #[test]
    fn invite_edit_changes_only_the_selected_fields_and_preserves_a_concurrent_change() {
        let old = example_invite();
        let mut current = old.clone();
        current.expires = Some(300);
        current.approval = true;
        current.limit = None;
        let edit = InviteEdit {
            title: "  Новое название ".into(),
            expires: old.expires,
            limit: old.limit,
            approval: old.approval,
        };
        let changes = edit.changes(&old, &current).unwrap();
        assert_eq!(
            changes,
            InviteChanges {
                title: Some("Новое название".into()),
                ..InviteChanges::default()
            }
        );
        current.title = "Одновременная правка".into();
        assert!(edit.changes(&old, &current).is_err());
    }

    #[test]
    fn invite_edit_clears_limits_and_expiration_and_accepts_a_safe_retry() {
        let old = example_invite();
        let edit = InviteEdit {
            title: String::new(),
            expires: None,
            limit: None,
            approval: true,
        };
        let changes = edit.changes(&old, &old).unwrap();
        assert_eq!(
            changes,
            InviteChanges {
                title: Some(String::new()),
                expire_date: Some(0),
                usage_limit: Some(0),
                request_needed: Some(true),
            }
        );
        let mut updated = old.clone();
        assert!(!changes.confirmed_by(&updated));
        updated.title.clear();
        updated.expires = None;
        updated.limit = None;
        updated.approval = true;
        assert!(changes.confirmed_by(&updated));
        assert_eq!(
            edit.changes(&old, &updated).unwrap(),
            InviteChanges::default()
        );
    }

    #[test]
    fn invite_edit_refuses_revoked_primary_and_subscription_parameter_changes() {
        let mut link = example_invite();
        let mut edit = InviteEdit {
            title: "Другое название".into(),
            expires: link.expires,
            limit: link.limit,
            approval: link.approval,
        };
        link.paid = true;
        assert!(edit.changes(&link, &link).is_ok());
        edit.expires = None;
        assert!(edit.changes(&link, &link).is_err());
        link.paid = false;
        link.revoked = true;
        assert!(edit.changes(&link, &link).is_err());
        link.revoked = false;
        link.permanent = true;
        assert!(edit.changes(&link, &link).is_err());
    }

    #[test]
    fn chat_info_retry_skips_a_confirmed_title_and_preserves_unedited_fields() {
        let original = ChatInfo {
            title: "Старое".into(),
            about: "Первое".into(),
        };
        let proposed = ChatInfo {
            title: " Новое ".into(),
            about: "Второе".into(),
        };
        let partial = ChatInfo {
            title: "Новое".into(),
            about: "Первое".into(),
        };
        assert_eq!(
            proposed.changes(&original, &partial).unwrap(),
            (None, Some("Второе".into()))
        );
        let conflicting = ChatInfo {
            about: "Третье".into(),
            ..partial
        };
        assert!(proposed.changes(&original, &conflicting).is_err());
        let rename_only = ChatInfo {
            about: original.about.clone(),
            ..proposed
        };
        assert_eq!(
            rename_only.changes(&original, &conflicting).unwrap(),
            (None, None)
        );
    }

    #[test]
    fn confirmed_device_changes_preserve_current_identity_and_change_only_acknowledged_fields() {
        let mut snapshot = crate::demo::device_sessions();
        let ids: Vec<_> = snapshot.sessions.iter().map(|session| session.id).collect();
        snapshot.apply_confirmed(DeviceChange::SetTtl(30));
        assert_eq!(snapshot.inactive_ttl_days, 30);
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            ids
        );
        snapshot.apply_confirmed(DeviceChange::Terminate(2));
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![0, -3]
        );
        snapshot.apply_confirmed(DeviceChange::TerminateOthers);
        assert_eq!(snapshot.sessions.len(), 1);
        assert!(snapshot.sessions[0].current);
        assert_eq!(snapshot.sessions[0].id, 0);
        assert_eq!(snapshot.inactive_ttl_days, 30);
        snapshot.apply_confirmed(DeviceChange::TerminateOthers);
        snapshot.apply_confirmed(DeviceChange::Terminate(0));
        assert_eq!(snapshot.sessions.len(), 1);
        assert!(snapshot.sessions[0].current);
    }

    #[test]
    fn manual_unread_mark_filters_as_unread_without_faking_a_message_count() {
        let mut chat = crate::demo::chats().remove(0);
        chat.unread = 0;
        chat.unread_mark = true;
        let folder = ChatFolder {
            kinds: vec![chat.kind],
            exclude_read: true,
            ..ChatFolder::default()
        };
        assert!(chat.is_unread());
        assert_eq!(chat.unread, 0);
        assert!(folder.contains(&chat));
        chat.unread_mark = false;
        assert!(!chat.is_unread());
        assert!(!folder.contains(&chat));
        chat.unread = 3;
        assert!(folder.contains(&chat));
    }

    #[test]
    fn reading_an_older_snapshot_does_not_clear_a_newer_message_count() {
        let mut chat = crate::demo::chats().remove(0);
        chat.last_message_id = 100;
        chat.unread = 4;
        chat.unread_mark = true;
        chat.read_through(90);
        assert!(!chat.unread_mark);
        assert_eq!(chat.unread, 4);
        chat.read_through(0);
        assert_eq!(chat.unread, 4);
        chat.read_through(100);
        assert_eq!(chat.unread, 0);
        assert!(!chat.is_unread());
    }

    #[test]
    fn chat_order_keeps_server_pin_order_and_uses_separate_folder_pins() {
        let base = crate::demo::chats().remove(0);
        let chats: Vec<_> = [(1, true, 10), (2, false, 30), (3, true, 20), (4, false, 40)]
            .into_iter()
            .map(|(id, pinned, last_activity)| Chat {
                id,
                pinned,
                last_activity,
                ..base.clone()
            })
            .collect();
        let mut visible: Vec<_> = chats.iter().collect();
        order_chats(&mut visible, None);
        assert_eq!(
            visible.iter().map(|chat| chat.id).collect::<Vec<_>>(),
            [1, 3, 4, 2]
        );
        let folder = ChatFolder {
            pinned: vec![2, 1],
            ..ChatFolder::default()
        };
        order_chats(&mut visible, Some(&folder));
        assert_eq!(
            visible.iter().map(|chat| chat.id).collect::<Vec<_>>(),
            [2, 1, 4, 3]
        );
        let folder = ChatFolder::default();
        order_chats(&mut visible, Some(&folder));
        assert_eq!(
            visible.iter().map(|chat| chat.id).collect::<Vec<_>>(),
            [4, 2, 3, 1]
        );
    }

    #[test]
    fn contact_search_matches_names_aliases_and_formatted_phone_numbers() {
        let mut contact = crate::demo::contacts().remove(0);
        for query in [
            "",
            "СОКОЛОВА Анна",
            "anna",
            "@DEMO_ANNA",
            "Анна @demo_anna",
            "+000 (000) 000-10",
        ] {
            assert!(contact.matches(query), "{query}");
        }
        for query in ["Ольга", "++++", "55555"] {
            assert!(!contact.matches(query), "{query}");
        }
        contact.phone = None;
        assert!(!contact.matches("00010"));
    }

    #[test]
    fn contact_chat_preserves_identity_and_recognizes_saved_messages() {
        let mut contact = crate::demo::contacts().remove(0);
        let chat = contact.chat();
        assert_eq!(chat.id, contact.id);
        assert_eq!(chat.title, contact.name);
        assert!(!chat.saved_messages);
        assert!(!chat.group);
        contact.is_self = true;
        let saved = contact.chat();
        assert!(saved.saved_messages);
        assert_eq!(saved.title, "Избранное");
        assert_eq!(saved.id, contact.id);
    }

    #[test]
    fn search_pages_keep_cross_chat_ids_and_skip_repeated_results() {
        let chat = crate::demo::chats().remove(0);
        let hit = |chat_id, message_id| SearchHit {
            chat: Chat {
                id: chat_id,
                ..chat.clone()
            },
            message: messages(&[message_id]).remove(0),
        };
        let mut buffer = SearchBuffer::default();
        for id in 1..=SEARCH_PAGE_SIZE as i32 {
            buffer.push(hit(1, id));
        }
        buffer.push(hit(1, 1));
        assert!(buffer.needs_more());
        buffer.push(hit(2, SEARCH_PAGE_SIZE as i32));
        assert!(!buffer.needs_more());
        let first = buffer.take_page();
        assert_eq!(first.hits.len(), SEARCH_PAGE_SIZE);
        assert!(first.has_more);
        // A repeat of a previously delivered result must not reappear on the next page.
        buffer.push(hit(1, 1));
        buffer.push(hit(2, SEARCH_PAGE_SIZE as i32));
        buffer.exhausted = true;
        let last = buffer.take_page();
        assert_eq!(last.hits.len(), 1);
        assert_eq!(last.hits[0].chat.id, 2);
        assert!(!last.has_more);
    }

    #[test]
    fn exact_search_page_does_not_offer_an_empty_next_page() {
        let chat = crate::demo::chats().remove(0);
        let mut buffer = SearchBuffer::default();
        for message in messages(&(1..=SEARCH_PAGE_SIZE as i32).collect::<Vec<_>>()) {
            buffer.push(SearchHit {
                chat: chat.clone(),
                message,
            });
        }
        buffer.exhausted = true;
        let page = buffer.take_page();
        assert_eq!(page.hits.len(), SEARCH_PAGE_SIZE);
        assert!(!page.has_more);
    }

    #[test]
    fn explicit_folder_members_override_categories_and_archive_filter() {
        let chat = Chat {
            id: 1,
            pinned: false,
            last_activity: 0,
            title: "Тест".into(),
            preview: String::new(),
            preview_sender: None,
            preview_image: None,
            group: true,
            forum: false,
            unread: 0,
            unread_mark: false,
            last_message_id: 0,
            broadcast: false,
            can_write: true,
            slow_mode_enabled: false,
            saved_messages: false,
            archived: true,
            kind: ChatKind::Group,
            muted: false,
            avatar: None,
        };
        let mut folder = ChatFolder {
            kinds: vec![ChatKind::Group],
            exclude_archived: true,
            ..ChatFolder::default()
        };
        assert!(!folder.contains(&chat));
        folder.pinned.push(chat.id);
        assert!(folder.contains(&chat));
        folder.excluded.push(chat.id);
        assert!(!folder.contains(&chat));
    }

    fn messages(ids: &[i32]) -> Vec<Message> {
        ids.iter()
            .map(|&id| Message {
                id,
                topic: None,
                sender: "Тест".into(),
                sender_avatar: None,
                self_authored: false,
                timestamp: 0,
                text: String::new(),
                styles: Vec::new(),
                links: Vec::new(),
                outgoing: false,
                attachment: None,
                photo_preview: false,
                playback: None,
                audio: None,
                thumbnail: None,
                poll: None,
                sticker: None,
                reply_to: None,
                reply_preview: None,
                can_reply: true,
                can_edit: false,
                can_delete: false,
                can_close_poll: false,
                can_forward: false,
                forwarded: false,
                pinned: false,
                reactions: Vec::new(),
                edited: false,
                ephemeral: false,
                album: None,
            })
            .collect()
    }

    fn history(ids: &[i32]) -> BTreeMap<i32, Message> {
        messages(ids).into_iter().map(|msg| (msg.id, msg)).collect()
    }

    #[test]
    fn forum_topics_use_separate_cached_histories() {
        let first = Conversation {
            chat: 7,
            topic: Some(100),
        };
        let second = Conversation {
            chat: 7,
            topic: Some(200),
        };
        let now = Instant::now();
        let mut cache = HistoryCache::default();
        for (target, id) in [(first, 110), (second, 210)] {
            let page = messages(&[id])
                .into_iter()
                .map(|message| (message.id, message))
                .collect();
            cache.store(target, page, false, HistoryRevision::default(), now, now);
        }
        assert!(cache.restore(7, now).is_none());
        assert!(
            cache
                .restore(second, now)
                .unwrap()
                .messages
                .contains_key(&210)
        );
        assert!(
            cache
                .restore(first, now)
                .unwrap()
                .messages
                .contains_key(&110)
        );
    }

    #[test]
    fn topic_authorship_and_admin_permissions_grant_distinct_actions() {
        use ForumTopicChange::*;

        let mut topic = ForumTopic {
            id: 100,
            title: "Тема".into(),
            unread: 0,
            closed: false,
            hidden: false,
            pinned: false,
            mine: true,
        };
        let member = ForumPermissions::default();
        assert!(member.allows(&topic, &Rename("Новое название".into())));
        assert!(member.allows(&topic, &Closed(true)));
        assert!(member.allows(&topic, &Delete));
        assert!(!member.allows(&topic, &Pinned(true)));
        topic.mine = false;
        assert!(!member.allows(&topic, &Closed(true)));
        assert!(!member.allows(&topic, &Delete));
        let mut admin = ForumPermissions {
            manage: true,
            ..member
        };
        assert!(admin.allows(&topic, &Pinned(true)));
        assert!(!admin.allows(&topic, &Hidden(true)));
        assert!(!admin.allows(&topic, &Delete));
        admin.delete_messages = true;
        assert!(admin.allows(&topic, &Delete));
        topic.id = GENERAL_TOPIC;
        assert!(admin.allows(&topic, &Hidden(true)));
        assert!(!admin.allows(&topic, &Delete));
    }

    #[test]
    fn enabling_topics_rechecks_linked_discussions_and_history_consent() {
        let disabled = ForumMode {
            enabled: false,
            linked_discussion: false,
            hidden_history: false,
        };
        let enabled = ForumMode {
            enabled: true,
            ..disabled
        };
        let hidden = ForumMode {
            hidden_history: true,
            ..disabled
        };
        let linked = ForumMode {
            linked_discussion: true,
            ..disabled
        };
        assert!(disabled.needs_change(disabled).unwrap());
        assert!(!enabled.needs_change(disabled).unwrap());
        assert!(hidden.needs_change(disabled).is_err());
        assert!(hidden.needs_change(hidden).unwrap());
        assert!(disabled.needs_change(hidden).unwrap());
        assert!(linked.needs_change(disabled).is_err());
        assert!(enabled.needs_change(enabled).unwrap());
        assert!(!disabled.needs_change(enabled).unwrap());
    }

    #[test]
    fn forum_topic_changes_keep_concurrent_edits_and_validate_utf8() {
        use ForumTopicChange::*;

        let original = ForumTopic {
            id: 200,
            title: "Прежнее".into(),
            unread: 2,
            closed: false,
            hidden: false,
            pinned: false,
            mine: true,
        };
        let mut current = original.clone();
        let rename = Rename(" Новое ".into());
        current.unread = 3;
        assert!(rename.needs_write(&original, &current).unwrap());
        current.title = "Новое".into();
        assert!(!rename.needs_write(&original, &current).unwrap());
        assert!(Delete.needs_write(&original, &current).is_err());
        current.title = "Параллельное".into();
        assert!(rename.needs_write(&original, &current).is_err());
        assert!(Pinned(true).needs_write(&original, &current).unwrap());
        assert!(Hidden(true).needs_write(&original, &current).is_err());
        current.id = 201;
        assert!(Pinned(true).needs_write(&original, &current).is_err());

        let limit = "я".repeat(64);
        assert_eq!(forum_topic_title(&format!(" {limit} ")).unwrap(), limit);
        assert!(forum_topic_title(&format!("{limit}я")).is_err());
        assert!(forum_topic_title("а\nб").is_err());
        assert!(forum_topic_title(" ").is_err());
    }

    #[test]
    fn cached_history_preserves_the_latest_contiguous_range_and_expires() {
        let now = Instant::now();
        let revision = HistoryRevision::default();
        let mut cache = HistoryCache::default();
        let ids: Vec<_> = (1..=450).collect();
        cache.store(1, history(&ids), false, revision, now, now);
        let cached = cache.restore(1, now + Duration::from_hours(5)).unwrap();
        assert_eq!(cached.messages.len(), CACHED_MESSAGES);
        assert_eq!(cached.messages.first_key_value().unwrap().0, &51);
        assert!(cached.has_older);
        assert!(!cached.revision.is_fresh(
            cached.refreshed,
            revision,
            now + Duration::from_hours(1)
        ));
        cache.store(1, cached.messages, true, revision, now, now);
        assert!(cache.restore(1, now + CACHE_RETENTION).is_none());
    }

    #[test]
    fn poll_draft_validates_visible_choices_and_maps_distinct_options() {
        let draft = PollDraft {
            question: "  Когда? ".into(),
            choices: vec!["Сейчас".into(), " ".into(), "Завтра ".into()],
            multiple: true,
            ..PollDraft::default()
        };
        let mut invalid = draft.clone();
        invalid.choices.push(" Завтра".into());
        assert!(invalid.validate().is_err());
        let poll = draft.validate().unwrap().into_poll(20);
        assert_eq!(poll.question, "Когда?");
        assert_eq!(
            (&*poll.choices[1].label, &*poll.choices[1].option),
            ("Завтра", &[1][..])
        );
        assert!(poll.accepts(&[vec![0], vec![1]]));
        assert!(!poll.accepts(&[vec![0], vec![0]]));
        assert!(!poll.accepts(&[vec![2]]));
        assert!(!poll.accepts(&[]));
    }

    #[test]
    fn quiz_draft_maps_answers_after_empty_rows_and_hides_the_key_before_voting() {
        let draft = PollDraft {
            question: "Какие дни выходные?".into(),
            choices: vec![
                " ".into(),
                "Суббота".into(),
                "Пятница".into(),
                String::new(),
                "Воскресенье".into(),
            ],
            multiple: true,
            quiz: Some(PollQuizDraft {
                correct: vec![4, 1],
                solution: "  Суббота\nи воскресенье.  ".into(),
            }),
            ..PollDraft::default()
        };
        let normalized = draft.clone().validate().unwrap();
        assert!(normalized == normalized.clone().validate().unwrap());
        assert_eq!(
            normalized.quiz,
            Some(PollQuizDraft {
                correct: vec![0, 2],
                solution: "Суббота\nи воскресенье.".into(),
            })
        );
        let poll = normalized.into_poll(46);
        assert!(poll.quiz && poll.multiple && poll.revoting_disabled);
        assert!(poll.choices.iter().all(|choice| !choice.correct));
        assert_eq!(poll.solution, None);

        for correct in [vec![], vec![0], vec![4, 4], vec![5]] {
            let mut invalid = draft.clone();
            invalid.quiz.as_mut().unwrap().correct = correct;
            assert!(invalid.validate().is_err());
        }
        let mut single = draft.clone();
        single.multiple = false;
        assert!(single.validate().is_err());

        for solution in ["а".repeat(201), "Раз\nДва\nТри\nЧетыре".into()] {
            let mut invalid = draft.clone();
            invalid.quiz.as_mut().unwrap().solution = solution;
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn poll_partial_updates_preserve_vote_and_explicit_targets_do_not_leak() {
        let mut poll = PollDraft {
            question: "Когда?".into(),
            choices: vec!["Сейчас".into(), "Завтра".into()],
            ..PollDraft::default()
        }
        .into_poll(26);
        poll.choices[1].chosen = true;
        poll.choices[1].votes = Some(1);
        let mut update = PollUpdate {
            id: 26,
            target: Some((-100, 10)),
            definition: None,
            results: Some(vec![PollVoteCount {
                option: vec![1],
                votes: Some(2),
                chosen: false,
                correct: false,
            }]),
            total: Some(3),
            solution: None,
            partial: true,
        };
        assert!(!update.apply(-101, 10, &mut poll));
        assert!(!update.apply(-100, 11, &mut poll));
        assert!(update.apply(-100, 10, &mut poll));
        assert!(poll.choices[1].chosen);
        assert_eq!(poll.choices[1].votes, Some(2));
        update.partial = false;
        assert!(update.apply(-100, 10, &mut poll));
        assert!(!poll.has_vote());
        let mut closed = poll.clone();
        closed.closed = true;
        update.definition = Some(closed);
        update.total = None;
        update.results = None;
        assert!(update.apply(-100, 10, &mut poll));
        assert_eq!(poll.total, Some(3));
        assert!(!poll.can_vote());
    }

    #[test]
    fn voter_names_require_visible_public_results() {
        let mut poll = PollDraft {
            question: "Когда?".into(),
            choices: vec!["Сейчас".into(), "Завтра".into()],
            public: true,
            ..PollDraft::default()
        }
        .into_poll(72);
        assert!(!poll.can_list_voters());
        poll.total = Some(3);
        assert!(poll.can_list_voters());
        poll.creator = false;
        assert!(!poll.can_list_voters());
        poll.choices[0].chosen = true;
        assert!(poll.can_list_voters());
        poll.hide_results_until_close = true;
        assert!(!poll.can_list_voters());
        poll.closed = true;
        assert!(poll.can_list_voters());
        poll.public = false;
        assert!(!poll.can_list_voters());
    }

    #[test]
    fn cache_evicts_oldest_chat_and_excludes_timed_messages() {
        let now = Instant::now();
        let revision = HistoryRevision::default();
        let mut cache = HistoryCache::default();
        for chat in 0..=CACHED_CHATS {
            let used = now + Duration::from_secs(chat as u64);
            cache.store(chat as i64, history(&[1]), false, revision, now, used);
        }
        let now = now + Duration::from_mins(5);
        assert!(cache.restore(0, now).is_none());
        assert!(cache.restore(1, now).is_some());
        let mut timed = history(&[1, 2, 3]);
        timed.get_mut(&2).unwrap().ephemeral = true;
        cache.store(1, timed, false, revision, now, now);
        assert!(cache.restore(1, now).is_none());
    }

    #[test]
    fn overlapping_refresh_keeps_older_pages_and_drops_deleted_messages() {
        let mut current = history(&[1, 4, 6, 8, 9]);
        assert!(merge_latest_history(
            &mut current,
            messages(&[4, 8, 10]),
            true,
            Some(9),
        ));
        assert_eq!(current.into_keys().collect::<Vec<_>>(), [1, 4, 8, 10]);
    }

    #[test]
    fn non_overlapping_refresh_restarts_paging_without_a_gap() {
        let mut current = history(&[2, 4, 7]);
        assert!(!merge_latest_history(
            &mut current,
            messages(&[50, 51]),
            true,
            Some(7),
        ));
        assert_eq!(current.into_keys().collect::<Vec<_>>(), [50, 51]);
    }

    #[test]
    fn ordinary_poll_can_lock_the_first_vote_without_becoming_a_quiz() {
        let mut poll = PollDraft {
            question: "Когда?".into(),
            choices: vec!["Сейчас".into(), "Завтра".into()],
            revoting_disabled: true,
            ..PollDraft::default()
        }
        .validate()
        .unwrap()
        .into_poll(47);
        assert!(!poll.quiz && poll.revoting_disabled);
        assert!(poll.accepts(&[vec![0]]));
        poll.choices[0].chosen = true;
        assert!(!poll.can_vote());
        assert!(!poll.accepts(&[vec![1]]));
        assert!(!poll.accepts(&[]));
    }

    #[test]
    fn open_poll_requires_public_voting_and_limits_answer_text() {
        let mut draft = PollDraft {
            question: "Когда?".into(),
            choices: vec!["Сейчас".into(), "Завтра".into()],
            open_answers: true,
            ..PollDraft::default()
        };
        assert!(draft.clone().validate().is_err());
        draft.public = true;
        let mut poll = draft.clone().validate().unwrap().into_poll(17);
        assert!(poll.answers_open());
        poll.closes_at = Some(1_800_000_000);
        assert!(poll.answers_open_at(1_799_999_999));
        assert!(!poll.answers_open_at(1_800_000_000));
        draft.quiz = Some(PollQuizDraft {
            correct: vec![0],
            solution: String::new(),
        });
        assert!(draft.validate().is_err());
        let text = "я".repeat(100);
        assert_eq!(
            normalized_poll_answer(&format!("  {text}\n")).unwrap(),
            text
        );
        assert!(normalized_poll_answer(&format!("{text}я")).is_err());
        assert!(normalized_poll_answer(" \n ").is_err());
    }

    #[test]
    fn added_poll_answer_can_only_be_deleted_by_creator_or_recent_author() {
        let mut poll = PollDraft {
            question: "Когда?".into(),
            choices: vec!["Сейчас".into(), "Завтра".into(), "В пятницу".into()],
            public: true,
            open_answers: true,
            ..PollDraft::default()
        }
        .into_poll(17);
        poll.creator = false;
        poll.choices[2].addition = Some(PollAnswerAddition {
            author: 42,
            date: 100,
        });
        let period = POLL_ANSWER_DELETE_PERIOD;
        let added = poll.choices[2].clone();
        assert!(poll.can_delete_answer(&added, Some(42), 100 + period - 1, period));
        assert!(!poll.can_delete_answer(&added, Some(47), 100, period));
        assert!(!poll.can_delete_answer(&added, Some(42), 100 + period, period));
        poll.creator = true;
        assert!(poll.can_delete_answer(&added, None, 100 + period, period));
        assert!(!poll.can_delete_answer(&poll.choices[0], Some(42), 100, period));
        poll.closed = true;
        assert!(!poll.can_delete_answer(&added, Some(42), 100, period));
        poll.closed = false;
        poll.choices.remove(0);
        assert!(!poll.can_delete_answer(&added, Some(42), 100, period));
    }

    #[test]
    fn open_poll_definition_updates_keep_votes_with_the_same_option_id() {
        let mut poll = PollDraft {
            question: "Когда?".into(),
            choices: vec!["Сейчас".into(), "Завтра".into(), "В пятницу".into()],
            public: true,
            open_answers: true,
            ..PollDraft::default()
        }
        .into_poll(17);
        poll.choices[1].chosen = true;
        poll.choices[1].votes = Some(3);
        poll.choices[2].addition = Some(PollAnswerAddition {
            author: 42,
            date: 100,
        });
        let mut definition = poll.clone();
        definition.choices.remove(2);
        definition.choices.reverse();
        definition.choices[0].chosen = false;
        definition.choices[0].votes = None;
        let update = PollUpdate {
            id: poll.id,
            target: Some((90, 10)),
            definition: Some(definition),
            results: None,
            total: Some(4),
            solution: None,
            partial: true,
        };
        assert!(!update.apply(90, 11, &mut poll));
        assert!(update.apply(90, 10, &mut poll));
        assert_eq!(poll.choices.len(), 2);
        assert_eq!(poll.choices[0].label, "Завтра");
        assert!(poll.choices[0].chosen);
        assert_eq!(poll.choices[0].votes, Some(3));
        assert_eq!(poll.total, Some(4));
    }

    #[test]
    fn older_page_advances_only_when_new_messages_precede_the_cursor() {
        let mut current = history(&[12, 15, 16]);
        assert!(prepend_history_page(
            &mut current,
            messages(&[5, 6, 6, 12, 16]),
            12,
            true,
        ));
        assert!(!prepend_history_page(
            &mut current,
            messages(&[5, 6, 12]),
            5,
            true,
        ));
        assert_eq!(current.into_keys().collect::<Vec<_>>(), [5, 6, 12, 15, 16]);
    }

    #[test]
    fn full_snapshot_keeps_only_messages_sent_during_refresh() {
        let mut current = history(&[1, 2, 3, 4]);
        assert!(!merge_latest_history(
            &mut current,
            messages(&[1, 2]),
            false,
            Some(3),
        ));
        assert_eq!(current.into_keys().collect::<Vec<_>>(), [1, 2, 4]);
    }

    #[test]
    fn timed_poll_encodes_one_deadline_and_uses_the_current_server_limit() {
        let now = 1_800_000_000;
        let mut draft = PollDraft {
            deadline: Some(PollDeadline::After(60)),
            ..PollDraft::default()
        };
        assert_eq!(draft.close_fields(now, 120).unwrap(), (Some(60), None));
        draft.deadline = Some(PollDeadline::At(now + 120));
        assert_eq!(
            draft.close_fields(now, 120).unwrap(),
            (None, Some(1_800_000_120))
        );
        assert!(draft.close_fields(now, 119).is_err());
        assert!(draft.close_fields(now + 61, 120).is_err());
        draft.deadline = None;
        draft.hide_results_until_close = true;
        assert!(draft.close_fields(now, 120).is_err());
    }

    #[test]
    fn elapsed_deadline_stops_voting_and_waits_for_server_confirmation_to_show_results() {
        let mut poll = PollDraft {
            question: "Когда?".into(),
            choices: vec!["Сейчас".into(), "Завтра".into()],
            ..PollDraft::default()
        }
        .into_poll(7);
        poll.creator = false;
        poll.closes_at = Some(1_800_000_000);
        poll.hide_results_until_close = true;
        assert!(poll.can_vote_at(1_799_999_999));
        assert!(!poll.can_vote_at(1_800_000_000));
        assert!(!poll.results_visible());
        let mut confirmed = poll.clone();
        confirmed.closes_at = None;
        confirmed.closed = true;
        assert!(
            PollUpdate {
                id: 7,
                target: Some((3, 4)),
                definition: Some(confirmed),
                results: None,
                total: Some(4),
                solution: None,
                partial: false,
            }
            .apply(3, 4, &mut poll)
        );
        assert_eq!(poll.closes_at, Some(1_800_000_000));
        assert!(poll.results_visible());
        assert!(!poll.can_vote_at(1_799_999_999));
    }
}
