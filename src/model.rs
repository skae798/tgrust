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
    pub unread: i32,
    pub unread_mark: bool,
    pub last_message_id: i32,
    pub broadcast: bool,
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
            unread: 0,
            unread_mark: false,
            last_message_id: 0,
            broadcast: false,
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
    pub bot_commands: Vec<BotCommand>,
}

pub const INVITE_PAGE_SIZE: i32 = 25;

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
    Banned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberRole {
    Owner,
    Administrator,
    Member,
    Banned,
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
            (Self::Ban, MemberRole::Member) => Some(MemberChange::Ban),
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
            Self::All => member.role != MemberRole::Banned,
            Self::Admins => matches!(member.role, MemberRole::Owner | MemberRole::Administrator),
            Self::Bots => member.bot && member.role != MemberRole::Banned,
            Self::Banned => member.role == MemberRole::Banned,
        }
    }
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
    pub reply_to: Option<i32>,
    pub reply_preview: Option<ReplyPreview>,
    pub can_reply: bool,
    pub can_edit: bool,
    pub can_delete: bool,
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
    pub id: i64,
    pub question: String,
    pub choices: Vec<PollChoice>,
    pub closed: bool,
    pub public: bool,
    pub multiple: bool,
    pub quiz: bool,
    pub revoting_disabled: bool,
    pub total: Option<i32>,
    pub solution: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollChoice {
    pub option: Vec<u8>,
    pub label: String,
    pub votes: Option<i32>,
    pub chosen: bool,
    pub correct: bool,
}

impl Poll {
    pub fn has_vote(&self) -> bool {
        self.choices.iter().any(|choice| choice.chosen)
    }

    pub fn can_vote(&self) -> bool {
        !self.closed && (!self.revoting_disabled || !self.has_vote()) && self.choices.len() >= 2
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
        if poll.id != self.id || self.target.is_some_and(|target| target != (chat, message)) {
            return false;
        }
        let mut next = self.definition.clone().unwrap_or_else(|| poll.clone());
        next.closed |= poll.closed;
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

#[derive(Clone, Default, PartialEq, Eq)]
pub struct PollDraft {
    pub question: String,
    pub choices: Vec<String>,
    pub multiple: bool,
    pub public: bool,
}

impl PollDraft {
    pub fn validate(self) -> anyhow::Result<Self> {
        let question = self.question.trim().to_owned();
        anyhow::ensure!(!question.is_empty(), "Введите вопрос");
        anyhow::ensure!(
            question.chars().count() <= 300,
            "Вопрос должен быть до 300 символов"
        );
        let choices: Vec<_> = self
            .choices
            .iter()
            .map(|choice| choice.trim())
            .filter(|choice| !choice.is_empty())
            .map(str::to_owned)
            .collect();
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
        Ok(Self {
            question,
            choices,
            ..self
        })
    }

    pub fn into_poll(self, id: i64) -> Poll {
        Poll {
            id,
            question: self.question,
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
                })
                .collect(),
            closed: false,
            public: self.public,
            multiple: self.multiple,
            quiz: false,
            revoting_disabled: false,
            total: None,
            solution: None,
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
    chats: HashMap<i64, CachedHistory>,
}

pub struct CachedHistory {
    pub messages: BTreeMap<i32, Message>,
    pub has_older: bool,
    pub revision: HistoryRevision,
    pub refreshed: Instant,
    used: Instant,
}

impl HistoryCache {
    pub fn update_polls(&mut self, mut update: impl FnMut(i64, i32, &mut Poll)) {
        for (&chat, history) in &mut self.chats {
            for (&id, message) in &mut history.messages {
                if let Some(poll) = &mut message.poll {
                    update(chat, id, poll);
                }
            }
        }
    }

    pub fn remove(&mut self, chat: i64) {
        self.chats.remove(&chat);
    }

    pub fn restore(&mut self, chat: i64, now: Instant) -> Option<CachedHistory> {
        self.expire(now);
        self.chats.remove(&chat)
    }

    pub fn store(
        &mut self,
        chat: i64,
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
            chat,
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
    use super::*;

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
            unread: 0,
            unread_mark: false,
            last_message_id: 0,
            broadcast: false,
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
                reply_to: None,
                reply_preview: None,
                can_reply: true,
                can_edit: false,
                can_delete: false,
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
            public: false,
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
}
