mod admin_log;
mod chat_addresses;
mod create_chat;
mod desktop_notifications;
mod poll_voters;
pub(crate) use desktop_notifications::NotificationFilter;
use desktop_notifications::{PendingNotification, notification_for};
mod forum_management;
mod forum_mode;
mod forum_topics;
mod group_permissions;
mod invite_members;
mod member_restrictions;
mod poll_management;
mod stickers;
pub use group_permissions::slow_mode_wait;
use invite_members::invite_restricted;
mod poll_timing;

mod poll_answers;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use grammers_client::client::{
    GlobalSearchIter, LoginToken, PasswordToken, PhoneLogin, QrLogin, SearchIter,
};
use grammers_client::media::{Downloadable, InputMedia, Media, PhotoSize, Uploaded};
use grammers_client::message::{InputMessage, Message as TelegramMessage};
use grammers_client::peer::Peer;
use grammers_client::sender::{SenderPool, SenderPoolHandle, UpdatesConfiguration};
use grammers_client::tl::{enums, types};
use grammers_client::update::Update;
use grammers_client::{Client, InvocationError, SignInError};
use grammers_session::{
    Session,
    storages::SqliteSession,
    types::{PeerId, PeerKind, PeerRef},
};
use tokio::sync::{Mutex, RwLock, Semaphore, broadcast, mpsc, watch};

use crate::model::{
    AdminPolicy, AdminRight, AdminRights, AdminState, AudioInfo, BotCommand, ChatInfo, ChatMember,
    INVITE_PAGE_SIZE, InviteAction, InviteChanges, InviteCursor, InviteLink, InvitePage,
    JOIN_REQUEST_PAGE_SIZE, JoinDecision, JoinDecisionOutcome, JoinRequest, JoinRequestCursor,
    JoinRequestPage, MEMBER_PAGE_SIZE, MemberChange, MemberFilter, MemberModeration, MemberPage,
    MemberRole, TextLink,
};
use crate::model::{
    AttachmentThumbnail, Avatar, BLOCKED_PAGE_SIZE, BlockedPage, BlockedPeer, Chat, ChatFolder,
    ChatImage, ChatKind, Contact, DeviceChange, DeviceSession, DeviceSessions, FormattedText,
    HISTORY_PAGE_SIZE, HistoryRevision, MAX_ALBUM_ITEMS, Message, OwnProfile, PeerProfile,
    PlayableMedia, PlaybackProgress, PlaybackSource, Poll, PollChoice, PollDraft, PollUpdate,
    PollVoteCount, PrivacyAudience, PrivacyChange, PrivacyKey, PrivacySetting, ProfileDetails,
    ProfileEdit, ProfileEditResult, Reaction, ReadRequest, ReplyPreview, SearchBuffer,
    SearchHandle, SearchHit, SearchPage, TextSpan, TextStyle, UpdateStatus, Upload, UploadKind,
    redact_spoilers,
};
use crate::session_runner::SessionRunner;
use crate::storage::{
    PendingSave, PlaybackDownload, SessionRemoval, attachment_name, open_session_file,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginStep {
    Phone,
    Code,
    Password,
    Authorized,
}

enum LoginState {
    Phone,
    Code(LoginToken),
    Password(PasswordToken),
    Authorized,
}

impl LoginState {
    fn step(&self) -> LoginStep {
        match self {
            Self::Phone => LoginStep::Phone,
            Self::Code(_) => LoginStep::Code,
            Self::Password(_) => LoginStep::Password,
            Self::Authorized => LoginStep::Authorized,
        }
    }
}

async fn initial_login_state(
    authorization: impl std::future::Future<Output = Result<bool>>,
    has_api_hash: bool,
) -> Result<LoginState> {
    if authorization.await? {
        Ok(LoginState::Authorized)
    } else {
        ensure!(
            has_api_hash,
            "Сессия истекла. Введите API hash для повторного входа."
        );
        Ok(LoginState::Phone)
    }
}

#[derive(Clone)]
struct ChatAuthor {
    name: String,
    avatar: Option<Avatar>,
}

pub struct Telegram {
    client: Client,
    session: Arc<SqliteSession>,
    sender: SenderPoolHandle,
    runner: SessionRunner,
    session_removal: SessionRemoval,
    updates: Mutex<Option<mpsc::Receiver<grammers_session::updates::UpdatesLike>>>,
    api_hash: Option<String>,
    state: Mutex<LoginState>,
    peers: RwLock<HashMap<i64, PeerRef>>,
    chat_authors: StdMutex<HashMap<PeerId, ChatAuthor>>,
    changes: watch::Sender<UpdateStatus>,
    history_changes: StdMutex<(u64, HashMap<i64, HistoryRevision>)>,
    image_downloads: Semaphore,
    stickers: StdMutex<stickers::StickerCache>,
    sticker_settings: Mutex<()>,
    profile: RwLock<Option<Arc<Peer>>>,
    profile_edit: Mutex<()>,
    contacts_cache: Mutex<Option<ContactSnapshot>>,
    contacts_revision: AtomicU64,
    dialog_settings: Mutex<()>,
    privacy_settings: Mutex<()>,
    blocked_changes: watch::Sender<u64>,
    join_request_changes: broadcast::Sender<i64>,
    poll_updates: broadcast::Sender<PollUpdate>,
    poll_answer_changes: broadcast::Sender<(i64, i32)>,
    read_state: crate::read_state::ReadState,
}

struct ContactSnapshot {
    loaded: Instant,
    revision: u64,
    contacts: Arc<Vec<Contact>>,
}

impl ContactSnapshot {
    fn is_fresh(&self, revision: u64, now: Instant) -> bool {
        self.revision == revision
            && now.saturating_duration_since(self.loaded) < Duration::from_mins(5)
    }
}

enum SearchMessages {
    Global(GlobalSearchIter),
    Chat(SearchIter),
}

impl SearchMessages {
    async fn next(&mut self) -> Result<Option<TelegramMessage>, InvocationError> {
        match self {
            Self::Global(messages) => messages.next().await,
            Self::Chat(messages) => messages.next().await,
        }
    }
}

impl Telegram {
    pub async fn connect(
        profile: crate::model::AccountProfile,
        api_hash: Option<String>,
    ) -> Result<(Arc<Self>, LoginStep)> {
        let settings = tokio::task::spawn_blocking(crate::proxy::load).await??;
        let configuration = crate::proxy::configuration(&settings)?;
        let (path, lock) = open_session_file(&profile, api_hash.is_some())?;
        let session = Arc::new(
            SqliteSession::open(&path)
                .await
                .context("Не удалось открыть базу сессии")?,
        );
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::with_configuration(session.clone(), profile.api_id, configuration);
        let sender = (*handle).clone();
        let client = Client::new(handle);
        let session_removal = SessionRemoval::new(path, lock);
        let runner_lock = session_removal.clone();
        let runner = SessionRunner::spawn(async move {
            let _lock = runner_lock;
            runner.run().await;
        });
        let (runner, state) = runner
            .finish_startup(
                Duration::from_secs(20),
                initial_login_state(
                    async { client.is_authorized().await.map_err(anyhow::Error::from) },
                    api_hash.is_some(),
                ),
            )
            .await?;
        let step = state.step();
        let (changes, _) = watch::channel(UpdateStatus::default());
        Ok((
            Arc::new(Self {
                client,
                session,
                sender,
                runner,
                session_removal,
                updates: Mutex::new(Some(updates)),
                api_hash,
                state: Mutex::new(state),
                peers: RwLock::new(HashMap::new()),
                chat_authors: StdMutex::new(HashMap::new()),
                changes,
                history_changes: StdMutex::default(),
                image_downloads: Semaphore::new(4),
                stickers: StdMutex::new(stickers::StickerCache::default()),
                sticker_settings: Mutex::new(()),
                profile: RwLock::new(None),
                profile_edit: Mutex::new(()),
                contacts_cache: Mutex::new(None),
                contacts_revision: AtomicU64::new(0),
                dialog_settings: Mutex::new(()),
                privacy_settings: Mutex::new(()),
                blocked_changes: watch::channel(0).0,
                join_request_changes: broadcast::channel(32).0,
                poll_updates: broadcast::channel(64).0,
                poll_answer_changes: broadcast::channel(64).0,
                read_state: crate::read_state::ReadState::default(),
            }),
            step,
        ))
    }

    pub async fn logout(&self) -> Result<SessionRemoval> {
        let result = tokio::time::timeout(Duration::from_secs(25), self.client.sign_out())
            .await
            .context("Telegram не подтвердил выход вовремя")?;
        match result {
            Ok(_) => {}
            Err(error) if authorization_already_ended(&error) => {}
            Err(error) => return Err(error.into()),
        }
        self.client.disconnect();
        Ok(self.session_removal.clone())
    }

    async fn privacy_rules(&self, key: PrivacyKey) -> Result<types::account::PrivacyRules> {
        Ok(self
            .client
            .invoke(&grammers_client::tl::functions::account::GetPrivacy {
                key: crate::privacy_rules::key(key),
            })
            .await?
            .into())
    }

    pub async fn privacy_setting(&self, key: PrivacyKey) -> Result<PrivacySetting> {
        Ok(crate::privacy_rules::current(
            key,
            &self.privacy_rules(key).await?,
        ))
    }

    pub async fn set_privacy_setting(
        &self,
        key: PrivacyKey,
        baseline: &PrivacySetting,
        audience: PrivacyAudience,
    ) -> Result<PrivacyChange> {
        use crate::privacy_rules;
        let _operation = self.privacy_settings.lock().await;
        let raw = self.privacy_rules(key).await?;
        let Some(rules) = privacy_rules::prepare(key, baseline, &raw, audience)? else {
            return Ok(PrivacyChange::Conflict(privacy_rules::current(key, &raw)));
        };
        let updated: types::account::PrivacyRules = self
            .client
            .invoke(&grammers_client::tl::functions::account::SetPrivacy {
                key: privacy_rules::key(key),
                rules,
            })
            .await?
            .into();
        Ok(PrivacyChange::Saved {
            setting: privacy_rules::current(key, &updated),
            exceptions_changed: !privacy_rules::same_exceptions(&raw, &updated),
        })
    }

    pub async fn device_sessions(&self) -> Result<DeviceSessions> {
        let raw = self
            .client
            .invoke(&grammers_client::tl::functions::account::GetAuthorizations {})
            .await?;
        Ok(device_sessions_view(raw.into()))
    }

    pub async fn change_device_session(&self, change: DeviceChange) -> Result<()> {
        let confirmed = match change {
            DeviceChange::Terminate(id) => {
                let snapshot = self.device_sessions().await?;
                let Some(request) = device_reset_request(&snapshot, id)? else {
                    // A different client may have already terminated this session.
                    return Ok(());
                };
                self.client.invoke(&request).await
            }
            DeviceChange::TerminateOthers => {
                // This API atomically excludes the current authorization. tgrust
                // uses live MTProto updates, not a registered push-device token.
                self.client
                    .invoke(&grammers_client::tl::functions::auth::ResetAuthorizations {})
                    .await
            }
            DeviceChange::SetTtl(days) => self.client.invoke(&device_ttl_request(days)?).await,
        }
        .map_err(device_change_error)?;
        ensure!(confirmed, "Telegram не подтвердил изменение сеансов");
        Ok(())
    }

    pub async fn login_step(&self) -> LoginStep {
        self.state.lock().await.step()
    }

    pub async fn login_by_qr(
        &self,
        except_ids: Vec<i64>,
        mut cancel: watch::Receiver<bool>,
        tokens: mpsc::Sender<Vec<u8>>,
    ) -> Result<LoginStep> {
        let mut state = self.state.lock().await;
        if !matches!(*state, LoginState::Phone) {
            return Ok(state.step());
        }
        let hash = self
            .api_hash
            .as_deref()
            .context("Для входа нужны ключи приложения")?;
        let mut updates = self.updates.lock().await;
        let updates = updates.as_mut().context("Поток входа уже закрыт")?;
        while !*cancel.borrow() {
            let result = tokio::time::timeout(
                Duration::from_secs(18),
                self.client.export_qr_login(hash, except_ids.clone()),
            )
            .await
            .context("Telegram не выдал QR-код вовремя")?;
            match result {
                Ok(QrLogin::Pending { token, expires }) => {
                    tokio::select! {
                        _ = cancel.changed() => break,
                        result = tokens.send(token) => if result.is_err() { break; },
                    }
                    if !crate::qr_login::wait(updates, &mut cancel, expires).await? {
                        break;
                    }
                }
                Ok(QrLogin::Authorized(user)) => {
                    self.remember_profile(*user).await;
                    *state = LoginState::Authorized;
                    break;
                }
                Ok(QrLogin::PasswordRequired(token)) => {
                    *state = LoginState::Password(token);
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(state.step())
    }

    pub async fn login(&self, value: &str) -> Result<LoginStep> {
        let mut state = self.state.lock().await;
        match &*state {
            LoginState::Phone => {
                let number = normalize_phone(value)?;
                let api_hash = self
                    .api_hash
                    .as_deref()
                    .context("Для входа нужен API hash")?;
                match self.client.request_phone_login(&number, api_hash).await? {
                    PhoneLogin::Code(token) => *state = LoginState::Code(token),
                    PhoneLogin::Authorized(user) => {
                        self.remember_profile(*user).await;
                        *state = LoginState::Authorized;
                    }
                    PhoneLogin::PaymentRequired => {
                        bail!(
                            "Для этого номера Telegram требует дополнительный шаг оплаты. Воспользуйтесь QR-кодом с уже подключённого устройства."
                        )
                    }
                }
                Ok(state.step())
            }
            LoginState::Code(token) => match self.client.sign_in(token, value).await {
                Ok(user) => {
                    self.remember_profile(user).await;
                    *state = LoginState::Authorized;
                    Ok(LoginStep::Authorized)
                }
                Err(SignInError::PasswordRequired(token)) => {
                    *state = LoginState::Password(token);
                    Ok(LoginStep::Password)
                }
                Err(SignInError::InvalidCode) => {
                    bail!("Код неверен или истёк. Можно запросить новый код.")
                }
                Err(error) => Err(error.into()),
            },
            LoginState::Password(_) => {
                let LoginState::Password(token) = std::mem::replace(&mut *state, LoginState::Phone)
                else {
                    unreachable!();
                };
                match self.client.check_password(token, value).await {
                    Ok(user) => {
                        self.remember_profile(user).await;
                        *state = LoginState::Authorized;
                        Ok(LoginStep::Authorized)
                    }
                    Err(SignInError::InvalidPassword(token)) => {
                        *state = LoginState::Password(token);
                        bail!("Неверный облачный пароль");
                    }
                    Err(error) => Err(error).context("Запросите новый код для повторного входа"),
                }
            }
            LoginState::Authorized => Ok(LoginStep::Authorized),
        }
    }

    pub async fn restart_login(&self) {
        let mut state = self.state.lock().await;
        if !matches!(*state, LoginState::Authorized) {
            *state = LoginState::Phone;
        }
    }

    pub fn changes(&self) -> watch::Receiver<UpdateStatus> {
        self.changes.subscribe()
    }

    pub fn blocked_changes(&self) -> watch::Receiver<u64> {
        self.blocked_changes.subscribe()
    }

    pub fn join_request_changes(&self) -> broadcast::Receiver<i64> {
        self.join_request_changes.subscribe()
    }

    pub fn poll_updates(&self) -> broadcast::Receiver<PollUpdate> {
        self.poll_updates.subscribe()
    }

    pub fn poll_answer_changes(&self) -> broadcast::Receiver<(i64, i32)> {
        self.poll_answer_changes.subscribe()
    }

    fn invalidate_blocked(&self) {
        self.blocked_changes
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    pub async fn listen(
        self: Arc<Self>,
        mut stop: watch::Receiver<bool>,
        incoming: mpsc::Sender<PendingNotification>,
    ) -> Result<()> {
        let Some(receiver) = self.updates.lock().await.take() else {
            return Ok(());
        };
        let mut updates = self
            .client
            .stream_updates(receiver, UpdatesConfiguration { catch_up: true })
            .await
            .map_err(anyhow::Error::from_boxed)?;
        let mut retry_delay = Duration::from_secs(1);
        let started = SystemTime::now();
        let result = 'listen: loop {
            let result = tokio::select! {
                _ = stop.changed() => break Ok(()),
                update = updates.next() => update,
            };
            match result {
                Ok(update) => {
                    retry_delay = Duration::from_secs(1);
                    self.record_history_change(&update);
                    if let Update::NewMessage(message) = &update {
                        let message: &TelegramMessage = message;
                        if let Some(target) = poll_answers::changed_poll(&message.raw) {
                            let _ = self.poll_answer_changes.send(target);
                        }
                    }
                    if let enums::Update::MessagePoll(update) = update.raw() {
                        let _ = self.poll_updates.send(poll_update(update));
                    }
                    if matches!(update.raw(), enums::Update::PeerBlocked(_)) {
                        self.invalidate_blocked();
                    }
                    if let enums::Update::PendingJoinRequests(update) = update.raw()
                        && let Some(chat) =
                            raw_peer_id(&update.peer).and_then(PeerId::bot_api_dialog_id)
                    {
                        let _ = self.join_request_changes.send(chat);
                    }
                    if let Update::NewMessage(message) = &update
                        && let Some(notification) = notification_for(message, started)
                    {
                        let _ = incoming.try_send(notification);
                    }
                    if matches!(
                        update.raw(),
                        enums::Update::UserName(_)
                            | enums::Update::UserPhone(_)
                            | enums::Update::User(_)
                            | enums::Update::ContactsReset
                            | enums::Update::PeerSettings(_)
                    ) {
                        self.contacts_revision.fetch_add(1, Ordering::Relaxed);
                    }
                    if matches!(
                        update,
                        Update::NewMessage(_)
                            | Update::MessageEdited(_)
                            | Update::MessageDeleted(_)
                    ) || matches!(
                        update.raw(),
                        enums::Update::ReadHistoryInbox(_)
                            | enums::Update::ReadChannelInbox(_)
                            | enums::Update::PinnedMessages(_)
                            | enums::Update::PinnedChannelMessages(_)
                            | enums::Update::MessageReactions(_)
                            | enums::Update::FolderPeers(_)
                            | enums::Update::DialogPinned(_)
                            | enums::Update::PinnedDialogs(_)
                            | enums::Update::NotifySettings(_)
                            | enums::Update::DialogUnreadMark(_)
                            | enums::Update::Channel(_)
                    ) {
                        self.invalidate_views();
                    }
                    if matches!(
                        update.raw(),
                        enums::Update::DialogFilter(_)
                            | enums::Update::DialogFilterOrder(_)
                            | enums::Update::DialogFilters
                    ) {
                        self.changes.send_modify(|status| {
                            status.folders_revision = status.folders_revision.wrapping_add(1)
                        });
                    }
                }
                Err(error) if !self.runner.is_finished() && retry_update_error(&error) => {
                    let reconnect = self.reconnect_updates(error, &mut retry_delay);
                    tokio::select! {
                        _ = stop.changed() => break 'listen Ok(()),
                        result = reconnect => {
                            if let Err(error) = result {
                                break Err(error);
                            }
                        },
                    }
                }
                Err(error) => break Err(error.into()),
            }
        };
        let sync = updates
            .sync_update_state()
            .await
            .map_err(anyhow::Error::from_boxed);
        result.and(sync)
    }

    async fn reconnect_updates(
        &self,
        mut error: InvocationError,
        retry_delay: &mut Duration,
    ) -> Result<()> {
        self.changes.send_if_modified(|status| {
            let changed = !status.reconnecting;
            status.reconnecting = true;
            changed
        });
        loop {
            tokio::time::sleep(update_retry_delay(&error, *retry_delay)).await;
            *retry_delay = (*retry_delay * 2).min(Duration::from_secs(30));
            match tokio::time::timeout(Duration::from_secs(15), self.client.is_authorized()).await {
                Ok(Ok(true)) => {
                    self.invalidate_history(None, true);
                    self.invalidate_blocked();
                    self.changes.send_modify(|status| {
                        status.reconnecting = false;
                        status.revision = status.revision.wrapping_add(1);
                        status.folders_revision = status.folders_revision.wrapping_add(1);
                    });
                    // The existing stream can now resume its pending difference request.
                    return Ok(());
                }
                Ok(Ok(false)) => bail!("Сессия Telegram больше не авторизована"),
                Ok(Err(next)) if !self.runner.is_finished() && retry_update_error(&next) => {
                    error = next;
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(timeout) => {
                    self.sender.disconnect_from_dc(self.session.home_dc_id()?);
                    error = InvocationError::Io(timeout.into());
                }
            }
        }
    }

    fn invalidate_views(&self) {
        self.changes
            .send_modify(|status| status.revision = status.revision.wrapping_add(1));
    }

    pub fn history_revision(&self, chat: i64) -> HistoryRevision {
        let (all, chats) = &*self.history_changes.lock().unwrap();
        HistoryRevision {
            all: *all,
            ..chats.get(&chat).copied().unwrap_or_default()
        }
    }

    fn record_history_change(&self, update: &Update) {
        let chat = match update {
            Update::NewMessage(message) | Update::MessageEdited(message) => {
                message.peer_id().bot_api_dialog_id()
            }
            Update::MessageDeleted(deletion) => deletion
                .channel_id()
                .and_then(PeerId::channel)
                .and_then(PeerId::bot_api_dialog_id),
            _ => match update.raw() {
                enums::Update::PinnedMessages(update) => {
                    PeerId::from(update.peer.clone()).bot_api_dialog_id()
                }
                enums::Update::PinnedChannelMessages(update) => {
                    PeerId::channel(update.channel_id).and_then(PeerId::bot_api_dialog_id)
                }
                enums::Update::MessageReactions(update) => {
                    PeerId::from(update.peer.clone()).bot_api_dialog_id()
                }
                _ => return,
            },
        };
        self.invalidate_history(chat, !matches!(update, Update::NewMessage(_)));
    }

    fn invalidate_history(&self, chat: Option<i64>, rewritten: bool) {
        let (all, chats) = &mut *self.history_changes.lock().unwrap();
        if let Some(chat) = chat
            && (chats.len() < 512 || chats.contains_key(&chat))
        {
            let revision = chats.entry(chat).or_default();
            revision.chat = revision.chat.wrapping_add(1);
            if rewritten {
                revision.rewritten = revision.chat;
            }
        } else {
            *all = all.wrapping_add(1);
            chats.clear();
        }
    }

    pub async fn chats(&self) -> Result<Vec<Chat>> {
        let profile = self.profile().await;
        let profile = profile.as_deref();
        let now = SystemTime::now();
        let mut chats = Vec::new();
        let mut peers = HashMap::new();
        let mut notify_defaults = HashMap::new();
        let mut dialogs = self.client.iter_dialogs().limit(150);
        while let Some(dialog) = dialogs.next().await? {
            let Some(id) = dialog.peer_id().bot_api_dialog_id() else {
                continue;
            };
            let enums::Dialog::Dialog(raw) = &dialog.raw else {
                continue;
            };
            let peer = dialog.peer();
            let title = match peer {
                Peer::User(user) if user.is_self() => "Избранное",
                _ => peer
                    .name()
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Без названия"),
            };
            let group = matches!(peer, Peer::Group(_) | Peer::Community(_));
            let last = dialog.last_message.as_ref();
            let kind = chat_kind(peer);
            self.remember_chat_author(peer);
            let enums::PeerNotifySettings::Settings(settings) = &raw.notify_settings;
            let mute_until = match settings.mute_until {
                Some(until) => until,
                None => {
                    let category = notification_category(kind);
                    if let Some(until) = notify_defaults.get(&category) {
                        *until
                    } else {
                        let until = self.default_mute_until(category).await?;
                        notify_defaults.insert(category, until);
                        until
                    }
                }
            };
            chats.push(Chat {
                id,
                pinned: raw.pinned,
                last_activity: last.map_or(0, |message| message.date().as_second()),
                title: title.to_owned(),
                preview: last
                    .map(|message| {
                        let text = message_text(message);
                        redact_spoilers(&text, &message_styles(message)).into_owned()
                    })
                    .unwrap_or_default(),
                preview_sender: if group {
                    last.filter(|message| message.action().is_none())
                        .and_then(|message| {
                            self.message_author(message, profile)
                                .map(|author| author.name)
                        })
                } else {
                    None
                },
                preview_image: last.and_then(|message| {
                    let (media, _) = attachment(message)?;
                    thumbnail(&media).map(|media| ChatImage::attachment(id, message.id(), media))
                }),
                group,
                forum: forum_topics::is_forum(peer),
                unread: raw.unread_count.max(0),
                unread_mark: raw.unread_mark,
                last_message_id: raw.top_message.max(0),
                broadcast: matches!(peer, Peer::Channel(_)),
                can_write: can_write_chat(peer),
                slow_mode_enabled: group_permissions::slow_mode_enabled(peer),
                saved_messages: matches!(peer, Peer::User(user) if user.is_self()),
                avatar: peer_avatar(peer),
                archived: raw.folder_id == Some(1),
                kind,
                muted: is_muted(mute_until, now),
            });
            peers.insert(id, dialog.peer_ref());
        }
        self.peers.write().await.extend(peers);
        Ok(chats)
    }

    async fn peer(&self, id: i64) -> Result<PeerRef> {
        self.peers
            .read()
            .await
            .get(&id)
            .copied()
            .context("Чат не найден")
    }

    pub async fn contacts(&self, refresh: bool) -> Result<Arc<Vec<Contact>>> {
        let mut cache = self.contacts_cache.lock().await;
        let revision = self.contacts_revision.load(Ordering::Relaxed);
        if !refresh
            && let Some(cached) = cache.as_ref()
            && cached.is_fresh(revision, Instant::now())
        {
            return Ok(cached.contacts.clone());
        }
        let result = self
            .client
            .invoke(&grammers_client::tl::functions::contacts::GetContacts { hash: 0 })
            .await?;
        let enums::contacts::Contacts::Contacts(result) = result else {
            bail!("Telegram не вернул список контактов. Повторите загрузку");
        };
        let mut users: HashMap<_, _> = result
            .users
            .into_iter()
            .map(|user| (user.id(), user))
            .collect();
        let mut peers = HashMap::new();
        let mut contacts = Vec::new();
        for contact in result.contacts {
            let enums::Contact::Contact(contact) = contact;
            let Some(raw) = users.remove(&contact.user_id) else {
                continue;
            };
            let user = grammers_client::peer::User::from_raw(&self.client, raw);
            let reference = user.to_ref().await.map_err(anyhow::Error::from_boxed)?;
            let peer = Peer::User(user);
            let profile = peer_profile_view(&peer)?;
            if let Some(reference) = reference {
                peers.insert(profile.id, reference);
            }
            contacts.push(Contact {
                id: profile.id,
                name: profile.title,
                usernames: profile.usernames,
                phone: profile.phone,
                avatar: profile.avatar,
                kind: if profile.kind == ChatKind::Bot {
                    ChatKind::Bot
                } else {
                    ChatKind::Contact
                },
                is_self: profile.is_self,
                available: reference.is_some(),
            });
        }
        contacts.sort_by_cached_key(|contact| (contact.name.to_lowercase(), contact.id));
        self.peers.write().await.extend(peers);
        let contacts = Arc::new(contacts);
        *cache = Some(ContactSnapshot {
            loaded: Instant::now(),
            revision,
            contacts: contacts.clone(),
        });
        Ok(contacts)
    }

    pub async fn peer_profile(&self, id: i64) -> Result<PeerProfile> {
        let reference = self.peer(id).await?;
        if reference.id.kind() == PeerKind::User {
            let enums::users::UserFull::Full(result) = self
                .client
                .invoke(&grammers_client::tl::functions::users::GetFullUser {
                    id: reference.into(),
                })
                .await?;
            let enums::UserFull::Full(full) = result.full_user;
            ensure!(
                Some(full.id) == reference.id.bare_id(),
                "Telegram вернул другой профиль"
            );
            let raw = result
                .users
                .into_iter()
                .find(|user| user.id() == full.id)
                .context("Профиль пользователя недоступен")?;
            let peer = Peer::User(grammers_client::peer::User::from_raw(&self.client, raw));
            let mut profile = peer_profile_view(&peer)?;
            if !matches!(&peer, Peer::User(user) if user.deleted()) {
                profile.about = full.about.unwrap_or_default();
                profile.blocked = (!profile.is_self).then_some(full.blocked);
                if profile.kind == ChatKind::Bot {
                    profile.bot_commands = bot_commands(full.id, full.bot_info);
                }
            }
            ensure!(profile.id == id, "Telegram вернул другой профиль");
            return Ok(profile);
        }
        let result = self.full_chat(reference).await?;
        let mut profile = self.chat_profile_view(reference, result.chats)?;
        match result.full_chat {
            enums::ChatFull::Full(full) => {
                profile.about = full.about;
                if let enums::ChatParticipants::Participants(participants) = full.participants {
                    profile.can_view_members = true;
                    profile.members = u32::try_from(participants.participants.len()).ok();
                }
            }
            enums::ChatFull::ChannelFull(full) => {
                if let Some(mode) = &mut profile.forum_mode {
                    mode.linked_discussion = full.linked_chat_id.is_some();
                    mode.hidden_history = full.hidden_prehistory;
                }
                if let Some(permissions) = &mut profile.group_permissions {
                    permissions.slow_mode = Some(full.slowmode_seconds.unwrap_or_default());
                }
                profile.about = full.about;
                profile.can_view_members = full.can_view_participants;
                profile.blocked = (profile.kind == ChatKind::Broadcast).then_some(full.blocked);
                profile.members = full
                    .participants_count
                    .and_then(|count| u32::try_from(count).ok())
                    .or(profile.members);
                profile.online_members = full
                    .online_count
                    .and_then(|count| u32::try_from(count).ok());
            }
            enums::ChatFull::CommunityFull(full) => profile.about = full.about,
        }
        ensure!(profile.id == id, "Telegram вернул другой чат");
        Ok(profile)
    }

    fn chat_profile_view(
        &self,
        reference: PeerRef,
        chats: Vec<enums::Chat>,
    ) -> Result<PeerProfile> {
        let peer = chats
            .into_iter()
            .map(|chat| Peer::from_raw(&self.client, chat))
            .find(|peer| peer.id() == reference.id)
            .context("Информация о чате недоступна")?;
        peer_profile_view(&peer)
    }

    async fn full_chat(&self, reference: PeerRef) -> Result<types::messages::ChatFull> {
        let enums::messages::ChatFull::Full(result) = match reference.id.kind() {
            PeerKind::Chat => {
                self.client
                    .invoke(&grammers_client::tl::functions::messages::GetFullChat {
                        chat_id: reference.id.bare_id().context("Чат не найден")?,
                    })
                    .await?
            }
            PeerKind::Channel => {
                self.client
                    .invoke(&grammers_client::tl::functions::channels::GetFullChannel {
                        channel: reference.into(),
                    })
                    .await?
            }
            PeerKind::User => bail!("У пользователя нет списка участников"),
        };
        ensure!(
            Some(result.full_chat.id()) == reference.id.bare_id(),
            "Telegram вернул другой чат"
        );
        Ok(result)
    }

    pub async fn edit_chat_info(
        &self,
        id: i64,
        original: ChatInfo,
        proposed: ChatInfo,
    ) -> Result<ChatInfo> {
        let _serial = self.dialog_settings.lock().await;
        let profile = self.peer_profile(id).await?;
        ensure!(
            profile.can_edit_chat,
            "Telegram не разрешает менять данные этого чата"
        );
        let reference = self.peer(id).await?;
        let mut current = profile.chat_info();
        let (title, about) = proposed.changes(&original, &current)?;
        if let Some(title) = title {
            let result = match reference.id.kind() {
                PeerKind::Chat => {
                    self.client
                        .invoke(&grammers_client::tl::functions::messages::EditChatTitle {
                            chat_id: reference.id.bare_id().context("Чат не найден")?,
                            title: title.clone(),
                        })
                        .await
                }
                PeerKind::Channel => {
                    self.client
                        .invoke(&grammers_client::tl::functions::channels::EditTitle {
                            channel: reference.into(),
                            title: title.clone(),
                        })
                        .await
                }
                PeerKind::User => bail!("Нельзя менять название собеседника"),
            };
            if let Err(error) = result
                && !error.is("CHAT_NOT_MODIFIED")
            {
                return Err(error.into());
            }
            current.title = title;
            self.invalidate_history(Some(id), false);
            self.invalidate_views();
        }
        if let Some(about) = about {
            let result = self
                .client
                .invoke(&grammers_client::tl::functions::messages::EditChatAbout {
                    peer: reference.into(),
                    about: about.clone(),
                })
                .await;
            match result {
                Ok(true) => {}
                Err(error)
                    if error.is("CHAT_ABOUT_NOT_MODIFIED") || error.is("CHAT_NOT_MODIFIED") => {}
                Ok(false) => bail!("Telegram не подтвердил изменение описания"),
                Err(error) => return Err(error.into()),
            }
            current.about = about;
            self.invalidate_views();
        }
        Ok(current)
    }

    async fn invite_peer(&self, chat: i64) -> Result<PeerRef> {
        let reference = self.peer(chat).await?;
        let full = self.full_chat(reference).await?;
        ensure!(
            self.chat_profile_view(reference, full.chats)?
                .can_manage_invites,
            "Telegram не разрешает управлять ссылками этого чата"
        );
        Ok(reference)
    }

    pub async fn invite_links(
        &self,
        chat: i64,
        revoked: bool,
        cursor: Option<InviteCursor>,
    ) -> Result<InvitePage> {
        use grammers_client::tl::functions::messages::GetExportedChatInvites;
        let peer = self.invite_peer(chat).await?;
        let enums::messages::ExportedChatInvites::Invites(result) = self
            .client
            .invoke(&GetExportedChatInvites {
                revoked,
                peer: peer.into(),
                admin_id: enums::InputUser::UserSelf,
                offset_date: cursor.as_ref().map(|cursor| cursor.date),
                offset_link: cursor.as_ref().map(|cursor| cursor.link.clone()),
                limit: INVITE_PAGE_SIZE,
            })
            .await?;
        Ok(exported_invite_page(result, cursor.as_ref(), revoked))
    }

    pub async fn change_invite(&self, chat: i64, action: InviteAction) -> Result<()> {
        use grammers_client::tl::functions::messages::{EditExportedChatInvite, ExportChatInvite};
        let _settings = self.dialog_settings.lock().await;
        let peer = self.invite_peer(chat).await?;
        match action {
            InviteAction::Create(draft) => {
                let draft = draft.normalize()?;
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                let invite = self
                    .client
                    .invoke(&ExportChatInvite {
                        legacy_revoke_permanent: false,
                        request_needed: draft.approval,
                        peer: peer.into(),
                        expire_date: draft.expiration(now)?,
                        usage_limit: draft.limit,
                        title: (!draft.title.is_empty()).then_some(draft.title),
                        subscription_pricing: None,
                    })
                    .await?;
                ensure!(
                    invite_link(invite)
                        .is_some_and(|link| !link.revoked && link.approval == draft.approval),
                    "Telegram не подтвердил создание ссылки"
                );
            }
            InviteAction::Edit(original, edit) => {
                let link = &original.cursor.link;
                let current = self.own_invite(peer, link).await?;
                let changes = edit.changes(&original, &current)?;
                if changes == InviteChanges::default() {
                    return Ok(());
                }
                let result = self
                    .client
                    .invoke(&EditExportedChatInvite {
                        revoked: false,
                        peer: peer.into(),
                        link: link.clone(),
                        expire_date: changes.expire_date,
                        usage_limit: changes.usage_limit,
                        request_needed: changes.request_needed,
                        title: changes.title.clone(),
                    })
                    .await?;
                ensure!(
                    invite_link(result.invite()).is_some_and(|updated| {
                        updated.editable()
                            && updated.cursor.link == *link
                            && changes.confirmed_by(&updated)
                    }),
                    "Telegram не подтвердил параметры ссылки"
                );
            }
            InviteAction::Revoke(link) => {
                let current = self.own_invite(peer, &link).await?;
                ensure!(
                    !current.paid,
                    "Эта ссылка связана с подпиской Telegram Stars"
                );
                if current.revoked {
                    return Ok(());
                }
                let result = self
                    .client
                    .invoke(&EditExportedChatInvite {
                        revoked: true,
                        peer: peer.into(),
                        link: link.clone(),
                        expire_date: None,
                        usage_limit: None,
                        request_needed: None,
                        title: None,
                    })
                    .await?;
                ensure!(
                    invite_link(result.invite())
                        .is_some_and(|old| old.revoked && old.cursor.link == link),
                    "Telegram не подтвердил отзыв ссылки"
                );
            }
        }
        Ok(())
    }

    async fn own_invite(&self, peer: PeerRef, link: &str) -> Result<InviteLink> {
        use grammers_client::tl::functions::messages::GetExportedChatInvite;
        let raw = self
            .client
            .invoke(&GetExportedChatInvite {
                peer: peer.into(),
                link: link.to_owned(),
            })
            .await?
            .invite();
        let account = match self.self_id().await? {
            Some(account) => account,
            None => self
                .profile()
                .await
                .context("Не удалось проверить владельца ссылки")?
                .id(),
        };
        let account = account
            .bare_id()
            .context("Не удалось проверить владельца ссылки")?;
        owned_invite_link(raw, account, link)
    }

    pub async fn join_requests(
        &self,
        chat: i64,
        after: Option<JoinRequestCursor>,
    ) -> Result<JoinRequestPage> {
        use grammers_client::tl::functions::messages::GetChatInviteImporters;
        let peer = self.invite_peer(chat).await?;
        let offset_user = match after {
            Some(after) => self.peer(after.user).await?.into(),
            None => enums::InputUser::Empty,
        };
        let enums::messages::ChatInviteImporters::Importers(result) = self
            .client
            .invoke(&GetChatInviteImporters {
                requested: true,
                subscription_expired: false,
                peer: peer.into(),
                link: None,
                q: None,
                offset_date: after.map_or(0, |after| after.date),
                offset_user,
                limit: JOIN_REQUEST_PAGE_SIZE,
            })
            .await?;
        let mut users: HashMap<_, _> = result
            .users
            .into_iter()
            .map(|user| (user.id(), user))
            .collect();
        let mut requests = Vec::new();
        let mut references = Vec::new();
        let mut next = None;
        for enums::ChatInviteImporter::Importer(importer) in result.importers {
            if !importer.requested || PeerId::user(importer.user_id).is_none() {
                continue;
            }
            let user = users
                .remove(&importer.user_id)
                .map(|user| grammers_client::peer::User::from_raw(&self.client, user));
            let cursor = JoinRequestCursor {
                date: importer.date,
                user: importer.user_id,
            };
            let mut actionable = false;
            if let Some(user) = user.as_ref()
                && let Ok(Some(reference)) = user.to_ref().await
            {
                next = Some(cursor);
                references.push((cursor.user, reference));
                actionable = !user.is_self() && !user.deleted();
            }
            let name = user
                .as_ref()
                .map(|user| user.full_name())
                .unwrap_or_default();
            requests.push(JoinRequest {
                cursor,
                name: if name.trim().is_empty() {
                    "Недоступный аккаунт".into()
                } else {
                    name
                },
                about: importer.about.unwrap_or_default(),
                avatar: user.map(Peer::User).as_ref().and_then(peer_avatar),
                actionable,
            });
        }
        self.peers.write().await.extend(references);
        Ok(JoinRequestPage {
            requests,
            next: next.filter(|next| Some(*next) != after),
            total: result.count.max(0) as usize,
        })
    }

    pub async fn decide_join_request(
        &self,
        chat: i64,
        user: i64,
        decision: JoinDecision,
    ) -> Result<JoinDecisionOutcome> {
        use grammers_client::tl::functions::messages::HideChatJoinRequest;
        let _settings = self.dialog_settings.lock().await;
        let peer = self.invite_peer(chat).await?;
        let user = self.peer(user).await?;
        ensure!(
            user.id.kind() == PeerKind::User && Some(user.id) != self.self_id().await?,
            "Пользователь заявки недоступен"
        );
        let result = self
            .client
            .invoke(&HideChatJoinRequest {
                approved: matches!(decision, JoinDecision::Approve),
                user_id: user.into(),
                peer: peer.into(),
            })
            .await;
        match result {
            Ok(_) => {
                if matches!(decision, JoinDecision::Approve) {
                    self.invalidate_views();
                }
                Ok(JoinDecisionOutcome::Applied)
            }
            Err(error)
                if error.is("HIDE_REQUESTER_MISSING") || error.is("USER_ALREADY_PARTICIPANT") =>
            {
                Ok(JoinDecisionOutcome::AlreadyHandled)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn chat_members(
        &self,
        id: i64,
        filter: MemberFilter,
        offset: i32,
    ) -> Result<MemberPage> {
        ensure!(offset >= 0, "Неверная страница списка участников");
        let reference = self.peer(id).await?;
        match reference.id.kind() {
            PeerKind::Chat => {
                ensure!(
                    !matches!(filter, MemberFilter::Banned | MemberFilter::Restricted),
                    "В этой группе нет списка ограничений"
                );
                let result = self.full_chat(reference).await?;
                let enums::ChatFull::Full(full) = result.full_chat else {
                    bail!("Профиль группы изменился")
                };
                let enums::ChatParticipants::Participants(participants) = full.participants else {
                    bail!("Telegram ограничил доступ к списку участников")
                };
                let roles = participants
                    .participants
                    .into_iter()
                    .filter_map(|participant| {
                        Some((
                            PeerId::user(participant.user_id())?,
                            basic_member_role(&participant),
                            basic_admin_state(&participant),
                        ))
                    });
                let peers = self.peers_from_entities(result.users, Vec::new());
                let mut members = self.member_views(peers, roles, filter).await;
                members.sort_unstable_by_key(|member| member.id);
                let count = members.len() as i32;
                let members = members
                    .into_iter()
                    .skip(offset as usize)
                    .take(MEMBER_PAGE_SIZE as usize)
                    .collect();
                Ok(MemberPage {
                    members,
                    next: member_next(
                        offset,
                        MEMBER_PAGE_SIZE.min((count - offset).max(0)) as usize,
                        count,
                    ),
                })
            }
            PeerKind::Channel => {
                use enums::ChannelParticipantsFilter as F;
                if filter == MemberFilter::Banned {
                    ensure!(
                        self.peer_profile(id).await?.moderation == Some(MemberModeration::Ban),
                        "Telegram не разрешает просматривать блокировки этого чата"
                    );
                }
                if filter == MemberFilter::Restricted {
                    ensure!(
                        self.peer_profile(id).await?.group_permissions.is_some(),
                        "Telegram не разрешает просматривать ограничения этого чата"
                    );
                }
                let result = self
                    .client
                    .invoke(&grammers_client::tl::functions::channels::GetParticipants {
                        channel: reference.into(),
                        filter: match filter {
                            MemberFilter::All => F::ChannelParticipantsRecent,
                            MemberFilter::Admins => F::ChannelParticipantsAdmins,
                            MemberFilter::Bots => F::ChannelParticipantsBots,
                            MemberFilter::Restricted => {
                                F::ChannelParticipantsBanned(types::ChannelParticipantsBanned {
                                    q: String::new(),
                                })
                            }
                            MemberFilter::Banned => {
                                F::ChannelParticipantsKicked(types::ChannelParticipantsKicked {
                                    q: String::new(),
                                })
                            }
                        },
                        offset,
                        limit: MEMBER_PAGE_SIZE,
                        hash: 0,
                    })
                    .await?;
                let enums::channels::ChannelParticipants::Participants(result) = result else {
                    bail!("Telegram не вернул страницу участников")
                };
                let next = member_next(offset, result.participants.len(), result.count);
                let roles = result.participants.into_iter().filter_map(|participant| {
                    let (id, role) = channel_member(&participant)?;
                    let admin = channel_admin_state(&participant)
                        .and_then(|(state, editable)| editable.then_some(state));
                    Some((id, role?, admin))
                });
                let peers = self.peers_from_entities(result.users, result.chats);
                Ok(MemberPage {
                    members: self.member_views(peers, roles, filter).await,
                    next,
                })
            }
            PeerKind::User => bail!("У пользователя нет списка участников"),
        }
    }

    async fn member_views(
        &self,
        mut peers: HashMap<PeerId, Peer>,
        roles: impl IntoIterator<Item = (PeerId, MemberRole, Option<AdminState>)>,
        filter: MemberFilter,
    ) -> Vec<ChatMember> {
        let mut members = Vec::new();
        let mut refs = Vec::new();
        for (peer_id, role, admin) in roles {
            let Some(id) = peer_id.bot_api_dialog_id() else {
                continue;
            };
            let peer = peers.remove(&peer_id);
            let (name, bot, eligible) = match peer.as_ref() {
                Some(Peer::User(user)) => (
                    user.full_name(),
                    user.is_bot(),
                    !user.is_self() && !user.deleted(),
                ),
                Some(peer) => (
                    peer.name().unwrap_or_default().to_owned(),
                    false,
                    role == MemberRole::Banned,
                ),
                None => (String::new(), false, false),
            };
            let mut member = ChatMember {
                id,
                name: if name.trim().is_empty() {
                    "Недоступный аккаунт".into()
                } else {
                    name
                },
                bot,
                avatar: None,
                role,
                can_update: false,
                admin: None,
            };
            if filter.includes(&member) {
                if let Some(peer) = peer
                    && let Ok(Some(reference)) = peer.to_ref().await
                {
                    refs.push((id, reference));
                    member.avatar = peer_avatar(&peer);
                    member.can_update = eligible;
                    member.admin = admin.filter(|_| eligible);
                }
                members.push(member);
            }
        }
        self.peers.write().await.extend(refs);
        members
    }

    pub async fn change_chat_member(
        &self,
        chat: i64,
        member: i64,
        change: MemberChange,
    ) -> Result<()> {
        if let MemberChange::SetAdmin {
            original,
            requested,
        } = change
        {
            return self
                .change_chat_admin(chat, member, original, requested)
                .await;
        }
        let _serial = self.dialog_settings.lock().await;
        let chat = self.peer(chat).await?;
        let member = self.peer(member).await?;
        let input: enums::InputPeer = member.into();
        ensure!(
            member.id != chat.id
                && input != enums::InputPeer::PeerSelf
                && Some(member.id) != self.self_id().await?,
            "Свой аккаунт или этот чат нельзя удалить или заблокировать"
        );
        let full = self.full_chat(chat).await?;
        let profile = self.chat_profile_view(chat, full.chats)?;
        match (profile.moderation, change) {
            (Some(MemberModeration::Remove), MemberChange::Remove) => {
                let enums::ChatFull::Full(full) = full.full_chat else {
                    bail!("Профиль группы изменился")
                };
                let enums::ChatParticipants::Participants(members) = full.participants else {
                    bail!("Telegram ограничил доступ к списку участников")
                };
                let id = member.id.bare_id().context("Участник недоступен")?;
                ensure!(member.id.kind() == PeerKind::User, "Участник недоступен");
                if let Some(current) = members
                    .participants
                    .iter()
                    .find(|entry| entry.user_id() == id)
                {
                    ensure!(
                        basic_member_role(current) == MemberRole::Member,
                        "Роль участника изменилась"
                    );
                    self.client
                        .invoke(&grammers_client::tl::functions::messages::DeleteChatUser {
                            revoke_history: false,
                            chat_id: full.id,
                            user_id: member.into(),
                        })
                        .await?;
                    self.invalidate_history(chat.id.bot_api_dialog_id(), false);
                }
            }
            (Some(MemberModeration::Ban), MemberChange::Ban | MemberChange::Unban) => {
                use grammers_client::tl::functions::channels::{EditBanned, GetParticipant};
                let current = self
                    .client
                    .invoke(&GetParticipant {
                        channel: chat.into(),
                        participant: input.clone(),
                    })
                    .await;
                let role = match current {
                    Ok(enums::channels::ChannelParticipant::Participant(result)) => {
                        let (id, role) = channel_member(&result.participant)
                            .context("Telegram вернул неизвестного участника")?;
                        ensure!(id == member.id, "Telegram вернул другого участника");
                        ensure!(
                            !matches!(
                                result.participant,
                                enums::ChannelParticipant::ParticipantSelf(_)
                            ),
                            "Свой аккаунт нельзя заблокировать"
                        );
                        role
                    }
                    Err(error) if error.is("USER_NOT_PARTICIPANT") => None,
                    Err(error) => return Err(error.into()),
                };
                if channel_moderation_needed(role, change)? {
                    self.client
                        .invoke(&EditBanned {
                            channel: chat.into(),
                            participant: input,
                            banned_rights: membership_banned_rights(change == MemberChange::Ban)
                                .into(),
                        })
                        .await?;
                }
            }
            _ => bail!("Telegram не разрешает это действие с участником"),
        }
        self.invalidate_views();
        Ok(())
    }

    async fn change_chat_admin(
        &self,
        chat: i64,
        member: i64,
        original: AdminState,
        requested: AdminState,
    ) -> Result<()> {
        use grammers_client::tl::functions::{channels::EditAdmin, messages::EditChatAdmin};
        let _serial = self.dialog_settings.lock().await;
        let chat = self.peer(chat).await?;
        let member = self.peer(member).await?;
        let own = match self.self_id().await? {
            Some(id) => id,
            None => self.profile().await.context("Аккаунт недоступен")?.id(),
        };
        ensure!(
            member.id.kind() == PeerKind::User
                && member.id != own
                && enums::InputPeer::from(member) != enums::InputPeer::PeerSelf,
            "Эту роль нельзя изменить"
        );
        let member_id = member.id.bare_id().context("Участник недоступен")?;
        let full = self.full_chat(chat).await?;
        let policy = self
            .chat_profile_view(chat, full.chats)?
            .admin_policy
            .context("Telegram не разрешает управлять администраторами этого чата")?;
        match policy {
            AdminPolicy::Basic => {
                let enums::ChatFull::Full(full) = full.full_chat else {
                    bail!("Профиль группы изменился");
                };
                let enums::ChatParticipants::Participants(members) = full.participants else {
                    bail!("Telegram ограничил доступ к списку участников");
                };
                let current = members
                    .participants
                    .iter()
                    .find(|entry| entry.user_id() == member_id)
                    .and_then(basic_admin_state)
                    .context("Участник недоступен или его роль нельзя изменить")?;
                if policy.needs_change(original, current, requested)? {
                    ensure!(
                        self.client
                            .invoke(&EditChatAdmin {
                                chat_id: full.id,
                                user_id: member.into(),
                                is_admin: requested != AdminState::Member,
                            })
                            .await?,
                        "Telegram не подтвердил изменение роли"
                    );
                }
            }
            AdminPolicy::Channel(_) => {
                let (current, editable) = self.channel_admin(chat, member).await?;
                ensure!(editable, "Telegram не разрешает менять эту роль");
                if policy.needs_change(original, current, requested)? {
                    self.client
                        .invoke(&EditAdmin {
                            channel: chat.into(),
                            user_id: member.into(),
                            admin_rights: telegram_admin_rights(requested.channel_rights()).into(),
                            rank: None,
                        })
                        .await?;
                    self.invalidate_views();
                    let (confirmed, _) = self.channel_admin(chat, member).await?;
                    ensure!(
                        matches!(
                            policy.needs_change(requested, confirmed, requested),
                            Ok(false)
                        ),
                        "Telegram не подтвердил изменение прав"
                    );
                    return Ok(());
                }
            }
        }
        self.invalidate_views();
        Ok(())
    }

    async fn channel_admin(&self, chat: PeerRef, member: PeerRef) -> Result<(AdminState, bool)> {
        use grammers_client::tl::functions::channels::GetParticipant;
        let enums::channels::ChannelParticipant::Participant(result) = self
            .client
            .invoke(&GetParticipant {
                channel: chat.into(),
                participant: member.into(),
            })
            .await?;
        ensure!(
            channel_member(&result.participant).is_some_and(|(id, _)| id == member.id),
            "Telegram вернул другого участника"
        );
        channel_admin_state(&result.participant).context("Telegram не разрешает менять эту роль")
    }

    fn peers_from_entities(
        &self,
        users: Vec<enums::User>,
        chats: Vec<enums::Chat>,
    ) -> HashMap<PeerId, Peer> {
        users
            .into_iter()
            .map(|user| Peer::User(grammers_client::peer::User::from_raw(&self.client, user)))
            .chain(
                chats
                    .into_iter()
                    .map(|chat| Peer::from_raw(&self.client, chat)),
            )
            .map(|peer| (peer.id(), peer))
            .collect()
    }

    pub async fn blocked_page(&self, offset: i32) -> Result<BlockedPage> {
        let raw = self
            .client
            .invoke(&grammers_client::tl::functions::contacts::GetBlocked {
                my_stories_from: false,
                offset,
                limit: BLOCKED_PAGE_SIZE,
            })
            .await?;
        let (entries, users, chats, count) = match raw {
            enums::contacts::Blocked::Blocked(raw) => (raw.blocked, raw.users, raw.chats, None),
            enums::contacts::Blocked::Slice(raw) => {
                (raw.blocked, raw.users, raw.chats, Some(raw.count))
            }
        };
        let next_offset = next_blocked_offset(offset, entries.len(), count);
        let peers = self.peers_from_entities(users, chats);
        let mut page = Vec::new();
        let mut refs = HashMap::new();
        for entry in entries {
            let enums::PeerBlocked::Blocked(entry) = entry;
            let Some(peer_id) = raw_peer_id(&entry.peer_id) else {
                continue;
            };
            let Some(id) = peer_id.bot_api_dialog_id() else {
                continue;
            };
            let peer = peers.get(&peer_id);
            let reference = match peer {
                Some(peer) => peer.to_ref().await.map_err(anyhow::Error::from_boxed)?,
                None => self.session.peer_ref(peer_id).await?,
            };
            if let Some(reference) = reference {
                refs.insert(id, reference);
            }
            let profile = peer.map(peer_profile_view).transpose()?;
            page.push(BlockedPeer {
                id,
                avatar: profile
                    .as_ref()
                    .and_then(|profile| reference.and(profile.avatar)),
                username: profile
                    .as_ref()
                    .and_then(|profile| profile.usernames.first().cloned()),
                name: profile.map_or_else(
                    || format!("Недоступный профиль ({id})"),
                    |profile| profile.title,
                ),
                available: reference.is_some(),
            });
        }
        self.peers.write().await.extend(refs);
        Ok(BlockedPage {
            peers: page,
            next_offset,
        })
    }

    pub async fn set_blocked(&self, id: i64, blocked: bool) -> Result<()> {
        use grammers_client::tl::functions::contacts::{Block, Unblock};
        let id: enums::InputPeer = self.peer(id).await?.into();
        ensure!(
            id != enums::InputPeer::PeerSelf,
            "Свой аккаунт заблокировать нельзя"
        );
        let confirmed = if blocked {
            self.client
                .invoke(&Block {
                    my_stories_from: false,
                    id,
                })
                .await?
        } else {
            self.client
                .invoke(&Unblock {
                    my_stories_from: false,
                    id,
                })
                .await?
        };
        ensure!(confirmed, "Telegram не подтвердил изменение блокировки");
        self.invalidate_blocked();
        Ok(())
    }

    async fn default_mute_until(&self, kind: ChatKind) -> Result<i32> {
        let peer = match notification_category(kind) {
            ChatKind::Group => enums::InputNotifyPeer::InputNotifyChats,
            ChatKind::Broadcast => enums::InputNotifyPeer::InputNotifyBroadcasts,
            _ => enums::InputNotifyPeer::InputNotifyUsers,
        };
        let enums::PeerNotifySettings::Settings(settings) = self
            .client
            .invoke(&grammers_client::tl::functions::account::GetNotifySettings { peer })
            .await?;
        Ok(settings.mute_until.unwrap_or_default())
    }

    pub async fn set_muted(&self, chat: i64, muted: bool) -> Result<()> {
        let peer = self.peer(chat).await?;
        let notify_peer =
            enums::InputNotifyPeer::from(types::InputNotifyPeer { peer: peer.into() });
        let enums::PeerNotifySettings::Settings(settings) = self
            .client
            .invoke(
                &grammers_client::tl::functions::account::GetNotifySettings {
                    peer: notify_peer.clone(),
                },
            )
            .await?;
        let updated = self
            .client
            .invoke(
                &grammers_client::tl::functions::account::UpdateNotifySettings {
                    peer: notify_peer,
                    settings: notification_settings(settings, muted).into(),
                },
            )
            .await?;
        ensure!(updated, "Telegram не подтвердил изменение уведомлений");
        Ok(())
    }

    pub async fn set_chat_pinned(
        &self,
        chat: i64,
        pinned: bool,
        folder: Option<i32>,
    ) -> Result<()> {
        let _settings = self.dialog_settings.lock().await;
        let reference = self.peer(chat).await?;
        let accepted = if let Some(id) = folder {
            // Read the complete current filter; never rebuild it from the smaller UI model.
            let enums::messages::DialogFilters::Filters(result) = self
                .client
                .invoke(&grammers_client::tl::functions::messages::GetDialogFilters {})
                .await?;
            let mut filter = result
                .filters
                .into_iter()
                .find(|filter| match filter {
                    enums::DialogFilter::Filter(filter) => filter.id == id,
                    enums::DialogFilter::Chatlist(filter) => filter.id == id,
                    enums::DialogFilter::Default => false,
                })
                .context("Папка больше не существует. Обновите список папок")?;
            let self_id = self.self_id().await?;
            set_folder_pin(&mut filter, reference.into(), pinned, self_id)?;
            self.client
                .invoke(
                    &grammers_client::tl::functions::messages::UpdateDialogFilter {
                        id,
                        filter: Some(filter),
                    },
                )
                .await?
        } else {
            self.client
                .invoke(&grammers_client::tl::functions::messages::ToggleDialogPin {
                    pinned,
                    peer: grammers_client::tl::types::InputDialogPeer {
                        peer: reference.into(),
                    }
                    .into(),
                })
                .await?
        };
        ensure!(accepted, "Telegram не подтвердил изменение закрепления");
        self.invalidate_views();
        Ok(())
    }

    pub async fn folders(&self) -> Result<Vec<ChatFolder>> {
        let self_id = self.self_id().await?;
        let enums::messages::DialogFilters::Filters(result) = self
            .client
            .invoke(&grammers_client::tl::functions::messages::GetDialogFilters {})
            .await?;
        Ok(result
            .filters
            .into_iter()
            .filter_map(|folder| folder_view(folder, self_id))
            .collect())
    }

    pub async fn set_archived(&self, chat: i64, archived: bool) -> Result<()> {
        let _settings = self.dialog_settings.lock().await;
        let peer = self.peer(chat).await?;
        self.client
            .invoke(&grammers_client::tl::functions::folders::EditPeerFolders {
                folder_peers: vec![
                    types::InputFolderPeer {
                        peer: peer.into(),
                        folder_id: i32::from(archived),
                    }
                    .into(),
                ],
            })
            .await?;
        Ok(())
    }

    async fn self_id(&self) -> Result<Option<PeerId>> {
        Ok(self
            .session
            .peer(PeerId::self_user())
            .await?
            .map(|peer| peer.id()))
    }

    async fn profile(&self) -> Option<Arc<Peer>> {
        if let Some(peer) = self.profile.read().await.as_ref() {
            return Some(peer.clone());
        }
        let mut cached = self.profile.write().await;
        if cached.is_none() {
            let user = tokio::time::timeout(Duration::from_secs(5), self.client.get_me())
                .await
                .ok()?
                .ok()?;
            *cached = Some(Arc::new(Peer::User(user)));
        }
        cached.clone()
    }

    async fn remember_profile(&self, user: grammers_client::peer::User) {
        self.profile
            .write()
            .await
            .replace(Arc::new(Peer::User(user)));
    }

    pub async fn own_profile(&self) -> Result<OwnProfile> {
        let _edit = self.profile_edit.lock().await;
        self.read_own_profile().await
    }

    async fn read_own_profile(&self) -> Result<OwnProfile> {
        let enums::users::UserFull::Full(result) = self
            .client
            .invoke(&grammers_client::tl::functions::users::GetFullUser {
                id: enums::InputUser::UserSelf,
            })
            .await?;
        let enums::UserFull::Full(full) = result.full_user;
        let raw = result
            .users
            .into_iter()
            .find(|user| user.id() == full.id)
            .context("Профиль аккаунта недоступен")?;
        let user = grammers_client::peer::User::from_raw(&self.client, raw);
        let snapshot = own_profile_view(&user, full.about.unwrap_or_default())?;
        self.remember_profile(user).await;
        Ok(snapshot)
    }

    pub async fn edit_own_profile(
        &self,
        original: &OwnProfile,
        edit: ProfileEdit,
    ) -> Result<ProfileEditResult> {
        use crate::profile_edit::{Patch, Plan, prepare};
        use grammers_client::tl::functions::account::{UpdateProfile, UpdateUsername};

        let _edit = self.profile_edit.lock().await;
        let mut current = self.read_own_profile().await?;
        let patch = match prepare(original, &current, &edit)? {
            Plan::Current => return Ok(ProfileEditResult::Saved(current)),
            Plan::Conflict => return Ok(ProfileEditResult::Conflict(current)),
            Plan::Write(patch) => patch,
        };
        let (updated, verify_about) = match patch {
            Patch::Details {
                first_name,
                last_name,
                about,
            } => {
                let updated = self
                    .client
                    .invoke(&UpdateProfile {
                        first_name,
                        last_name,
                        about: about.clone(),
                    })
                    .await
                    .map_err(profile_rpc_error)?;
                let check = about.is_some();
                if let Some(about) = about {
                    current.details.about = about;
                }
                (updated, check)
            }
            Patch::Username(username) => {
                match self.client.invoke(&UpdateUsername { username }).await {
                    Ok(user) => (user, false),
                    Err(error) if error.is("USERNAME_NOT_MODIFIED") => {
                        return self.read_own_profile().await.map(ProfileEditResult::Saved);
                    }
                    Err(error) => return Err(profile_rpc_error(error)),
                }
            }
        };
        let user = grammers_client::peer::User::from_raw(&self.client, updated);
        let next = own_profile_view(&user, current.details.about)?;
        ensure!(next.id == current.id, "Telegram вернул другой аккаунт");
        self.remember_profile(user).await;
        self.invalidate_views();
        if verify_about {
            return Ok(
                match tokio::time::timeout(Duration::from_secs(6), self.read_own_profile()).await {
                    Ok(Ok(current)) => ProfileEditResult::Saved(current),
                    _ => ProfileEditResult::Confirmed(next),
                },
            );
        }
        Ok(ProfileEditResult::Saved(next))
    }

    pub async fn set_own_photo(
        &self,
        original: &OwnProfile,
        jpeg: Arc<[u8]>,
    ) -> Result<ProfileEditResult> {
        use grammers_client::tl::functions::photos::UploadProfilePhoto;

        let photo_changed = |current: &OwnProfile| {
            ensure!(current.id == original.id, "Telegram вернул другой аккаунт");
            Ok::<_, anyhow::Error>(
                current.avatar.map(|photo| photo.photo) != original.avatar.map(|photo| photo.photo),
            )
        };
        let _edit = self.profile_edit.lock().await;
        let current = self.read_own_profile().await?;
        if photo_changed(&current)? {
            return Ok(ProfileEditResult::Conflict(current));
        }
        let size = jpeg.len();
        let mut stream = std::io::Cursor::new(jpeg);
        let upload = self
            .client
            .upload_stream(&mut stream, size, "profile.jpg".into())
            .await?;
        let mut current = self.read_own_profile().await?;
        if photo_changed(&current)? {
            return Ok(ProfileEditResult::Conflict(current));
        }
        let enums::photos::Photo::Photo(result) = self
            .client
            .invoke(&UploadProfilePhoto {
                fallback: false,
                bot: None,
                file: Some(upload.raw),
                video: None,
                video_start_ts: None,
                video_emoji_markup: None,
            })
            .await?;
        self.invalidate_views();

        if let enums::Photo::Photo(photo) = result.photo {
            let avatar = Avatar {
                peer: current.id,
                photo: photo.id,
                dc: photo.dc_id,
            };
            current.avatar = Some(avatar);
            if let Some(raw) = result
                .users
                .into_iter()
                .find(|user| user.id() == current.id)
            {
                let user = grammers_client::peer::User::from_raw(&self.client, raw);
                if let Ok(next) = own_profile_view(&user, current.details.about.clone())
                    && next.avatar.is_some_and(|value| value.photo == avatar.photo)
                {
                    self.remember_profile(user).await;
                    return Ok(ProfileEditResult::Saved(next));
                }
            }
        }
        Ok(
            match tokio::time::timeout(Duration::from_secs(6), self.read_own_profile()).await {
                Ok(Ok(next)) => ProfileEditResult::Saved(next),
                _ => ProfileEditResult::Confirmed(current),
            },
        )
    }

    pub async fn search(self: &Arc<Self>, query: &str, chat: Option<i64>) -> Result<SearchHandle> {
        let messages = match chat {
            Some(id) => SearchMessages::Chat(
                self.client
                    .search_messages(self.peer(id).await?)
                    .query(query),
            ),
            None => SearchMessages::Global(self.client.search_all_messages().query(query)),
        };
        let (requests, mut receiver) =
            mpsc::channel::<tokio::sync::oneshot::Sender<Result<SearchPage>>>(1);
        let client = self.clone();
        tokio::spawn(async move {
            let mut messages = messages;
            let mut buffer = SearchBuffer::default();
            let mut pending = None;
            while let Some(mut response) = receiver.recv().await {
                let result = tokio::select! {
                    _ = response.closed() => break,
                    result = client.search_page(&mut messages, &mut buffer, &mut pending) => result,
                };
                let finished = result.as_ref().is_ok_and(|page| !page.has_more);
                if response.send(result).is_err() || finished {
                    break;
                }
            }
        });
        Ok(SearchHandle { requests })
    }

    async fn search_page(
        &self,
        messages: &mut SearchMessages,
        buffer: &mut SearchBuffer,
        pending: &mut Option<TelegramMessage>,
    ) -> Result<SearchPage> {
        let self_id = self.self_id().await?;
        let profile = self.profile().await;
        let profile = profile.as_deref();
        while buffer.needs_more() {
            if pending.is_none() {
                *pending = messages.next().await?;
            }
            let Some(message) = pending.as_ref() else {
                buffer.exhausted = true;
                break;
            };
            let Some(id) = message.peer_id().bot_api_dialog_id() else {
                pending.take();
                continue;
            };
            let Some(peer) = message.peer() else {
                pending.take();
                continue;
            };
            let Some(peer_ref) = message
                .peer_ref()
                .await
                .map_err(anyhow::Error::from_boxed)?
            else {
                pending.take();
                continue;
            };
            self.peers.write().await.insert(id, peer_ref);
            self.remember_chat_author(peer);
            buffer.push(SearchHit {
                chat: Chat {
                    id,
                    pinned: false,
                    last_activity: 0,
                    title: match peer {
                        Peer::User(user) if user.is_self() => "Избранное",
                        _ => peer
                            .name()
                            .filter(|name| !name.is_empty())
                            .unwrap_or("Без названия"),
                    }
                    .to_owned(),
                    preview: String::new(),
                    preview_sender: None,
                    preview_image: None,
                    group: matches!(peer, Peer::Group(_) | Peer::Community(_)),
                    forum: forum_topics::is_forum(peer),
                    unread: 0,
                    unread_mark: false,
                    last_message_id: 0,
                    broadcast: matches!(peer, Peer::Channel(_)),
                    can_write: can_write_chat(peer),
                    slow_mode_enabled: group_permissions::slow_mode_enabled(peer),
                    saved_messages: Some(peer_ref.id) == self_id
                        || matches!(peer, Peer::User(user) if user.is_self()),
                    avatar: peer_avatar(peer),
                    archived: false,
                    kind: chat_kind(peer),
                    muted: false,
                },
                message: self.message_view(message, self_id, profile),
            });
            pending.take();
        }
        Ok(buffer.take_page())
    }

    pub async fn history(
        &self,
        id: i64,
        before: Option<i32>,
        read: Option<ReadRequest>,
    ) -> Result<(Vec<Message>, bool, bool)> {
        let peer = self.peer(id).await?;
        let self_id = self.self_id().await?;
        let profile = self.profile().await;
        let profile = profile.as_deref();
        let mut iter = self.client.iter_messages(peer);
        if let Some(before) = before {
            iter = iter.offset_id(before);
        }
        let mut page: Vec<TelegramMessage> = Vec::new();
        let mut has_older = false;
        let mut cursor = before;
        while let Some(message) = iter.next().await? {
            if matches!(message.raw, enums::Message::Empty(_))
                || cursor.is_some_and(|id| message.id() >= id)
            {
                continue;
            }
            cursor = Some(message.id());
            // Read one message past the page and keep its boundary album together.
            let album = message.grouped_id();
            if page.len() >= HISTORY_PAGE_SIZE
                && (album.is_none()
                    || page.last().and_then(TelegramMessage::grouped_id) != album
                    || page.len() >= HISTORY_PAGE_SIZE + MAX_ALBUM_ITEMS - 1)
            {
                has_older = true;
                break;
            }
            page.push(message);
        }
        let marked = if let Some(read) = read {
            self.mark_read_through(id, page.first().map_or(0, TelegramMessage::id), read)
                .await
                .unwrap_or(false)
        } else {
            false
        };
        self.resolve_chat_authors(peer, page.iter()).await;
        let mut result: Vec<_> = page
            .iter()
            .rev()
            .map(|message| self.message_view(message, self_id, profile))
            .collect();
        self.load_reply_previews(peer, &mut result, self_id, profile)
            .await;
        Ok((result, has_older, marked))
    }

    fn message_author(
        &self,
        message: &TelegramMessage,
        profile: Option<&Peer>,
    ) -> Option<ChatAuthor> {
        if let Some(id) = message_chat_author(message) {
            return incoming_author_peer(message, id)
                .and_then(peer_author)
                .or_else(|| self.chat_authors.lock().unwrap().get(&id).cloned());
        }
        let sender = message.sender_id();
        let own = profile.filter(|profile| {
            sender == Some(profile.id())
                || sender == Some(PeerId::self_user())
                || (sender.is_none() && message.outgoing())
        });
        own.or_else(|| message.sender()).and_then(peer_author)
    }

    fn message_view(
        &self,
        message: &TelegramMessage,
        self_id: Option<PeerId>,
        profile: Option<&Peer>,
    ) -> Message {
        self.remember_sticker_message(message);
        let chat_author = message_chat_author(message);
        let author = self.message_author(message, profile);
        message_view(message, self_id, profile, chat_author, author)
    }

    fn remember_chat_author(&self, peer: &Peer) {
        if peer.id().kind() != PeerKind::User
            && let Some(author) = peer_author(peer)
        {
            self.chat_authors.lock().unwrap().insert(peer.id(), author);
        }
    }

    async fn resolve_chat_authors<'a>(
        &self,
        peer: PeerRef,
        messages: impl IntoIterator<Item = &'a TelegramMessage>,
    ) {
        let mut channels = HashMap::new();
        let mut groups = HashSet::new();
        {
            let mut authors = self.chat_authors.lock().unwrap();
            for message in messages {
                let Some(id) = message_chat_author(message) else {
                    continue;
                };
                if let Some(author) = incoming_author_peer(message, id).and_then(peer_author) {
                    authors.insert(id, author);
                    continue;
                }
                if authors.contains_key(&id) {
                    continue;
                }
                let Some(bare_id) = id.bare_id() else {
                    continue;
                };
                match id.kind() {
                    PeerKind::Channel => {
                        channels.entry(id).or_insert_with(|| {
                            if id == peer.id {
                                peer.into()
                            } else {
                                types::InputChannelFromMessage {
                                    peer: peer.into(),
                                    msg_id: message.id(),
                                    channel_id: bare_id,
                                }
                                .into()
                            }
                        });
                    }
                    PeerKind::Chat => {
                        groups.insert(bare_id);
                    }
                    PeerKind::User => {}
                }
            }
            channels.retain(|id, _| !authors.contains_key(id));
            groups.retain(|id| PeerId::chat(*id).is_some_and(|id| !authors.contains_key(&id)));
        }
        if !channels.is_empty() {
            let channels = grammers_client::tl::functions::channels::GetChannels {
                id: channels.into_values().collect(),
            };
            if let Ok(Ok(chats)) =
                tokio::time::timeout(Duration::from_secs(2), self.client.invoke(&channels)).await
            {
                self.remember_chat_authors(chats).await;
            }
        }
        if !groups.is_empty() {
            let groups = grammers_client::tl::functions::messages::GetChats {
                id: groups.into_iter().collect(),
            };
            if let Ok(Ok(chats)) =
                tokio::time::timeout(Duration::from_secs(2), self.client.invoke(&groups)).await
            {
                self.remember_chat_authors(chats).await;
            }
        }
    }

    async fn remember_chat_authors(&self, chats: enums::messages::Chats) {
        let chats = match chats {
            enums::messages::Chats::Chats(chats) => chats.chats,
            enums::messages::Chats::Slice(chats) => chats.chats,
        };
        for chat in chats {
            let peer = Peer::from_raw(&self.client, chat);
            self.remember_chat_author(&peer);
            if let Some(id) = peer.id().bot_api_dialog_id()
                && let Ok(Some(reference)) = peer.to_ref().await
            {
                self.peers.write().await.insert(id, reference);
            }
        }
    }

    pub fn read_revision(&self, chat: i64) -> u64 {
        self.read_state.revision(chat)
    }

    pub fn begin_read_change(&self, chat: i64) -> u64 {
        self.read_state.advance(chat)
    }

    async fn mark_dialog_unread(&self, chat: i64, unread: bool) -> Result<()> {
        let peer = self.peer(chat).await?;
        let accepted = self
            .client
            .invoke(
                &grammers_client::tl::functions::messages::MarkDialogUnread {
                    unread,
                    parent_peer: None,
                    peer: types::InputDialogPeer { peer: peer.into() }.into(),
                },
            )
            .await?;
        ensure!(accepted, "Telegram не подтвердил отметку чата");
        Ok(())
    }

    pub async fn set_dialog_unread(
        &self,
        chat: i64,
        unread: bool,
        through: i32,
        revision: u64,
    ) -> Result<bool> {
        let result = self
            .read_state
            .run(chat, revision, async {
                // A failed response can still follow an accepted server-side marker.
                if unread {
                    self.read_state.set_may_be_marked(chat, true);
                }
                self.mark_dialog_unread(chat, unread).await?;
                if !unread {
                    self.read_state.set_may_be_marked(chat, false);
                    self.read_through(chat, through).await?;
                }
                Ok(())
            })
            .await;
        // A failure may follow a successful first RPC; always reconcile with Telegram.
        self.invalidate_views();
        result.map(|result| result.is_some())
    }

    pub async fn mark_read_through(
        &self,
        chat: i64,
        max_id: i32,
        read: ReadRequest,
    ) -> Result<bool> {
        let result = self
            .read_state
            .run(chat, read.revision, async {
                self.read_through(chat, max_id).await?;
                // Inspect this after acquiring the queue: an older manual RPC may
                // have completed since the user reopened the chat.
                let clear = read.clear_mark || self.read_state.may_be_marked(chat);
                if clear {
                    self.mark_dialog_unread(chat, false).await?;
                    self.read_state.set_may_be_marked(chat, false);
                }
                Ok(clear)
            })
            .await?;
        if result == Some(true) {
            self.invalidate_views();
        }
        Ok(result.is_some())
    }

    async fn read_through(&self, chat: i64, max_id: i32) -> Result<()> {
        if max_id <= 0 {
            return Ok(());
        }
        let peer = self.peer(chat).await?;
        if peer.id.kind() == PeerKind::Channel {
            let accepted = self
                .client
                .invoke(&grammers_client::tl::functions::channels::ReadHistory {
                    channel: peer.into(),
                    max_id,
                })
                .await?;
            ensure!(accepted, "Telegram не подтвердил чтение истории");
        } else {
            self.client
                .invoke(&grammers_client::tl::functions::messages::ReadHistory {
                    peer: peer.into(),
                    max_id,
                })
                .await?;
        }
        Ok(())
    }

    pub async fn history_around(&self, chat: i64, anchor: i32) -> Result<(Vec<Message>, bool)> {
        let peer = self.peer(chat).await?;
        let self_id = self.self_id().await?;
        let profile = self.profile().await;
        let profile = profile.as_deref();
        let original = self
            .message_by_id(peer, anchor)
            .await?
            .context("Сообщение больше недоступно")?;
        let (mut messages, older, _) = self.history(chat, Some(anchor), None).await?;
        let mut additional = vec![original];
        let mut newer = self
            .client
            .iter_messages(peer)
            .offset_id(anchor)
            .reverse(true)
            .limit(HISTORY_PAGE_SIZE / 2);
        while let Some(message) = newer.next().await? {
            if message.id() > anchor {
                additional.push(message);
            }
        }
        self.resolve_chat_authors(peer, additional.iter()).await;
        messages.extend(
            additional
                .iter()
                .map(|message| self.message_view(message, self_id, profile)),
        );
        self.load_reply_previews(peer, &mut messages, self_id, profile)
            .await;
        Ok((messages, older))
    }

    async fn load_reply_previews(
        &self,
        peer: PeerRef,
        messages: &mut [Message],
        self_id: Option<PeerId>,
        profile: Option<&Peer>,
    ) {
        let mut ids = messages
            .iter()
            .filter_map(|message| message.reply_to)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        ids.retain(|id| !messages.iter().any(|message| message.id == *id));
        if ids.is_empty() {
            return;
        }
        let Ok(Ok(originals)) = tokio::time::timeout(
            Duration::from_secs(3),
            self.client.get_messages_by_id(peer, &ids),
        )
        .await
        else {
            return;
        };
        self.resolve_chat_authors(peer, originals.iter().flatten())
            .await;
        let previews: HashMap<_, _> = originals.into_iter().flatten()
            .filter(|message| matches!(&message.raw, enums::Message::Message(raw) if raw.ttl_period.is_none()))
            .map(|message| {
                let original = self.message_view(&message, self_id, profile);
                (original.id, original.reply_preview())
            }).collect();
        for message in messages {
            if let Some(original) = message.reply_to.and_then(|id| previews.get(&id)) {
                let preview = message
                    .reply_preview
                    .get_or_insert_with(|| original.clone());
                preview.sender.clone_from(&original.sender);
                preview.self_authored = original.self_authored;
            }
        }
    }

    pub async fn send(
        &self,
        id: i64,
        text: FormattedText,
        uploads: Vec<Upload>,
        reply_to: Option<i32>,
        topic: Option<i32>,
    ) -> Result<Vec<Message>> {
        let peer = self.peer(id).await?;
        let self_id = self.self_id().await?;
        let profile = self.profile().await;
        let profile = profile.as_deref();
        let entities = outgoing_entities(&text);
        if uploads.len() > 1 {
            ensure!(
                uploads.len() <= MAX_ALBUM_ITEMS,
                "В альбоме может быть до 10 вложений"
            );
            let kind = uploads[0].kind;
            ensure!(
                uploads.iter().all(|upload| upload.kind == kind),
                "Выберите фотографии или документы для одного альбома"
            );
            let mut album = Vec::with_capacity(uploads.len());
            for (index, upload) in uploads.into_iter().enumerate() {
                let uploaded = self.upload_file(upload).await?;
                let mut media = InputMedia::new();
                if index == 0 {
                    media = media
                        .caption(&text.text)
                        .fmt_entities(entities.iter().cloned())
                        .reply_to(reply_to)
                        .forum_topic(topic);
                }
                album.push(match kind {
                    UploadKind::File => media.file(uploaded),
                    UploadKind::Photo => media.photo(uploaded),
                });
            }
            let messages = self.client.send_album(peer, album).await?;
            return Ok(messages
                .into_iter()
                .flatten()
                .map(|message| self.message_view(&message, self_id, profile))
                .collect());
        }

        let mut message = InputMessage::new()
            .text(text.text)
            .fmt_entities(entities)
            .reply_to(reply_to)
            .forum_topic(topic);
        if let Some(upload) = uploads.into_iter().next() {
            let kind = upload.kind;
            let uploaded = self.upload_file(upload).await?;
            message = match kind {
                UploadKind::File => message.file(uploaded),
                UploadKind::Photo => message.photo(uploaded),
            };
        }
        let message = self.client.send_message(peer, message).await?;
        Ok(vec![self.message_view(&message, self_id, profile)])
    }

    pub async fn send_poll(
        &self,
        chat: i64,
        draft: PollDraft,
        random_id: i64,
        topic: Option<i32>,
    ) -> Result<Option<Message>> {
        let poll = draft.validate()?;
        let max_seconds = self.poll_close_limit(poll.deadline.is_some()).await;
        let (close_period, close_date) =
            poll.close_fields(crate::model::unix_now(), max_seconds)?;
        let media = poll_media(poll, random_id, close_period, close_date);
        let peer = self.peer(chat).await?;
        let self_id = self.self_id().await?;
        let profile = self.profile().await;
        let message = self
            .client
            .send_message(
                peer,
                InputMessage::new()
                    .media(media)
                    .random_id(random_id)
                    .forum_topic(topic),
            )
            .await;
        match message {
            Ok(message) => Ok(Some(self.message_view(
                &message,
                self_id,
                profile.as_deref(),
            ))),
            Err(error) if error.is("RANDOM_ID_DUPLICATE") => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn vote(
        &self,
        chat: i64,
        id: i32,
        expected: i64,
        options: Vec<Vec<u8>>,
    ) -> Result<Option<PollUpdate>> {
        use grammers_client::tl::functions::messages::SendVote;
        let peer = self.peer(chat).await?;
        let message = self
            .message_by_id(peer, id)
            .await?
            .context("Опрос больше недоступен")?;
        let Some(Media::Poll(raw)) = message.media() else {
            bail!("Опрос больше недоступен")
        };
        let poll = poll_timing::poll_view_at(&raw, &message);
        ensure!(
            poll.id == expected && poll.accepts(&options),
            "Этот голос больше недоступен"
        );
        let response = self
            .client
            .invoke(&SendVote {
                peer: peer.into(),
                msg_id: id,
                options,
            })
            .await?;
        let updates = match response {
            enums::Updates::Updates(updates) => updates.updates,
            enums::Updates::Combined(updates) => updates.updates,
            enums::Updates::UpdateShort(update) => vec![update.update],
            _ => Vec::new(),
        };
        for item in updates {
            if let enums::Update::MessagePoll(raw) = item
                && raw.poll_id == expected
            {
                let mut update = poll_update(&raw);
                if !raw_results(&raw.results).min
                    && update.results.is_some()
                    && update.target.is_none_or(|target| target == (chat, id))
                {
                    update.target = Some((chat, id));
                    update.partial = false;
                    return Ok(Some(update));
                }
            }
        }
        let refreshed =
            tokio::time::timeout(Duration::from_secs(6), self.message_by_id(peer, id)).await;
        Ok(match refreshed {
            Ok(Ok(Some(message))) => match message.media() {
                Some(Media::Poll(raw)) if raw.raw.id == expected => Some(poll_results(
                    expected,
                    Some((chat, id)),
                    Some(poll_timing::poll_view_at(&raw, &message)),
                    &raw.raw_results,
                )),
                _ => None,
            },
            _ => None,
        })
    }

    async fn upload_file(&self, upload: Upload) -> Result<Uploaded> {
        use tokio::io::AsyncReadExt;

        let metadata = tokio::fs::metadata(&upload.path)
            .await
            .context("Не удалось прочитать выбранный файл")?;
        ensure!(metadata.is_file(), "Выберите обычный файл");
        let file = tokio::fs::File::open(&upload.path).await?;
        let metadata = file.metadata().await?;
        ensure!(metadata.is_file(), "Выберите обычный файл");
        ensure!(metadata.len() <= 4 * 1024_u64.pow(3), "Файл больше 4 ГиБ");
        let size = usize::try_from(metadata.len()).context("Файл слишком большой")?;
        ensure!(size > 0, "Нельзя отправить пустой файл");
        let mut stream = file.take(metadata.len());
        let uploaded = self
            .client
            .upload_stream(&mut stream, size, upload.name)
            .await
            .context("Не удалось загрузить файл в Telegram")?;
        ensure!(
            stream.limit() == 0,
            "Размер файла изменился во время отправки"
        );
        Ok(uploaded)
    }

    pub async fn save(&self, chat: i64, id: i32, destination: PathBuf) -> Result<()> {
        let mut output = PendingSave::new(destination)?;
        let peer = self.peer(chat).await?;
        let mut media = self.attachment_media(peer, id).await?;
        let identity = attachment_id(&media);
        let mut retried = false;
        let mut download = self.client.iter_download(&media);
        loop {
            match download.next().await {
                Ok(Some(chunk)) => output.write(&chunk).await?,
                Ok(None) => return output.finish(media.size()).await,
                Err(error) if !retried && expired_file_reference(&error) => {
                    media = self.attachment_media(peer, id).await?;
                    ensure!(
                        attachment_id(&media) == identity,
                        "Вложение в сообщении изменилось"
                    );
                    output.restart().await?;
                    download = self.client.iter_download(&media);
                    retried = true;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub async fn playback_preview(
        self: &Arc<Self>,
        chat: i64,
        id: i32,
        progress: watch::Sender<PlaybackProgress>,
        whole: bool,
    ) -> Result<PlaybackSource> {
        self.playback_preview_mode(chat, id, progress, whole, false)
            .await
    }

    pub async fn sticker_video_preview(
        self: &Arc<Self>,
        chat: i64,
        id: i32,
        progress: watch::Sender<PlaybackProgress>,
        whole: bool,
    ) -> Result<PlaybackSource> {
        self.playback_preview_mode(chat, id, progress, whole, true)
            .await
    }

    async fn playback_preview_mode(
        self: &Arc<Self>,
        chat: i64,
        id: i32,
        progress: watch::Sender<PlaybackProgress>,
        whole: bool,
        sticker: bool,
    ) -> Result<PlaybackSource> {
        let peer = self.peer(chat).await?;
        let mut media = self.attachment_media(peer, id).await?;
        let kind = playable_media(&media).context("Это вложение нельзя воспроизвести")?;
        if sticker {
            ensure!(
                matches!(&media, Media::Sticker(_)) && matches!(kind, PlayableMedia::Video { .. }),
                "Видеостикер в сообщении изменился"
            );
        }
        let identity = attachment_id(&media);
        let expected = media.size().context("Неизвестен размер медиа")? as u64;
        if !whole && kind != PlayableMedia::Voice {
            let telegram = self.clone();
            let cached = Arc::new(Mutex::new(media));
            let stream = crate::media_stream::StreamSource::new(expected, move |index| {
                let telegram = telegram.clone();
                let cached = cached.clone();
                Box::pin(async move { telegram.playback_chunk(peer, id, index, &cached).await })
            })?;
            return Ok(PlaybackSource::Stream(kind, stream));
        }

        let mut output = PlaybackDownload::new(expected)?;
        let mut received = 0;
        let mut retried = false;
        progress.send_replace(PlaybackProgress {
            received,
            total: expected,
        });
        let mut download = self.client.iter_download(&media);
        loop {
            match download.next().await {
                Ok(Some(chunk)) => {
                    output.write(&chunk).await?;
                    received += chunk.len() as u64;
                    progress.send_replace(PlaybackProgress {
                        received,
                        total: expected,
                    });
                }
                Ok(None) => return Ok(PlaybackSource::File(output.finish(kind).await?)),
                Err(error) if !retried && expired_file_reference(&error) => {
                    media = self.attachment_media(peer, id).await?;
                    ensure!(
                        attachment_id(&media) == identity
                            && playable_media(&media) == Some(kind)
                            && media.size().map(|size| size as u64) == Some(expected),
                        "Медиа в сообщении изменилось"
                    );
                    output.restart().await?;
                    received = 0;
                    progress.send_replace(PlaybackProgress {
                        received,
                        total: expected,
                    });
                    download = self.client.iter_download(&media);
                    retried = true;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn playback_chunk(
        &self,
        peer: PeerRef,
        id: i32,
        index: i32,
        cached: &Mutex<Media>,
    ) -> Result<bytes::Bytes> {
        let mut media = cached.lock().await;
        for attempt in 0..2 {
            let mut chunk = self
                .client
                .iter_download(&*media)
                .chunk_size(crate::media_stream::BLOCK_SIZE)
                .skip_chunks(index);
            match chunk.next().await {
                Ok(Some(bytes)) => return Ok(bytes.into()),
                Ok(None) => bail!("Фрагмент вложения недоступен"),
                Err(error) if attempt == 0 && expired_file_reference(&error) => {
                    let refreshed = self.attachment_media(peer, id).await?;
                    ensure!(
                        attachment_id(&refreshed) == attachment_id(&media)
                            && refreshed.size() == media.size()
                            && playable_media(&refreshed) == playable_media(&media),
                        "Медиа в сообщении изменилось"
                    );
                    *media = refreshed;
                }
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!()
    }

    pub async fn photo_preview(&self, chat: i64, id: i32) -> Result<Vec<u8>> {
        let peer = self.peer(chat).await?;
        let mut previous = None;
        for attempt in 0..2 {
            let message = self
                .message_by_id(peer, id)
                .await?
                .context("Сообщение больше недоступно")?;
            let Some((Media::Photo(photo), _)) = attachment(&message) else {
                bail!("Фотография больше недоступна");
            };
            let identity = photo.id();
            ensure!(
                previous.is_none_or(|previous| previous == identity),
                "Фотография в сообщении изменилась"
            );
            previous = Some(identity);
            let thumbnail = photo
                .thumbs()
                .into_iter()
                .filter(|thumb| match thumb {
                    PhotoSize::Size(size) => {
                        (1..=1600).contains(&size.width) && (1..=1600).contains(&size.height)
                    }
                    PhotoSize::Progressive(size) => {
                        (1..=1600).contains(&size.width) && (1..=1600).contains(&size.height)
                    }
                    PhotoSize::Cached(size) => {
                        (1..=1600).contains(&size.width) && (1..=1600).contains(&size.height)
                    }
                    _ => false,
                })
                .filter(|thumb| (1..=4 * 1024 * 1024).contains(&thumb.size()))
                .max_by_key(PhotoSize::size)
                .context("Нет доступного размера для предпросмотра")?;
            let expected = thumbnail.size();
            let mut download = self.client.iter_download(&PhotoPreview {
                thumbnail,
                dc: photo.dc_id(),
            });
            let mut bytes = Vec::with_capacity(expected);
            loop {
                match download.next().await {
                    Ok(Some(chunk)) => {
                        ensure!(
                            bytes.len() + chunk.len() <= expected,
                            "Не удалось проверить размер фотографии"
                        );
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(None) => {
                        ensure!(bytes.len() == expected, "Фотография загружена не полностью");
                        return Ok(bytes);
                    }
                    Err(error) if attempt == 0 && expired_file_reference(&error) => break,
                    Err(error) => return Err(error.into()),
                }
            }
        }
        unreachable!("the second download either succeeds or returns an error")
    }

    pub async fn chat_image(&self, image: ChatImage) -> Result<Vec<u8>> {
        let _permit = self.image_downloads.acquire().await?;
        tokio::time::timeout(Duration::from_secs(15), self.fetch_chat_image(image))
            .await
            .context("Не удалось загрузить изображение вовремя")?
    }

    async fn fetch_chat_image(&self, image: ChatImage) -> Result<Vec<u8>> {
        match image {
            ChatImage::Sticker(id) => self.sticker_image(id).await,
            ChatImage::Avatar(avatar) => {
                let id = PeerId::from_bot_api_dialog_id(avatar.peer).context("Чат не найден")?;
                let cached = self.peers.read().await.get(&avatar.peer).copied();
                let peer = if Some(id) == self.self_id().await? {
                    PeerRef {
                        id: PeerId::self_user(),
                        auth: Default::default(),
                    }
                } else if let Some(peer) = cached {
                    peer
                } else {
                    let peer = self.session.peer(id).await?.context("Чат не найден")?;
                    PeerRef {
                        id,
                        auth: peer.auth().context("Фото профиля недоступно")?,
                    }
                };
                let location = types::InputPeerPhotoFileLocation {
                    big: false,
                    peer: peer.into(),
                    photo_id: avatar.photo,
                };
                self.load_image(&PeerPhoto {
                    location,
                    dc: avatar.dc,
                })
                .await
            }
            ChatImage::Attachment {
                chat,
                message,
                media,
            } => {
                let peer = self.peer(chat).await?;
                let message = self
                    .message_by_id(peer, message)
                    .await?
                    .context("Сообщение больше недоступно")?;
                let (attachment, _) = attachment(&message).context("Вложение больше недоступно")?;
                ensure!(
                    attachment_id(&attachment) == Some(media),
                    "Вложение в сообщении изменилось"
                );
                let (sizes, dc) = match attachment {
                    Media::Photo(photo) if !photo.is_spoiler() => (photo.thumbs(), photo.dc_id()),
                    Media::Document(document) if !document.is_spoiler() => {
                        (document.thumbs(), document.dc_id())
                    }
                    _ => bail!("Миниатюра недоступна"),
                };
                let thumbnail = pick_thumbnail(sizes).context("Миниатюра недоступна")?;
                self.load_image(&PhotoPreview { thumbnail, dc }).await
            }
        }
    }

    async fn load_image(&self, source: &impl Downloadable) -> Result<Vec<u8>> {
        let mut download = self.client.iter_download(source);
        let mut bytes = Vec::new();
        while let Some(chunk) = download.next().await? {
            ensure!(
                bytes.len() + chunk.len() <= 512 * 1024,
                "Миниатюра слишком большая"
            );
            bytes.extend_from_slice(&chunk);
        }
        ensure!(
            !bytes.is_empty() && source.size().is_none_or(|size| bytes.len() == size),
            "Миниатюра загружена не полностью"
        );
        Ok(bytes)
    }

    pub async fn edit_text(&self, chat: i64, id: i32, text: FormattedText) -> Result<()> {
        let peer = self.peer(chat).await?;
        let message = self
            .message_by_id(peer, id)
            .await?
            .context("Сообщение больше недоступно")?;
        ensure!(can_edit_text(&message), "Это сообщение нельзя изменить");
        let link_preview = matches!(message.media(), Some(Media::WebPage(_)));
        let entities = outgoing_entities(&text);
        // Include even an empty vector when removing the last style.
        let edit = grammers_client::tl::functions::messages::EditMessage {
            no_webpage: !link_preview,
            invert_media: false,
            peer: peer.into(),
            id,
            message: Some(text.text),
            media: None,
            reply_markup: None,
            entities: Some(entities),
            schedule_date: None,
            schedule_repeat_period: None,
            quick_reply_shortcut_id: None,
            rich_message: None,
        };
        match self.client.invoke(&edit).await {
            Err(error) if error.is("MESSAGE_NOT_MODIFIED") => Ok(()),
            result => result.map(drop).map_err(Into::into),
        }
    }

    pub async fn delete_message(&self, chat: i64, id: i32) -> Result<()> {
        let peer = self.peer(chat).await?;
        let Some(message) = self.message_by_id(peer, id).await? else {
            return Ok(());
        };
        let self_id = self.self_id().await?;
        ensure!(
            can_delete(&message, self_id),
            "Это сообщение нельзя удалить"
        );
        self.client.delete_messages(peer, &[id]).await?;
        Ok(())
    }

    pub async fn set_message_pinned(&self, chat: i64, id: i32, pinned: bool) -> Result<()> {
        let peer = self.peer(chat).await?;
        if pinned {
            self.client.pin_message(peer, id).await?;
        } else {
            self.client.unpin_message(peer, id).await?;
        }
        Ok(())
    }

    pub async fn toggle_reaction(
        &self,
        chat: i64,
        id: i32,
        emoji: String,
    ) -> Result<Vec<Reaction>> {
        let peer = self.peer(chat).await?;
        let message = self
            .message_by_id(peer, id)
            .await?
            .context("Сообщение больше недоступно")?;
        let mut chosen = reaction_counts(&message)
            .iter()
            .filter_map(|enums::ReactionCount::Count(reaction)| {
                reaction
                    .chosen_order
                    .map(|order| (order, reaction.reaction.clone()))
            })
            .filter(|(_, reaction)| {
                !matches!(reaction, enums::Reaction::Paid | enums::Reaction::Empty)
            })
            .collect::<Vec<_>>();
        chosen.sort_by_key(|(order, _)| *order);
        let selected = enums::Reaction::Emoji(types::ReactionEmoji { emoticon: emoji });
        if let Some(index) = chosen
            .iter()
            .position(|(_, reaction)| *reaction == selected)
        {
            chosen.remove(index);
        } else {
            chosen.push((0, selected));
        }
        let reactions = chosen
            .into_iter()
            .map(|(_, reaction)| reaction)
            .collect::<Vec<_>>();
        self.client.send_reactions(peer, id, reactions).await?;
        let message = self
            .message_by_id(peer, id)
            .await?
            .context("Сообщение больше недоступно")?;
        Ok(message_reactions(&message))
    }

    pub async fn forward_message(
        &self,
        source: i64,
        id: i32,
        destination: crate::model::Conversation,
    ) -> Result<Message> {
        let self_id = self.self_id().await?;
        let profile = self.profile().await;
        let profile = profile.as_deref();
        let source = self.peer(source).await?;
        let destination_peer = self.peer(destination.chat).await?;
        let original = self
            .message_by_id(source, id)
            .await?
            .context("Сообщение больше недоступно")?;
        ensure!(can_forward(&original), "Это сообщение нельзя переслать");
        let message = self
            .client
            .forward_messages_to_topic(destination_peer, destination.topic, &[id], source)
            .await?
            .into_iter()
            .next()
            .flatten()
            .context("Telegram не вернул пересланное сообщение")?;
        Ok(self.message_view(&message, self_id, profile))
    }

    async fn message_by_id(&self, peer: PeerRef, id: i32) -> Result<Option<TelegramMessage>> {
        Ok(self
            .client
            .get_messages_by_id(peer, &[id])
            .await?
            .into_iter()
            .next()
            .flatten())
    }

    async fn attachment_media(&self, peer: PeerRef, id: i32) -> Result<Media> {
        let message = self
            .message_by_id(peer, id)
            .await?
            .context("Сообщение больше недоступно")?;
        attachment(&message)
            .map(|(media, _)| media)
            .context("Вложение больше недоступно")
    }
}

struct PhotoPreview {
    thumbnail: PhotoSize,
    dc: Option<i32>,
}

struct PeerPhoto {
    location: types::InputPeerPhotoFileLocation,
    dc: i32,
}

impl Downloadable for PeerPhoto {
    fn to_raw_input_location(&self) -> Option<enums::InputFileLocation> {
        Some(self.location.clone().into())
    }

    fn dc_id(&self) -> Option<i32> {
        Some(self.dc)
    }
}

impl Downloadable for PhotoPreview {
    fn to_raw_input_location(&self) -> Option<enums::InputFileLocation> {
        self.thumbnail.to_raw_input_location()
    }

    fn to_data(&self) -> Option<Vec<u8>> {
        self.thumbnail.to_data()
    }

    fn size(&self) -> Option<usize> {
        Some(self.thumbnail.size())
    }

    fn dc_id(&self) -> Option<i32> {
        self.dc
    }
}

fn attachment_id(media: &Media) -> Option<AttachmentThumbnail> {
    match media {
        Media::Photo(photo) => Some(AttachmentThumbnail::Photo(photo.id())),
        Media::Document(document) => Some(AttachmentThumbnail::Document(document.id())),
        Media::Sticker(sticker) => Some(AttachmentThumbnail::Document(sticker.document.id())),
        _ => None,
    }
}

fn device_sessions_view(raw: types::account::Authorizations) -> DeviceSessions {
    let mut sessions: Vec<_> = raw
        .authorizations
        .into_iter()
        .map(|raw| {
            let enums::Authorization::Authorization(raw) = raw;
            DeviceSession {
                id: raw.hash,
                current: raw.current,
                device: raw.device_model,
                platform: raw.platform,
                system_version: raw.system_version,
                application: raw.app_name,
                app_version: raw.app_version,
                created: raw.date_created.into(),
                active: raw.date_active.into(),
                ip: raw.ip,
                country: raw.country,
                region: raw.region,
                official: raw.official_app,
                password_pending: raw.password_pending,
                unconfirmed: raw.unconfirmed,
            }
        })
        .collect();
    sessions.sort_by_key(|session| {
        (
            !session.current,
            std::cmp::Reverse(session.active),
            session.id,
        )
    });
    let mut seen = std::collections::HashSet::new();
    sessions.retain(|session| seen.insert(session.id));
    DeviceSessions {
        sessions,
        inactive_ttl_days: raw.authorization_ttl_days,
    }
}

fn device_reset_request(
    snapshot: &DeviceSessions,
    id: i64,
) -> Result<Option<grammers_client::tl::functions::account::ResetAuthorization>> {
    ensure!(
        id != 0,
        "Текущий сеанс нельзя завершить из списка устройств"
    );
    ensure!(
        !snapshot
            .sessions
            .iter()
            .any(|session| session.id == id && session.current),
        "Текущий сеанс нельзя завершить из списка устройств"
    );
    Ok(snapshot
        .sessions
        .iter()
        .any(|session| session.id == id)
        .then_some(grammers_client::tl::functions::account::ResetAuthorization { hash: id }))
}

fn device_ttl_request(
    days: i32,
) -> Result<grammers_client::tl::functions::account::SetAuthorizationTtl> {
    ensure!(
        crate::model::SESSION_TTL_DAYS.contains(&days),
        "Выберите срок 7, 30, 90, 180 или 365 дней"
    );
    Ok(
        grammers_client::tl::functions::account::SetAuthorizationTtl {
            authorization_ttl_days: days,
        },
    )
}

fn device_change_error(error: InvocationError) -> anyhow::Error {
    if error.is("FRESH_RESET_AUTHORISATION_FORBIDDEN") {
        anyhow::anyhow!(
            "Telegram ограничивает управление сеансами в первые 24 часа после входа на этом устройстве"
        )
    } else if error.is("TTL_DAYS_INVALID") {
        anyhow::anyhow!("Telegram отклонил выбранный срок. Обновите список устройств")
    } else if error.is("HASH_INVALID") {
        anyhow::anyhow!("Сеанс больше недоступен. Обновите список устройств")
    } else {
        error.into()
    }
}

fn public_usernames(primary: Option<&str>, extra: Option<&[enums::Username]>) -> Vec<String> {
    let mut names = Vec::<String>::new();
    let extras = extra.into_iter().flatten().filter_map(|name| {
        let enums::Username::Username(name) = name;
        name.active.then_some(name.username.as_str())
    });
    for name in primary.into_iter().chain(extras) {
        let name = name.trim().trim_start_matches('@');
        if !name.is_empty() {
            let name = format!("@{name}");
            if !names.iter().any(|known| known.eq_ignore_ascii_case(&name)) {
                names.push(name);
            }
        }
    }
    names
}

fn member_next(offset: i32, received: usize, count: i32) -> Option<i32> {
    let next = offset.checked_add(i32::try_from(received).ok()?)?;
    (received > 0 && next < count).then_some(next)
}

fn basic_member_role(participant: &enums::ChatParticipant) -> MemberRole {
    match participant {
        enums::ChatParticipant::Creator(_) => MemberRole::Owner,
        enums::ChatParticipant::Admin(_) => MemberRole::Administrator,
        enums::ChatParticipant::Participant(_) => MemberRole::Member,
    }
}

fn basic_admin_state(participant: &enums::ChatParticipant) -> Option<AdminState> {
    match participant {
        enums::ChatParticipant::Participant(_) => Some(AdminState::Member),
        enums::ChatParticipant::Admin(_) => Some(AdminState::Administrator(AdminRights::default())),
        enums::ChatParticipant::Creator(_) => None,
    }
}

fn channel_admin_state(participant: &enums::ChannelParticipant) -> Option<(AdminState, bool)> {
    use enums::ChannelParticipant as P;
    match participant {
        P::Participant(_) => Some((AdminState::Member, true)),
        P::Admin(admin) if !admin.is_self => Some((
            AdminState::Administrator(model_admin_rights(&admin.admin_rights)),
            admin.can_edit,
        )),
        _ => None,
    }
}

macro_rules! admin_rights_mapping {
    ($($right:ident => $field:ident),+ $(,)?) => {
        fn model_admin_rights(rights: &enums::ChatAdminRights) -> AdminRights {
            let enums::ChatAdminRights::Rights(rights) = rights;
            AdminRights::from_rights([
                $((AdminRight::$right, rights.$field)),+
            ].into_iter().filter_map(|(right, enabled)| enabled.then_some(right)))
        }

        fn telegram_admin_rights(rights: AdminRights) -> types::ChatAdminRights {
            types::ChatAdminRights {
                $($field: rights.has(AdminRight::$right)),+
            }
        }
    };
}

admin_rights_mapping! {
    ChangeInfo => change_info,
    PostMessages => post_messages,
    EditMessages => edit_messages,
    DeleteMessages => delete_messages,
    BanUsers => ban_users,
    InviteUsers => invite_users,
    PinMessages => pin_messages,
    AddAdmins => add_admins,
    Anonymous => anonymous,
    ManageCall => manage_call,
    Other => other,
    ManageTopics => manage_topics,
    PostStories => post_stories,
    EditStories => edit_stories,
    DeleteStories => delete_stories,
    ManageDirectMessages => manage_direct_messages,
    ManageRanks => manage_ranks,
    ManageLinkedPeers => manage_linked_peers,
    ManageWelcomeMessages => manage_welcome_messages,
}

fn channel_admin_policy(channel: &types::Channel) -> Option<AdminPolicy> {
    if channel.left || channel.min || channel.monoforum {
        return None;
    }
    let available = AdminRights::channel(
        channel.broadcast,
        channel.forum,
        channel.linked_monoforum_id.is_some(),
    );
    if channel.creator {
        return Some(AdminPolicy::Channel(available));
    }
    let own = model_admin_rights(channel.admin_rights.as_ref()?);
    own.has(AdminRight::AddAdmins)
        .then_some(AdminPolicy::Channel(
            available.intersect(own.with(AdminRight::Other, true)),
        ))
}

fn channel_member(participant: &enums::ChannelParticipant) -> Option<(PeerId, Option<MemberRole>)> {
    use enums::ChannelParticipant as P;
    let (id, role) = match participant {
        P::Participant(value) => (PeerId::user(value.user_id), Some(MemberRole::Member)),
        P::ParticipantSelf(value) => (PeerId::user(value.user_id), Some(MemberRole::Member)),
        P::Creator(value) => (PeerId::user(value.user_id), Some(MemberRole::Owner)),
        P::Admin(value) => (PeerId::user(value.user_id), Some(MemberRole::Administrator)),
        P::Banned(value) => {
            let enums::ChatBannedRights::Rights(rights) = &value.banned_rights;
            let role = if rights.view_messages {
                Some(MemberRole::Banned)
            } else if member_restrictions::personal_restriction(
                rights,
                crate::model::restriction_now(),
            )
            .active()
            {
                Some(if value.left {
                    MemberRole::RestrictedFormer
                } else {
                    MemberRole::Restricted
                })
            } else {
                (!value.left).then_some(MemberRole::Member)
            };
            (raw_peer_id(&value.peer), role)
        }
        P::Left(value) => (raw_peer_id(&value.peer), None),
    };
    Some((id?, role))
}

fn channel_moderation_needed(role: Option<MemberRole>, change: MemberChange) -> Result<bool> {
    match (role, change) {
        (Some(MemberRole::Member | MemberRole::Restricted), MemberChange::Ban)
        | (Some(MemberRole::Banned), MemberChange::Unban) => Ok(true),
        (Some(MemberRole::Banned), MemberChange::Ban)
        | (
            Some(MemberRole::Member | MemberRole::Restricted | MemberRole::RestrictedFormer) | None,
            MemberChange::Unban,
        ) => Ok(false),
        (None, MemberChange::Ban) => bail!("Пользователь больше не состоит в чате"),
        _ => bail!("Роль участника изменилась. Обновите список"),
    }
}

fn membership_banned_rights(banned: bool) -> types::ChatBannedRights {
    types::ChatBannedRights {
        view_messages: banned,
        send_messages: false,
        send_media: false,
        send_stickers: false,
        send_gifs: false,
        send_games: false,
        send_inline: false,
        embed_links: false,
        send_polls: false,
        change_info: false,
        invite_users: false,
        pin_messages: false,
        manage_topics: false,
        send_photos: false,
        send_videos: false,
        send_roundvideos: false,
        send_audios: false,
        send_voices: false,
        send_docs: false,
        send_plain: false,
        edit_rank: false,
        send_reactions: false,
        manage_linked_peers: false,
        until_date: 0,
    }
}

fn bot_commands(id: i64, info: Option<enums::BotInfo>) -> Vec<BotCommand> {
    let mut seen = HashSet::new();
    let mut commands: Vec<_> = info
        .and_then(|enums::BotInfo::Info(info)| {
            info.user_id
                .is_none_or(|user| user == id)
                .then_some(info.commands)
                .flatten()
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|enums::BotCommand::Command(raw)| {
            let name = raw.command;
            let valid = (1..=32).contains(&name.len())
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
            (valid && seen.insert(name.clone())).then(|| BotCommand {
                name: format!("/{name}"),
                description: raw.description,
            })
        })
        .collect();
    if !seen.contains("start") {
        commands.insert(
            0,
            BotCommand {
                name: "/start".into(),
                description: "Запустить бота".into(),
            },
        );
    }
    commands
}

fn can_edit_chat(creator: bool, rights: Option<&enums::ChatAdminRights>) -> bool {
    creator || rights.is_some_and(|enums::ChatAdminRights::Rights(rights)| rights.change_info)
}

fn can_moderate_chat(creator: bool, rights: Option<&enums::ChatAdminRights>) -> bool {
    creator || rights.is_some_and(|enums::ChatAdminRights::Rights(rights)| rights.ban_users)
}

fn can_manage_invites(creator: bool, rights: Option<&enums::ChatAdminRights>) -> bool {
    creator || rights.is_some_and(|enums::ChatAdminRights::Rights(rights)| rights.invite_users)
}

fn invite_link(raw: enums::ExportedChatInvite) -> Option<InviteLink> {
    let enums::ExportedChatInvite::ChatInviteExported(raw) = raw else {
        return None;
    };
    (!raw.link.is_empty()).then_some(InviteLink {
        cursor: InviteCursor {
            date: raw.date,
            link: raw.link,
        },
        title: raw.title.unwrap_or_default(),
        permanent: raw.permanent,
        revoked: raw.revoked,
        expires: raw.expire_date.filter(|date| *date > 0),
        limit: raw.usage_limit.filter(|limit| *limit > 0),
        uses: raw.usage.unwrap_or_default().max(0),
        approval: raw.request_needed,
        pending: raw.requested.unwrap_or_default().max(0),
        paid: raw.subscription_pricing.is_some(),
    })
}

fn exported_invite_page(
    raw: types::messages::ExportedChatInvites,
    cursor: Option<&InviteCursor>,
    revoked: bool,
) -> InvitePage {
    let received = raw.invites.len();
    let mut links: Vec<_> = raw.invites.into_iter().filter_map(invite_link).collect();
    let next = (received >= INVITE_PAGE_SIZE as usize || received < raw.count.max(0) as usize)
        .then(|| links.last().map(|link| link.cursor.clone()))
        .flatten()
        .filter(|next| Some(next) != cursor);
    links.retain(|link| link.revoked == revoked);
    InvitePage { links, next }
}

fn owned_invite_link(
    raw: enums::ExportedChatInvite,
    account: i64,
    link: &str,
) -> Result<InviteLink> {
    let enums::ExportedChatInvite::ChatInviteExported(invite) = &raw else {
        bail!("Telegram вернул неизвестную ссылку");
    };
    ensure!(
        invite.link == link && invite.admin_id == account,
        "Ссылка принадлежит другому администратору"
    );
    invite_link(raw).context("Ссылка недоступна")
}

fn peer_profile_view(peer: &Peer) -> Result<PeerProfile> {
    let mut profile = PeerProfile {
        id: peer.id().bot_api_dialog_id().context("Чат не найден")?,
        title: peer
            .name()
            .filter(|name| !name.is_empty())
            .unwrap_or("Без названия")
            .to_owned(),
        kind: chat_kind(peer),
        is_self: false,
        blocked: None,
        avatar: peer_avatar(peer),
        about: String::new(),
        usernames: Vec::new(),
        phone: None,
        members: None,
        online_members: None,
        verified: false,
        scam: false,
        fake: false,
        can_edit_chat: false,
        can_view_members: false,
        moderation: None,
        admin_policy: None,
        can_manage_invites: false,
        can_add_members: false,
        can_read_admin_log: false,
        addresses: None,
        group_permissions: group_permissions::group_permissions(peer).map(|(settings, _)| settings),
        forum_mode: None,
        bot_commands: Vec::new(),
    };
    let channel = match peer {
        Peer::Channel(channel) => Some(&channel.raw),
        Peer::Group(group) => match &group.raw {
            enums::Chat::Channel(channel) => Some(channel),
            enums::Chat::Chat(chat) => {
                profile.members = u32::try_from(chat.participants_count).ok();
                if !chat.left && !chat.deactivated && chat.migrated_to.is_none() {
                    profile.admin_policy = chat.creator.then_some(AdminPolicy::Basic);
                    profile.can_edit_chat = can_edit_chat(chat.creator, chat.admin_rights.as_ref());
                    profile.can_manage_invites =
                        can_manage_invites(chat.creator, chat.admin_rights.as_ref());
                    profile.can_add_members = profile.can_manage_invites
                        || !invite_restricted(chat.default_banned_rights.as_ref());
                    profile.moderation =
                        can_moderate_chat(chat.creator, chat.admin_rights.as_ref())
                            .then_some(MemberModeration::Remove);
                }
                None
            }
            _ => None,
        },
        Peer::User(user) => {
            profile.is_self = user.is_self();
            let name = user.full_name();
            if !name.is_empty() {
                profile.title = name;
            }
            if user.deleted() {
                profile.title = "Удалённый аккаунт".to_owned();
                profile.avatar = None;
            } else if let enums::User::User(raw) = &user.raw {
                profile.usernames =
                    public_usernames(raw.username.as_deref(), raw.usernames.as_deref());
                profile.phone = raw
                    .phone
                    .as_ref()
                    .filter(|phone| !phone.is_empty())
                    .cloned();
                profile.verified = raw.verified;
                profile.scam = raw.scam;
                profile.fake = raw.fake;
            }
            None
        }
        Peer::Community(_) => None,
    };
    if let Some(channel) = channel {
        profile.forum_mode = forum_mode::owner_forum_mode(channel);
        profile.admin_policy = channel_admin_policy(channel);
        profile.can_read_admin_log = !channel.left
            && !channel.min
            && !channel.monoforum
            && (channel.creator || channel.admin_rights.is_some());
        profile.can_edit_chat =
            !channel.left && can_edit_chat(channel.creator, channel.admin_rights.as_ref());
        profile.can_manage_invites = !channel.left
            && !channel.min
            && !channel.monoforum
            && can_manage_invites(channel.creator, channel.admin_rights.as_ref());
        profile.can_add_members = !channel.left
            && !channel.min
            && !channel.monoforum
            && (profile.can_manage_invites
                || (channel.megagroup
                    && !invite_restricted(channel.default_banned_rights.as_ref())
                    && !invite_restricted(channel.banned_rights.as_ref())));
        profile.moderation = (!channel.left
            && !channel.min
            && can_moderate_chat(channel.creator, channel.admin_rights.as_ref()))
        .then_some(MemberModeration::Ban);
        profile.usernames =
            public_usernames(channel.username.as_deref(), channel.usernames.as_deref());
        if channel.creator && !channel.left && !channel.min && !channel.monoforum {
            profile.addresses = Some(chat_addresses::channel_addresses(
                channel.username.as_deref(),
                channel.usernames.as_deref(),
            ));
        }
        profile.members = channel
            .participants_count
            .and_then(|count| u32::try_from(count).ok());
        profile.verified = channel.verified;
        profile.scam = channel.scam;
        profile.fake = channel.fake;
    }
    Ok(profile)
}

fn own_profile_view(user: &grammers_client::peer::User, about: String) -> Result<OwnProfile> {
    ensure!(
        user.is_self() && !user.deleted(),
        "Telegram вернул другой аккаунт"
    );
    Ok(OwnProfile {
        id: user
            .id()
            .bot_api_dialog_id()
            .context("Профиль аккаунта недоступен")?,
        details: ProfileDetails {
            first_name: user.first_name().unwrap_or_default().to_owned(),
            last_name: user.last_name().unwrap_or_default().to_owned(),
            about,
        },
        username: user.username().unwrap_or_default().to_owned(),
        avatar: peer_avatar(&Peer::User(user.clone())),
    })
}

fn profile_rpc_error(error: InvocationError) -> anyhow::Error {
    match error {
        error if error.is("ABOUT_TOO_LONG") => {
            anyhow::anyhow!("Telegram отклонил слишком длинное описание")
        }
        error if error.is("FIRSTNAME_INVALID") => anyhow::anyhow!("Telegram отклонил имя"),
        error if error.is("USERNAME_OCCUPIED") => {
            anyhow::anyhow!("Это имя пользователя уже занято")
        }
        error if error.is("USERNAME_PURCHASE_AVAILABLE") => {
            anyhow::anyhow!("Telegram предлагает это имя пользователя на Fragment")
        }
        error if error.is("USERNAME_INVALID") => {
            anyhow::anyhow!("Telegram отклонил имя пользователя")
        }
        error => error.into(),
    }
}

fn normalize_phone(value: &str) -> Result<String> {
    let number: String = value
        .chars()
        .filter(|character| !character.is_whitespace() && !matches!(character, '(' | ')' | '-'))
        .collect();
    ensure!(
        number.strip_prefix('+').is_some_and(|digits| {
            (5..=15).contains(&digits.len()) && digits.bytes().all(|digit| digit.is_ascii_digit())
        }),
        "Введите номер с кодом страны, например +380…"
    );
    Ok(number)
}

fn peer_avatar(peer: &Peer) -> Option<Avatar> {
    let (photo, dc) = match peer {
        Peer::User(user) => {
            let photo = user.photo()?;
            (photo.photo_id, photo.dc_id)
        }
        Peer::Group(group) => {
            let photo = group.photo()?;
            (photo.photo_id, photo.dc_id)
        }
        Peer::Channel(channel) => {
            let photo = channel.photo()?;
            (photo.photo_id, photo.dc_id)
        }
        Peer::Community(community) => {
            let photo = community.photo()?;
            (photo.photo_id, photo.dc_id)
        }
    };
    Some(Avatar {
        peer: peer.id().bot_api_dialog_id()?,
        photo,
        dc,
    })
}

fn can_write_chat(peer: &Peer) -> bool {
    match peer {
        Peer::Channel(channel) => {
            let raw = &channel.raw;
            !raw.left
                && !raw.min
                && (raw.creator
                    || raw
                        .admin_rights
                        .as_ref()
                        .is_some_and(|enums::ChatAdminRights::Rights(rights)| rights.post_messages))
        }
        _ => true,
    }
}

fn chat_kind(peer: &Peer) -> ChatKind {
    match peer {
        Peer::User(user) if user.is_bot() => ChatKind::Bot,
        Peer::User(user) if user.contact() => ChatKind::Contact,
        Peer::User(_) => ChatKind::User,
        Peer::Channel(_) => ChatKind::Broadcast,
        Peer::Group(_) | Peer::Community(_) => ChatKind::Group,
    }
}

fn raw_peer_id(peer: &enums::Peer) -> Option<PeerId> {
    match peer {
        enums::Peer::User(peer) => PeerId::user(peer.user_id),
        enums::Peer::Chat(peer) => PeerId::chat(peer.chat_id),
        enums::Peer::Channel(peer) => PeerId::channel(peer.channel_id),
    }
}

fn next_blocked_offset(offset: i32, received: usize, count: Option<i32>) -> Option<i32> {
    let next = offset.checked_add(i32::try_from(received).ok()?)?;
    (received > 0 && next < count?).then_some(next)
}

fn input_peer_id(peer: &enums::InputPeer, self_id: Option<PeerId>) -> Option<i64> {
    match peer {
        enums::InputPeer::PeerSelf => self_id,
        enums::InputPeer::User(peer) => PeerId::user(peer.user_id),
        enums::InputPeer::Chat(peer) => PeerId::chat(peer.chat_id),
        enums::InputPeer::Channel(peer) => PeerId::channel(peer.channel_id),
        enums::InputPeer::UserFromMessage(peer) => PeerId::user(peer.user_id),
        enums::InputPeer::ChannelFromMessage(peer) => PeerId::channel(peer.channel_id),
        enums::InputPeer::Empty => None,
    }?
    .bot_api_dialog_id()
}

fn set_folder_pin(
    filter: &mut enums::DialogFilter,
    peer: enums::InputPeer,
    pinned: bool,
    self_id: Option<PeerId>,
) -> Result<()> {
    let id = input_peer_id(&peer, self_id).context("Чат недоступен для закрепления")?;
    let pins = match filter {
        enums::DialogFilter::Filter(filter) => &mut filter.pinned_peers,
        enums::DialogFilter::Chatlist(filter) => &mut filter.pinned_peers,
        enums::DialogFilter::Default => {
            bail!("Для общего списка используется отдельное закрепление")
        }
    };
    pins.retain(|peer| input_peer_id(peer, self_id) != Some(id));
    if pinned {
        pins.insert(0, peer);
    }
    Ok(())
}

fn folder_view(folder: enums::DialogFilter, self_id: Option<PeerId>) -> Option<ChatFolder> {
    fn peers(peers: Vec<enums::InputPeer>, self_id: Option<PeerId>) -> Vec<i64> {
        peers
            .iter()
            .filter_map(|peer| input_peer_id(peer, self_id))
            .collect()
    }
    let result = match folder {
        enums::DialogFilter::Filter(folder) => {
            let enums::TextWithEntities::Entities(title) = folder.title;
            let kinds = [
                (folder.contacts, ChatKind::Contact),
                (folder.non_contacts, ChatKind::User),
                (folder.groups, ChatKind::Group),
                (folder.broadcasts, ChatKind::Broadcast),
                (folder.bots, ChatKind::Bot),
            ]
            .into_iter()
            .filter_map(|(enabled, kind)| enabled.then_some(kind))
            .collect();
            ChatFolder {
                id: folder.id,
                title: title.text,
                pinned: peers(folder.pinned_peers, self_id),
                included: peers(folder.include_peers, self_id),
                excluded: peers(folder.exclude_peers, self_id),
                kinds,
                exclude_muted: folder.exclude_muted,
                exclude_read: folder.exclude_read,
                exclude_archived: folder.exclude_archived,
            }
        }
        enums::DialogFilter::Chatlist(folder) => {
            let enums::TextWithEntities::Entities(title) = folder.title;
            ChatFolder {
                id: folder.id,
                title: title.text,
                pinned: peers(folder.pinned_peers, self_id),
                included: peers(folder.include_peers, self_id),
                ..ChatFolder::default()
            }
        }
        enums::DialogFilter::Default => return None,
    };
    Some(result)
}

fn pick_thumbnail(sizes: Vec<PhotoSize>) -> Option<PhotoSize> {
    sizes
        .into_iter()
        .filter(|thumb| {
            let (width, height) = match thumb {
                PhotoSize::Size(size) => (size.width, size.height),
                PhotoSize::Progressive(size) => (size.width, size.height),
                PhotoSize::Cached(size) => (size.width, size.height),
                _ => return false,
            };
            (1..=480).contains(&width)
                && (1..=480).contains(&height)
                && (1..=512 * 1024).contains(&thumb.size())
        })
        .max_by_key(PhotoSize::size)
}

fn expired_file_reference(error: &InvocationError) -> bool {
    error.is("FILE_REFERENCE_EXPIRED") || error.is("FILE_REFERENCE_INVALID")
}

fn authorization_already_ended(error: &InvocationError) -> bool {
    [
        "AUTH_KEY_UNREGISTERED",
        "SESSION_REVOKED",
        "SESSION_EXPIRED",
    ]
    .iter()
    .any(|name| error.is(name))
}

fn retry_update_error(error: &InvocationError) -> bool {
    match error {
        InvocationError::Io(_) | InvocationError::Dropped(_) | InvocationError::Transport(_) => {
            true
        }
        InvocationError::Rpc(error) => error.code == 420 || error.code >= 500,
        _ => false,
    }
}

fn update_retry_delay(error: &InvocationError, backoff: Duration) -> Duration {
    if let InvocationError::Rpc(error) = error
        && error.code == 420
        && let Some(seconds) = error.value
    {
        return backoff.max(Duration::from_secs(seconds.into()));
    }
    backoff
}

fn message_view(
    message: &TelegramMessage,
    self_id: Option<PeerId>,
    profile: Option<&Peer>,
    chat_author: Option<PeerId>,
    author: Option<ChatAuthor>,
) -> Message {
    let topic = forum_topics::message_topic(&message.raw);
    let outgoing = message.outgoing();
    let attachment = attachment(message);
    let sticker = stickers::message_sticker(message);
    let photo_preview = matches!(attachment, Some((Media::Photo(_), _)));
    let playback = attachment
        .as_ref()
        .and_then(|(media, _)| playable_media(media));
    let audio = attachment.as_ref().and_then(|(media, _)| audio_info(media));
    let thumbnail = attachment.as_ref().and_then(|(media, _)| thumbnail(media));
    let poll = match message.media() {
        Some(Media::Poll(raw)) => Some(poll_timing::poll_view_at(&raw, message)),
        _ => None,
    };
    let (reply_to, reply_preview) = match &message.raw {
        enums::Message::Message(raw) => match &raw.reply_to {
            Some(enums::MessageReplyHeader::Header(header)) if !header.reply_to_ephemeral => {
                let same_chat = header
                    .reply_to_peer_id
                    .as_ref()
                    .is_none_or(|peer| PeerId::from(peer.clone()) == message.peer_id());
                let id = header
                    .reply_to_msg_id
                    .filter(|id| same_chat && Some(*id) != topic);
                let quote = header
                    .quote_text
                    .as_ref()
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| ReplyPreview {
                        sender: None,
                        self_authored: false,
                        text: quote_preview(
                            text,
                            header.quote_entities.as_deref().unwrap_or_default(),
                        ),
                    });
                (id, quote)
            }
            _ => (None, None),
        },
        _ => (None, None),
    };
    Message {
        id: message.id(),
        topic,
        sender: author
            .as_ref()
            .map(|author| author.name.clone())
            .unwrap_or_else(|| {
                if let Some(author) = chat_author {
                    if author.kind() == PeerKind::Chat
                        || (!message.post() && author == message.peer_id())
                    {
                        "Группа"
                    } else {
                        "Канал"
                    }
                } else if outgoing {
                    "Вы"
                } else {
                    "Сообщение"
                }
                .to_owned()
            }),
        sender_avatar: author.and_then(|author| author.avatar),
        self_authored: chat_author.is_none()
            && (matches!(message.sender(), Some(Peer::User(user)) if user.is_self())
                || message.sender_id().is_some_and(|id| {
                    id == PeerId::self_user()
                        || Some(id) == self_id
                        || profile.is_some_and(|own| own.id() == id)
                })
                || (outgoing && !message.post() && message.sender_id().is_none())),
        timestamp: message.date().as_second(),
        text: if attachment.is_some() || poll.is_some() || sticker.is_some() {
            message.text().to_owned()
        } else {
            message_text(message)
        },
        styles: message_styles(message),
        links: incoming_links(
            message.text(),
            message.fmt_entities().map_or(&[], Vec::as_slice),
        ),
        outgoing,
        attachment: attachment.map(|(_, name)| name),
        photo_preview,
        playback,
        audio,
        thumbnail,
        can_close_poll: poll.is_some() && poll_management::can_close_poll(message),
        poll,
        sticker,
        reply_to,
        reply_preview,
        can_reply: matches!(message.raw, enums::Message::Message(_)),
        can_edit: can_edit_text(message),
        can_delete: can_delete(message, self_id),
        can_forward: can_forward(message),
        forwarded: matches!(&message.raw, enums::Message::Message(raw) if raw.fwd_from.is_some()),
        pinned: message.pinned(),
        reactions: message_reactions(message),
        album: message.grouped_id(),
        edited: message.edit_date().is_some() && !message.edit_hide(),
        ephemeral: matches!(&message.raw, enums::Message::Message(raw) if raw.ttl_period.is_some())
            || match message.media() {
                Some(Media::Photo(photo)) => photo.raw.ttl_seconds.is_some(),
                Some(Media::Document(document)) => document.raw.ttl_seconds.is_some(),
                Some(Media::Sticker(sticker)) => sticker.document.raw.ttl_seconds.is_some(),
                _ => false,
            },
    }
}

fn poll_question(text: &enums::TextWithEntities) -> String {
    let enums::TextWithEntities::Entities(text) = text;
    quote_preview(&text.text, &text.entities)
}

fn poll_media(
    poll: PollDraft,
    random_id: i64,
    close_period: Option<i32>,
    close_date: Option<i32>,
) -> types::InputMediaPoll {
    let (correct_answers, solution) = match poll.quiz {
        Some(quiz) => (
            Some(quiz.correct.into_iter().map(|index| index as i32).collect()),
            (!quiz.solution.is_empty()).then_some(quiz.solution),
        ),
        None => (None, None),
    };
    let text = |text| {
        types::TextWithEntities {
            text,
            entities: Vec::new(),
        }
        .into()
    };
    types::InputMediaPoll {
        poll: types::Poll {
            id: random_id,
            closed: false,
            public_voters: poll.public,
            multiple_choice: poll.multiple,
            quiz: correct_answers.is_some(),
            open_answers: poll.open_answers,
            revoting_disabled: poll.revoting_disabled || correct_answers.is_some(),
            shuffle_answers: poll.shuffle_answers,
            hide_results_until_close: poll.hide_results_until_close,
            creator: false,
            subscribers_only: false,
            question: text(poll.question),
            answers: poll
                .choices
                .into_iter()
                .map(|choice| {
                    types::InputPollAnswer {
                        text: text(choice),
                        media: None,
                    }
                    .into()
                })
                .collect(),
            close_period,
            close_date,
            countries_iso2: None,
            hash: 0,
        }
        .into(),
        correct_answers,
        attached_media: None,
        solution_entities: solution.as_ref().map(|_| Vec::new()),
        solution,
        solution_media: None,
    }
}

fn poll_definition(raw: &types::Poll) -> Poll {
    Poll {
        open_answers: raw.open_answers,
        id: raw.id,
        question: poll_question(&raw.question),
        creator: raw.creator,
        hide_results_until_close: raw.hide_results_until_close,
        choices: raw
            .answers
            .iter()
            .filter_map(|answer| {
                let enums::PollAnswer::Answer(answer) = answer else {
                    return None;
                };
                Some(PollChoice {
                    option: answer.option.clone(),
                    label: poll_question(&answer.text),
                    votes: None,
                    chosen: false,
                    correct: false,
                    addition: poll_answers::answer_addition(answer),
                })
            })
            .collect(),
        closed: raw.closed,
        public: raw.public_voters,
        multiple: raw.multiple_choice,
        quiz: raw.quiz,
        revoting_disabled: raw.revoting_disabled,
        shuffle_answers: raw.shuffle_answers,
        total: None,
        solution: None,
        closes_at: raw.close_date.filter(|date| *date > 0).map(i64::from),
    }
}

fn raw_results(results: &enums::PollResults) -> &types::PollResults {
    let enums::PollResults::Results(results) = results;
    results
}

fn poll_results(
    id: i64,
    target: Option<(i64, i32)>,
    definition: Option<Poll>,
    raw: &types::PollResults,
) -> PollUpdate {
    PollUpdate {
        id,
        target,
        definition,
        results: raw.results.as_ref().map(|results| {
            results
                .iter()
                .map(|entry| {
                    let enums::PollAnswerVoters::Voters(entry) = entry;
                    PollVoteCount {
                        option: entry.option.clone(),
                        votes: entry.voters.map(|count| count.max(0)),
                        chosen: entry.chosen,
                        correct: entry.correct,
                    }
                })
                .collect()
        }),
        total: raw.total_voters.map(|count| count.max(0)),
        solution: raw
            .solution
            .as_ref()
            .map(|text| quote_preview(text, raw.solution_entities.as_deref().unwrap_or_default())),
        partial: raw.min,
    }
}

fn poll_view(raw: &grammers_client::media::Poll) -> Poll {
    let mut poll = poll_definition(&raw.raw);
    poll_results(poll.id, None, None, &raw.raw_results).apply(0, 0, &mut poll);
    poll
}

fn poll_update(raw: &types::UpdateMessagePoll) -> PollUpdate {
    let target = raw
        .peer
        .clone()
        .and_then(|peer| PeerId::from(peer).bot_api_dialog_id())
        .zip(raw.msg_id);
    let definition = raw
        .poll
        .as_ref()
        .map(|enums::Poll::Poll(poll)| poll_definition(poll));
    let mut update = poll_results(raw.poll_id, target, definition, raw_results(&raw.results));
    update.partial |= target.is_none();
    update
}

fn chat_author_id(
    sender: Option<PeerId>,
    destination: PeerId,
    post: bool,
    outgoing: bool,
) -> Option<PeerId> {
    match sender {
        Some(id) if id.kind() != PeerKind::User => Some(id),
        None if destination.kind() != PeerKind::User && (post || !outgoing) => Some(destination),
        _ => None,
    }
}

fn message_chat_author(message: &TelegramMessage) -> Option<PeerId> {
    chat_author_id(
        message.sender_id(),
        message.peer_id(),
        message.post(),
        message.outgoing(),
    )
}

fn incoming_author_peer(message: &TelegramMessage, id: PeerId) -> Option<&Peer> {
    message
        .sender()
        .filter(|peer| peer.id() == id)
        .or_else(|| message.peer().filter(|peer| peer.id() == id))
}

fn peer_author(peer: &Peer) -> Option<ChatAuthor> {
    let name = match peer {
        Peer::User(user) => user.full_name(),
        peer => peer.name()?.to_owned(),
    };
    (!name.trim().is_empty()).then(|| ChatAuthor {
        name,
        avatar: peer_avatar(peer),
    })
}

fn can_delete(message: &TelegramMessage, self_id: Option<PeerId>) -> bool {
    matches!(message.raw, enums::Message::Message(_))
        && (message.outgoing()
            || Some(message.peer_id()) == self_id
            || matches!(message.peer(), Some(Peer::User(user)) if user.is_self()))
}

fn reaction_counts(message: &TelegramMessage) -> &[enums::ReactionCount] {
    if let enums::Message::Message(raw) = &message.raw
        && let Some(enums::MessageReactions::Reactions(reactions)) = &raw.reactions
    {
        &reactions.results
    } else {
        &[]
    }
}

fn message_reactions(message: &TelegramMessage) -> Vec<Reaction> {
    reaction_counts(message)
        .iter()
        .filter_map(|enums::ReactionCount::Count(reaction)| {
            let enums::Reaction::Emoji(emoji) = &reaction.reaction else {
                return None;
            };
            Some(Reaction {
                emoji: emoji.emoticon.clone(),
                count: reaction.count,
                chosen: reaction.chosen_order.is_some(),
            })
        })
        .collect()
}

fn can_forward(message: &TelegramMessage) -> bool {
    let enums::Message::Message(raw) = &message.raw else {
        return false;
    };
    !raw.noforwards
        && raw.ttl_period.is_none()
        && match message.media() {
            Some(Media::Photo(photo)) => photo.raw.ttl_seconds.is_none(),
            Some(Media::Document(document)) => document.raw.ttl_seconds.is_none(),
            Some(Media::Sticker(sticker)) => sticker.document.raw.ttl_seconds.is_none(),
            _ => true,
        }
}

fn entity_ranges<'a>(
    text: &str,
    entities: &'a [enums::MessageEntity],
) -> impl Iterator<Item = (std::ops::Range<usize>, &'a enums::MessageEntity)> {
    let mut utf16 = 0;
    let mut boundaries = HashMap::new();
    if !entities.is_empty() {
        for (byte, ch) in text.char_indices() {
            boundaries.insert(utf16, byte);
            utf16 += ch.len_utf16() as i32;
        }
        boundaries.insert(utf16, text.len());
    }
    entities.iter().filter_map(move |entity| {
        let start = *boundaries.get(&entity.offset())?;
        let end = *boundaries.get(&entity.offset().checked_add(entity.length())?)?;
        (start < end).then_some((start..end, entity))
    })
}

fn incoming_styles(text: &str, entities: &[enums::MessageEntity]) -> Vec<TextSpan> {
    use enums::MessageEntity as Entity;
    let mut spans: Vec<_> = entity_ranges(text, entities)
        .filter_map(|(range, entity)| {
            let style = match entity {
                Entity::Bold(_) => TextStyle::Bold,
                Entity::Italic(_) => TextStyle::Italic,
                Entity::Underline(_) => TextStyle::Underline,
                Entity::Strike(_) => TextStyle::Strike,
                Entity::Code(_) | Entity::Pre(_) => TextStyle::Code,
                Entity::Spoiler(_) => TextStyle::Spoiler,
                _ => return None,
            };
            Some(TextSpan { range, style })
        })
        .collect();
    spans.sort_by_key(|span| (span.range.start, span.range.end, span.style));
    spans
}

fn incoming_links(text: &str, entities: &[enums::MessageEntity]) -> Vec<TextLink> {
    use enums::MessageEntity as Entity;
    entity_ranges(text, entities)
        .filter_map(|(range, entity)| {
            let uri = match entity {
                Entity::Url(_) => crate::links::normalize(&text[range.clone()]),
                Entity::TextUrl(link) => crate::links::normalize(&link.url),
                Entity::Email(_) => {
                    crate::links::normalize(&format!("mailto:{}", &text[range.clone()]))
                }
                _ => None,
            }?;
            Some(TextLink { range, uri })
        })
        .collect()
}

fn outgoing_entities(formatted: &FormattedText) -> Vec<enums::MessageEntity> {
    if formatted.spans.is_empty() {
        return Vec::new();
    }
    let mut boundaries: Vec<_> = formatted
        .spans
        .iter()
        .flat_map(|span| [span.range.start, span.range.end])
        .collect();
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut entities = Vec::new();
    for pair in boundaries.windows(2) {
        let [start, end] = [pair[0], pair[1]];
        let styles: std::collections::BTreeSet<_> = formatted
            .spans
            .iter()
            .filter(|span| span.range.start <= start && span.range.end >= end)
            .map(|span| span.style)
            .collect();
        let part = formatted.text[start..end].trim_end();
        if part.is_empty() {
            continue;
        }
        let offset = formatted.text[..start].encode_utf16().count() as i32;
        let length = part.encode_utf16().count() as i32;
        // Code is exclusive. Splitting all other spans at style boundaries keeps
        // partially overlapping selections valid as Telegram entities.
        if styles.contains(&TextStyle::Code) {
            entities.push(types::MessageEntityCode { offset, length }.into());
            continue;
        }
        for style in styles {
            entities.push(match style {
                TextStyle::Bold => types::MessageEntityBold { offset, length }.into(),
                TextStyle::Italic => types::MessageEntityItalic { offset, length }.into(),
                TextStyle::Underline => types::MessageEntityUnderline { offset, length }.into(),
                TextStyle::Strike => types::MessageEntityStrike { offset, length }.into(),
                TextStyle::Spoiler => types::MessageEntitySpoiler { offset, length }.into(),
                TextStyle::Code => unreachable!("code spans are exclusive"),
            });
        }
    }
    entities
}

fn can_edit_text(message: &TelegramMessage) -> bool {
    let enums::Message::Message(raw) = &message.raw else {
        return false;
    };
    message.outgoing()
        && !message.text().is_empty()
        && raw.fwd_from.is_none()
        && raw.via_bot_id.is_none()
        && raw.ttl_period.is_none()
        && raw.entities.iter().flatten().all(|entity| {
            matches!(
                entity,
                enums::MessageEntity::Bold(_)
                    | enums::MessageEntity::Italic(_)
                    | enums::MessageEntity::Underline(_)
                    | enums::MessageEntity::Strike(_)
                    | enums::MessageEntity::Code(_)
                    | enums::MessageEntity::Spoiler(_)
                    | enums::MessageEntity::Url(_)
                    | enums::MessageEntity::Email(_)
                    | enums::MessageEntity::Mention(_)
                    | enums::MessageEntity::Hashtag(_)
                    | enums::MessageEntity::Cashtag(_)
                    | enums::MessageEntity::BotCommand(_)
                    | enums::MessageEntity::Phone(_)
            )
        })
        && matches!(
            raw.media,
            None | Some(enums::MessageMedia::Empty | enums::MessageMedia::WebPage(_))
        )
}

fn thumbnail(media: &Media) -> Option<AttachmentThumbnail> {
    match media {
        Media::Photo(photo) if !photo.is_spoiler() && pick_thumbnail(photo.thumbs()).is_some() => {
            Some(AttachmentThumbnail::Photo(photo.id()))
        }
        Media::Document(document)
            if !document.is_spoiler() && pick_thumbnail(document.thumbs()).is_some() =>
        {
            Some(AttachmentThumbnail::Document(document.id()))
        }
        _ => None,
    }
}

fn quote_preview(text: &str, entities: &[enums::MessageEntity]) -> String {
    redact_spoilers(text, &incoming_styles(text, entities)).into_owned()
}

fn message_styles(message: &TelegramMessage) -> Vec<TextSpan> {
    incoming_styles(
        message.text(),
        message.fmt_entities().map_or(&[], Vec::as_slice),
    )
}

fn message_text(message: &TelegramMessage) -> String {
    if !message.text().is_empty() {
        message.text().to_owned()
    } else if let Some(action) = message.action() {
        match action {
            enums::MessageAction::PollAppendAnswer(action) => match &action.answer {
                enums::PollAnswer::Answer(answer) => {
                    format!("Добавлен вариант опроса «{}»", poll_question(&answer.text))
                }
                enums::PollAnswer::InputPollAnswer(answer) => {
                    format!("Добавлен вариант опроса «{}»", poll_question(&answer.text))
                }
            },
            enums::MessageAction::PollDeleteAnswer(_) => "Удалён вариант опроса".to_owned(),
            _ => "Событие в чате".to_owned(),
        }
    } else if let Some(sticker) = stickers::message_sticker(message) {
        sticker.label()
    } else if let Some(Media::Poll(raw)) = message.media() {
        let kind = if raw.raw.quiz {
            "Викторина"
        } else {
            "Опрос"
        };
        format!("{kind} · {}", poll_question(&raw.raw.question))
    } else if let Some((media, filename)) = attachment(message)
        && let Some(kind @ (PlayableMedia::Audio | PlayableMedia::Voice)) = playable_media(&media)
    {
        if kind == PlayableMedia::Voice {
            kind.title().to_owned()
        } else {
            audio_info(&media)
                .and_then(|info| info.title)
                .unwrap_or(filename)
        }
    } else if message.media().is_some() {
        "Вложение".to_owned()
    } else {
        String::new()
    }
}

fn notification_category(kind: ChatKind) -> ChatKind {
    match kind {
        ChatKind::Contact | ChatKind::Bot => ChatKind::User,
        _ => kind,
    }
}

fn notification_settings(
    settings: types::PeerNotifySettings,
    muted: bool,
) -> types::InputPeerNotifySettings {
    types::InputPeerNotifySettings {
        show_previews: settings.show_previews,
        silent: settings.silent,
        mute_until: Some(if muted { i32::MAX } else { 0 }),
        sound: settings.other_sound,
        stories_muted: settings.stories_muted,
        stories_hide_sender: settings.stories_hide_sender,
        stories_sound: settings.stories_other_sound,
    }
}

fn is_muted(until: i32, now: SystemTime) -> bool {
    u64::try_from(until)
        .is_ok_and(|until| until > now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
}

fn playable_media(media: &Media) -> Option<PlayableMedia> {
    if let Media::Sticker(sticker) = media {
        return stickers::video_playback(sticker);
    }
    let Media::Document(document) = media else {
        return None;
    };
    if document.raw.ttl_seconds.is_some() {
        return None;
    }
    let Some(enums::Document::Document(raw)) = &document.raw.document else {
        return None;
    };
    if document.raw.voice {
        return Some(PlayableMedia::Voice);
    }
    let size = u64::try_from(raw.size).unwrap_or_default();
    raw.attributes
        .iter()
        .find_map(|attribute| match attribute {
            enums::DocumentAttribute::Video(video) => Some(PlayableMedia::Video {
                width: video.w,
                height: video.h,
                duration: (video.duration.is_finite() && video.duration > 0.0)
                    .then(|| video.duration.ceil() as u32),
                size,
            }),
            enums::DocumentAttribute::Audio(audio) => Some(if audio.voice {
                PlayableMedia::Voice
            } else {
                PlayableMedia::Audio
            }),
            _ => None,
        })
        .or_else(|| {
            let mime = raw.mime_type.to_ascii_lowercase();
            if mime.starts_with("video/") {
                Some(PlayableMedia::Video {
                    width: 0,
                    height: 0,
                    duration: None,
                    size,
                })
            } else if mime.starts_with("audio/") {
                Some(PlayableMedia::Audio)
            } else if matches!(mime.as_str(), "application/octet-stream" | "image/gif") {
                let extension = document
                    .name()
                    .and_then(|name| std::path::Path::new(name).extension())?
                    .to_str()?
                    .to_ascii_lowercase();
                match extension.as_str() {
                    "mp4" | "webm" | "mkv" | "mov" | "m4v" | "avi" | "3gp" | "gif" => {
                        Some(PlayableMedia::Video {
                            width: 0,
                            height: 0,
                            duration: None,
                            size,
                        })
                    }
                    "mp3" | "wav" | "flac" | "ogg" | "opus" | "m4a" | "aac" => {
                        Some(PlayableMedia::Audio)
                    }
                    _ => None,
                }
            } else {
                None
            }
        })
}

fn audio_info(media: &Media) -> Option<AudioInfo> {
    let Media::Document(document) = media else {
        return None;
    };
    let Some(enums::Document::Document(raw)) = &document.raw.document else {
        return None;
    };
    raw.attributes.iter().find_map(|attribute| {
        let enums::DocumentAttribute::Audio(audio) = attribute else {
            return None;
        };
        let nonempty = |text: &Option<String>| {
            text.as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        Some(AudioInfo {
            title: nonempty(&audio.title),
            performer: nonempty(&audio.performer),
            duration: u32::try_from(audio.duration)
                .ok()
                .filter(|seconds| *seconds > 0),
            waveform: crate::audio::waveform(audio.waveform.as_deref().unwrap_or_default()),
        })
    })
}

fn attachment(message: &TelegramMessage) -> Option<(Media, String)> {
    let enums::Message::Message(raw) = &message.raw else {
        return None;
    };
    // Telegram's noforwards flag does not control manual saves in this private build.
    // Expiring messages and view-once media still use their separate TTL fields.
    if raw.ttl_period.is_some() {
        return None;
    }
    let media = message.media()?;
    let name = match &media {
        Media::Photo(photo) if photo.raw.ttl_seconds.is_none() => {
            format!("photo-{}.jpg", message.id())
        }
        Media::Sticker(sticker) if sticker.document.raw.ttl_seconds.is_none() => {
            let extension = match sticker.document.mime_type() {
                Some("image/webp") => "webp",
                Some("application/x-tgsticker") => "tgs",
                Some("video/webm") => "webm",
                _ => "bin",
            };
            let fallback = format!("sticker-{}.{extension}", message.id());
            attachment_name(sticker.document.name().unwrap_or(&fallback), &fallback)
        }
        Media::Document(document) if document.raw.ttl_seconds.is_none() => {
            let fallback = format!("file-{}", message.id());
            attachment_name(document.name().unwrap_or(&fallback), &fallback)
        }
        _ => return None,
    };
    (media.to_raw_input_location().is_some() || media.to_data().is_some()).then_some((media, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_media_sends_quiz_indices_and_pairs_optional_solution_fields() {
        use crate::model::PollQuizDraft;

        let mut draft = PollDraft {
            question: "Какой день идёт после вторника?".into(),
            choices: vec!["Понедельник".into(), " ".into(), "Среда".into()],
            public: true,
            ..PollDraft::default()
        };
        let media = poll_media(draft.clone().validate().unwrap(), 123, None, None);
        let enums::Poll::Poll(poll) = media.poll;
        assert!(poll.public_voters && !poll.quiz && !poll.revoting_disabled);
        assert!(media.correct_answers.is_none());
        assert!(media.solution.is_none() && media.solution_entities.is_none());

        draft.quiz = Some(PollQuizDraft {
            correct: vec![2],
            solution: "Среда идёт после вторника.".into(),
        });
        let media = poll_media(draft.clone().validate().unwrap(), 123, None, None);
        let enums::Poll::Poll(poll) = media.poll;
        assert_eq!(poll.id, 123);
        assert!(poll.public_voters && poll.quiz && poll.revoting_disabled);
        let enums::PollAnswer::InputPollAnswer(answer) = &poll.answers[1] else {
            panic!("expected an outgoing answer");
        };
        assert_eq!(poll_question(&answer.text), "Среда");
        assert_eq!(media.correct_answers, Some(vec![1]));
        assert_eq!(
            media.solution.as_deref(),
            Some("Среда идёт после вторника.")
        );
        assert_eq!(media.solution_entities, Some(vec![]));

        draft.quiz.as_mut().unwrap().solution.clear();
        let media = poll_media(draft.validate().unwrap(), 123, None, None);
        assert_eq!(media.correct_answers, Some(vec![1]));
        assert!(media.solution.is_none() && media.solution_entities.is_none());
    }

    #[test]
    fn timed_quiz_request_preserves_deadline_hidden_results_and_correct_indices() {
        use crate::model::{PollDeadline, PollQuizDraft};
        let now = 1_800_000_000;
        for deadline in [PollDeadline::After(60), PollDeadline::At(now + 120)] {
            let draft = PollDraft {
                question: "Когда?".into(),
                choices: vec!["Утром".into(), "".into(), "Днём".into()],
                quiz: Some(PollQuizDraft {
                    correct: vec![2],
                    solution: "После утра.".into(),
                }),
                deadline: Some(deadline),
                hide_results_until_close: true,
                ..PollDraft::default()
            }
            .validate()
            .unwrap();
            let (period, date) = draft.close_fields(now, 3600).unwrap();
            let request = poll_media(draft, 41, period, date);
            let enums::Poll::Poll(poll) = request.poll;
            assert!(poll.quiz && poll.hide_results_until_close && poll.revoting_disabled);
            assert_eq!((poll.close_period, poll.close_date), (period, date));
            assert_eq!(request.correct_answers, Some(vec![1]));
            assert_eq!(request.solution.as_deref(), Some("После утра."));
        }
    }

    #[test]
    fn open_answer_request_preserves_public_voting_and_timed_hidden_results() {
        let draft = PollDraft {
            question: "Когда?".into(),
            choices: vec!["Утром".into(), "Днём".into()],
            public: true,
            open_answers: true,
            hide_results_until_close: true,
            deadline: Some(crate::model::PollDeadline::After(60)),
            ..PollDraft::default()
        }
        .validate()
        .unwrap();
        let (period, date) = draft.close_fields(1_800_000_000, 3600).unwrap();
        let request = poll_media(draft, 42, period, date);
        let enums::Poll::Poll(poll) = request.poll;
        assert!(poll.public_voters && poll.open_answers && poll.hide_results_until_close);
        assert!(!poll.quiz);
        assert_eq!((poll.close_period, poll.close_date), (Some(60), None));
        assert!(request.correct_answers.is_none());
    }

    #[test]
    fn ordinary_poll_request_sets_the_selected_vote_lock() {
        for locked in [false, true] {
            let draft = PollDraft {
                question: "Когда?".into(),
                choices: vec!["Сейчас".into(), "Завтра".into()],
                revoting_disabled: locked,
                ..PollDraft::default()
            }
            .validate()
            .unwrap();
            let request = poll_media(draft, 43, None, None);
            let enums::Poll::Poll(poll) = request.poll;
            assert!(!poll.quiz);
            assert_eq!(poll.revoting_disabled, locked);
            assert!(request.correct_answers.is_none());
        }
    }

    #[test]
    fn shuffled_quiz_request_keeps_the_original_answer_indices() {
        for quiz in [false, true] {
            let draft = PollDraft {
                question: "Когда?".into(),
                choices: vec!["Первый".into(), "".into(), "Второй".into()],
                shuffle_answers: true,
                quiz: quiz.then(|| crate::model::PollQuizDraft {
                    correct: vec![2],
                    solution: String::new(),
                }),
                ..PollDraft::default()
            }
            .validate()
            .unwrap();
            let request = poll_media(draft, 17, None, None);
            let enums::Poll::Poll(poll) = request.poll;
            assert!(poll.shuffle_answers);
            let enums::PollAnswer::InputPollAnswer(first) = &poll.answers[0] else {
                panic!("Expected an input answer")
            };
            assert_eq!(poll_question(&first.text), "Первый");
            assert_eq!(request.correct_answers, quiz.then(|| vec![1]));
        }
    }

    #[test]
    fn channel_admin_mapping_preserves_protocol_flags_and_server_editability() {
        use AdminRight::*;
        let rights = AdminRights::from_rights([PostMessages, AddAdmins, ManageLinkedPeers, Other]);
        let encoded = telegram_admin_rights(rights);
        assert!(encoded.post_messages && encoded.add_admins && encoded.manage_linked_peers);
        assert!(encoded.other && !encoded.delete_messages && !encoded.anonymous);
        assert!(!telegram_admin_rights(AdminRights::default()).other);
        assert_eq!(model_admin_rights(&encoded.clone().into()), rights);
        let mut admin = types::ChannelParticipantAdmin {
            can_edit: true,
            is_self: false,
            user_id: 42,
            inviter_id: None,
            promoted_by: 17,
            date: 100,
            admin_rights: encoded.into(),
            rank: Some("Модератор".into()),
        };
        assert_eq!(
            channel_admin_state(&admin.clone().into()),
            Some((AdminState::Administrator(rights), true))
        );
        admin.can_edit = false;
        assert_eq!(
            channel_admin_state(&admin.clone().into()),
            Some((AdminState::Administrator(rights), false))
        );
        admin.is_self = true;
        assert_eq!(channel_admin_state(&admin.into()), None);
    }

    #[test]
    fn member_offset_follows_server_rows_and_stops_when_no_progress_is_possible() {
        assert_eq!(member_next(40, 37, 120), Some(77));
        assert_eq!(member_next(40, 0, 120), None);
        assert_eq!(member_next(40, 37, 77), None);
        assert_eq!(member_next(i32::MAX, 1, i32::MAX), None);
    }

    fn test_invite(link: &str, revoked: bool) -> types::ChatInviteExported {
        types::ChatInviteExported {
            revoked,
            permanent: true,
            request_needed: false,
            link: link.into(),
            admin_id: 123,
            date: 100,
            start_date: None,
            expire_date: None,
            usage_limit: None,
            usage: None,
            requested: None,
            subscription_expired: None,
            title: None,
            subscription_pricing: None,
        }
    }

    #[test]
    fn exported_invites_advance_both_cursor_fields_despite_equal_dates() {
        let last = InviteCursor {
            date: 100,
            link: "second".into(),
        };
        let page = || types::messages::ExportedChatInvites {
            count: 5,
            invites: vec![
                test_invite("first", false).into(),
                test_invite("second", false).into(),
                enums::ExportedChatInvite::ChatInvitePublicJoinRequests,
            ],
            users: Vec::new(),
        };
        let previous = InviteCursor {
            date: 100,
            link: "previous".into(),
        };
        let first = exported_invite_page(page(), Some(&previous), false);
        assert_eq!(first.links.len(), 2);
        assert!(first.next.as_ref() == Some(&last));
        assert!(
            exported_invite_page(page(), Some(&last), false)
                .next
                .is_none()
        );
        assert!(exported_invite_page(page(), None, true).links.is_empty());
    }

    #[test]
    fn invite_writes_verify_link_url_and_owner_before_using_server_state() {
        let mut raw = test_invite("link", false);
        assert!(
            !owned_invite_link(raw.clone().into(), 123, "link")
                .unwrap()
                .revoked
        );
        assert!(owned_invite_link(raw.clone().into(), 123, "other").is_err());
        assert!(owned_invite_link(raw.clone().into(), 124, "link").is_err());
        raw.revoked = true;
        assert!(
            owned_invite_link(raw.clone().into(), 123, "link")
                .unwrap()
                .revoked
        );
        raw.subscription_pricing = Some(
            types::StarsSubscriptionPricing {
                period: 2592000,
                amount: 4,
            }
            .into(),
        );
        assert!(owned_invite_link(raw.into(), 123, "link").unwrap().paid);
    }

    #[test]
    fn moderation_distinguishes_bans_from_other_restrictions_and_leavers() {
        let build = |left, banned| {
            let mut rights = membership_banned_rights(banned);
            rights.send_plain = true;
            enums::ChannelParticipant::Banned(types::ChannelParticipantBanned {
                left,
                peer: types::PeerUser { user_id: 5 }.into(),
                kicked_by: 6,
                date: 0,
                banned_rights: rights.into(),
                rank: None,
            })
        };
        let id = PeerId::user(5).unwrap();
        assert_eq!(
            channel_member(&build(false, false)),
            Some((id, Some(MemberRole::Restricted)))
        );
        assert_eq!(
            channel_member(&build(true, false)),
            Some((id, Some(MemberRole::RestrictedFormer)))
        );
        assert_eq!(
            channel_member(&build(true, true)),
            Some((id, Some(MemberRole::Banned)))
        );
    }

    #[test]
    fn moderation_retries_are_idempotent_and_protect_privileged_participants() {
        use MemberChange::{Ban, Unban};
        use MemberRole::{Administrator, Banned, Member, Owner};

        assert!(channel_moderation_needed(Some(Member), Ban).unwrap());
        assert!(channel_moderation_needed(Some(Banned), Unban).unwrap());
        assert!(!channel_moderation_needed(Some(Banned), Ban).unwrap());
        assert!(!channel_moderation_needed(Some(Member), Unban).unwrap());
        assert!(!channel_moderation_needed(None, Unban).unwrap());
        assert!(channel_moderation_needed(None, Ban).is_err());
        for role in [Administrator, Owner] {
            assert!(channel_moderation_needed(Some(role), Ban).is_err());
            assert!(channel_moderation_needed(Some(role), Unban).is_err());
        }
    }

    #[test]
    fn bot_always_offers_a_standard_start_command() {
        let commands = bot_commands(4, None);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "/start");
    }

    #[test]
    fn chat_authorship_distinguishes_destination_from_explicit_sender() {
        let member = PeerId::user(5).unwrap();
        let group = PeerId::chat(7).unwrap();
        let channel = PeerId::channel(11).unwrap();
        let megagroup = PeerId::channel(13).unwrap();

        assert_eq!(
            chat_author_id(Some(channel), megagroup, false, false),
            Some(channel)
        );
        assert_eq!(
            chat_author_id(Some(megagroup), megagroup, false, true),
            Some(megagroup)
        );
        assert_eq!(chat_author_id(None, channel, true, false), Some(channel));
        assert_eq!(chat_author_id(None, group, false, false), Some(group));
        assert_eq!(chat_author_id(Some(member), group, false, false), None);
        assert_eq!(chat_author_id(Some(member), channel, true, false), None);
        assert_eq!(chat_author_id(None, group, false, true), None);
        assert_eq!(chat_author_id(None, member, false, false), None);
    }
    use grammers_client::sender::RpcError;

    #[test]
    fn phone_login_accepts_international_formatting_and_rejects_other_characters() {
        assert_eq!(
            normalize_phone("+1 (202) 555-0100").unwrap(),
            "+12025550100"
        );
        for invalid in [
            "2025550100",
            "+x5550100",
            "+1+2025550100",
            "+1234",
            "+1234567890123456",
        ] {
            assert!(normalize_phone(invalid).is_err());
        }
    }

    #[test]
    fn blocked_list_offsets_count_raw_entries_and_stop_on_complete_empty_or_final_pages() {
        assert_eq!(next_blocked_offset(0, 12, Some(86)), Some(12));
        assert_eq!(next_blocked_offset(12, 40, Some(86)), Some(52));
        assert_eq!(next_blocked_offset(52, 34, Some(86)), None);
        assert_eq!(next_blocked_offset(52, 2, None), None);
        assert_eq!(next_blocked_offset(52, 0, Some(86)), None);
        assert_eq!(next_blocked_offset(i32::MAX, 2, Some(i32::MAX)), None);
    }

    #[test]
    fn blocked_list_keeps_user_group_and_channel_ids_distinct() {
        let peers = [
            types::PeerUser { user_id: 23 }.into(),
            types::PeerChat { chat_id: 23 }.into(),
            types::PeerChannel { channel_id: 23 }.into(),
        ];
        assert_eq!(
            peers.map(|peer| raw_peer_id(&peer).and_then(PeerId::bot_api_dialog_id)),
            [Some(23), Some(-23), Some(-1_000_000_000_023)]
        );
    }

    fn synthetic_device(hash: i64, current: bool, active: i32) -> enums::Authorization {
        types::Authorization {
            current,
            official_app: true,
            password_pending: true,
            encrypted_requests_disabled: false,
            call_requests_disabled: false,
            unconfirmed: true,
            hash,
            device_model: "Synthetic <device>".into(),
            platform: "Linux".into(),
            system_version: "Demo OS".into(),
            api_id: 12345,
            app_name: "Demo & app".into(),
            app_version: "0.0".into(),
            date_created: 10,
            date_active: active,
            ip: "192.0.2.10".into(),
            country: "Demo country".into(),
            region: "Demo region".into(),
        }
        .into()
    }

    #[test]
    fn devices_preserve_details_order_current_first_and_keep_its_canonical_identity() {
        let snapshot = device_sessions_view(types::account::Authorizations {
            authorization_ttl_days: 180,
            authorizations: vec![
                synthetic_device(-3, false, 100),
                synthetic_device(7, false, 200),
                synthetic_device(7, true, 20),
                synthetic_device(2, false, 300),
                synthetic_device(-3, false, 90),
            ],
        });
        assert_eq!(snapshot.inactive_ttl_days, 180);
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![7, 2, -3]
        );
        let current = &snapshot.sessions[0];
        assert!(current.current && !current.can_terminate());
        assert_eq!(current.device, "Synthetic <device>");
        assert_eq!(current.application, "Demo & app");
        assert_eq!(current.app_version, "0.0");
        assert_eq!(current.platform, "Linux");
        assert_eq!(current.system_version, "Demo OS");
        assert_eq!((current.created, current.active), (10, 20));
        assert_eq!(current.ip, "192.0.2.10");
        assert_eq!(
            (current.country.as_str(), current.region.as_str()),
            ("Demo country", "Demo region")
        );
        assert!(current.official && current.password_pending && current.unconfirmed);
    }

    #[test]
    fn devices_reset_rejects_current_and_zero_ids_accepts_negative_ids_and_skips_absent_sessions() {
        let snapshot = device_sessions_view(types::account::Authorizations {
            authorization_ttl_days: 30,
            authorizations: vec![
                synthetic_device(7, true, 20),
                synthetic_device(-3, false, 100),
            ],
        });
        assert!(device_reset_request(&snapshot, 0).is_err());
        assert!(device_reset_request(&snapshot, 7).is_err());
        assert_eq!(
            device_reset_request(&snapshot, -3).unwrap().unwrap().hash,
            -3
        );
        assert!(device_reset_request(&snapshot, 123).unwrap().is_none());
        assert!(snapshot.sessions[1].can_terminate());
    }

    #[test]
    fn session_ttl_requests_preserve_supported_days_and_reject_invalid_or_unoffered_values() {
        for days in crate::model::SESSION_TTL_DAYS {
            assert_eq!(
                device_ttl_request(days).unwrap().authorization_ttl_days,
                days
            );
        }
        for days in [i32::MIN, -1, 0, 1, 14, 31, 366, i32::MAX] {
            assert!(device_ttl_request(days).is_err());
        }
    }

    #[test]
    fn devices_errors_explain_fresh_login_and_stale_session_without_logging_identifiers() {
        let error = |name: &str| {
            InvocationError::Rpc(RpcError {
                code: 406,
                name: name.into(),
                value: None,
                caused_by: None,
            })
        };
        assert!(
            device_change_error(error("FRESH_RESET_AUTHORISATION_FORBIDDEN"))
                .to_string()
                .contains("24 часа")
        );
        assert!(
            device_change_error(error("TTL_DAYS_INVALID"))
                .to_string()
                .contains("отклонил выбранный срок")
        );
        assert!(
            device_change_error(error("HASH_INVALID"))
                .to_string()
                .contains("Обновите список")
        );
        assert!(
            device_change_error(error("UNKNOWN_ERROR"))
                .to_string()
                .contains("UNKNOWN_ERROR")
        );
    }

    async fn idle_session_runner(
        path: &std::path::Path,
    ) -> (SessionRunner, Client, Arc<SqliteSession>) {
        let lock = crate::storage::lock_session_file(path, true).unwrap();
        let session = Arc::new(SqliteSession::open(path).await.unwrap());
        session
            .set_update_state(grammers_session::types::UpdateState::Primary {
                pts: 42,
                date: 1,
                seq: 1,
            })
            .await
            .unwrap();
        let SenderPool { runner, handle, .. } = SenderPool::new(session.clone(), 12345);
        let client = Client::new(handle);
        let (started, ready) = tokio::sync::oneshot::channel();
        let runner = SessionRunner::spawn(async move {
            let _lock = lock;
            let _ = started.send(());
            runner.run().await;
        });
        ready.await.unwrap();
        assert!(!runner.is_finished());
        assert_session_locked(path);
        (runner, client, session)
    }

    fn assert_session_locked(path: &std::path::Path) {
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
    }

    fn reacquire_session(path: &std::path::Path) -> std::fs::File {
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        second
            .try_lock()
            .expect("The sender still owns the session lock");
        second
    }

    async fn await_session_release(path: &std::path::Path) -> std::fs::File {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .unwrap();
                match file.try_lock() {
                    Ok(()) => return file,
                    Err(std::fs::TryLockError::WouldBlock) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(error) => panic!("Cannot lock synthetic session: {error}"),
                }
            }
        })
        .await
        .expect("The cancelled sender did not release its session")
    }

    #[test]
    fn missing_login_credentials_releases_the_session_before_returning_an_error() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("synthetic.session");
            let (runner, _client, _session) = idle_session_runner(&path).await;
            let error = runner
                .finish_startup(
                    Duration::from_secs(1),
                    initial_login_state(async { Ok(false) }, false),
                )
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("API hash"));
            let _lock = reacquire_session(&path);
            let reopened = SqliteSession::open(&path).await.unwrap();
            assert_eq!(reopened.updates_state().await.unwrap().pts, 42);
        });
    }

    #[test]
    fn startup_rpc_failure_and_timeout_allow_immediate_session_retry() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for timeout in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("synthetic.session");
                let (runner, _client, _session) = idle_session_runner(&path).await;
                let error = runner
                    .finish_startup(
                        Duration::from_millis(20),
                        initial_login_state(
                            async {
                                if timeout {
                                    std::future::pending().await
                                } else {
                                    Err(anyhow::anyhow!("Synthetic connection failure"))
                                }
                            },
                            true,
                        ),
                    )
                    .await
                    .err()
                    .unwrap();
                assert!(error.to_string().contains(if timeout {
                    "не ответил вовремя"
                } else {
                    "Synthetic connection failure"
                }));
                let _lock = reacquire_session(&path);
            }
        });
    }

    #[test]
    fn cancelling_startup_stops_its_sender_even_with_a_live_client_handle() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("synthetic.session");
            let (runner, _client, _session) = idle_session_runner(&path).await;
            let (started, ready) = tokio::sync::oneshot::channel();
            let startup = tokio::spawn(async move {
                runner
                    .finish_startup(
                        Duration::from_secs(60),
                        initial_login_state(
                            async {
                                let _ = started.send(());
                                std::future::pending().await
                            },
                            true,
                        ),
                    )
                    .await
            });
            ready.await.unwrap();
            assert_session_locked(&path);
            startup.abort();
            assert!(startup.await.err().unwrap().is_cancelled());
            let _lock = await_session_release(&path).await;
        });
    }

    #[test]
    fn successful_startup_keeps_the_session_until_its_last_owner_drops() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for (authorized, has_hash, step) in [
                (true, false, LoginStep::Authorized),
                (true, true, LoginStep::Authorized),
                (false, true, LoginStep::Phone),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("synthetic.session");
                let (runner, _client, _session) = idle_session_runner(&path).await;
                let (runner, state) = runner
                    .finish_startup(
                        Duration::from_secs(1),
                        initial_login_state(async { Ok(authorized) }, has_hash),
                    )
                    .await
                    .unwrap();
                assert_eq!(state.step(), step);
                let owner = Arc::new(runner);
                let second_owner = owner.clone();
                drop(owner);
                assert_session_locked(&path);
                assert!(!second_owner.is_finished());
                drop(second_owner);
                let _lock = await_session_release(&path).await;
            }
        });
    }

    #[test]
    fn playback_classification_uses_attributes_mime_and_document_names_but_excludes_ttl() {
        let media = |mime: &str, attributes: Vec<enums::DocumentAttribute>, ttl_seconds| {
            Media::Document(grammers_client::media::Document::from_raw_media(
                types::MessageMediaDocument {
                    nopremium: false,
                    spoiler: false,
                    video: false,
                    round: false,
                    voice: false,
                    document: Some(
                        types::Document {
                            id: 1,
                            access_hash: 0,
                            file_reference: vec![],
                            date: 0,
                            mime_type: mime.into(),
                            size: 4,
                            thumbs: None,
                            video_thumbs: None,
                            dc_id: 1,
                            attributes,
                        }
                        .into(),
                    ),
                    alt_documents: None,
                    video_cover: None,
                    video_timestamp: None,
                    ttl_seconds,
                },
            ))
        };
        assert_eq!(
            playable_media(&media("VIDEO/MP4", vec![], None)),
            Some(PlayableMedia::Video {
                width: 0,
                height: 0,
                duration: None,
                size: 4,
            })
        );
        assert_eq!(
            playable_media(&media("audio/ogg", vec![], None)),
            Some(PlayableMedia::Audio)
        );
        let voice = media(
            "application/ogg",
            vec![
                types::DocumentAttributeAudio {
                    voice: true,
                    duration: 35,
                    title: None,
                    performer: None,
                    waveform: Some(vec![0xc1, 0xff, 0x07]),
                }
                .into(),
            ],
            None,
        );
        assert_eq!(playable_media(&voice), Some(PlayableMedia::Voice));
        let audio = audio_info(&voice).unwrap();
        assert_eq!(audio.duration, Some(35));
        assert_eq!(audio.waveform, [1, 30, 31, 15]);
        assert_eq!(
            playable_media(&media(
                "application/octet-stream",
                vec![
                    types::DocumentAttributeFilename {
                        file_name: "sample.MP4".into()
                    }
                    .into()
                ],
                None
            )),
            Some(PlayableMedia::Video {
                width: 0,
                height: 0,
                duration: None,
                size: 4,
            })
        );
        let video = types::DocumentAttributeVideo {
            round_message: true,
            supports_streaming: true,
            nosound: false,
            duration: 8.0,
            w: 720,
            h: 1280,
            preload_prefix_size: None,
            video_start_ts: None,
            video_codec: None,
        };
        assert_eq!(
            playable_media(&media(
                "application/octet-stream",
                vec![video.clone().into()],
                None
            )),
            Some(PlayableMedia::Video {
                width: 720,
                height: 1280,
                duration: Some(8),
                size: 4,
            })
        );
        assert_eq!(
            playable_media(&media("video/mp4", vec![video.into()], Some(1))),
            None
        );
        assert_eq!(playable_media(&media("text/plain", vec![], None)), None);
    }

    #[test]
    fn telegram_link_ranges_follow_utf16_and_preserve_explicit_targets() {
        let text = "😀 ссылка";
        let links = incoming_links(
            text,
            &[
                types::MessageEntityTextUrl {
                    offset: 3,
                    length: 6,
                    url: "https://example.org/note".into(),
                }
                .into(),
                types::MessageEntityTextUrl {
                    offset: 1,
                    length: 1,
                    url: "https://example.org/invalid-offset".into(),
                }
                .into(),
            ],
        );
        assert_eq!(links.len(), 1);
        assert_eq!(&text[links[0].range.clone()], "ссылка");
        assert_eq!(links[0].uri, "https://example.org/note");
    }

    #[test]
    fn changing_folder_pins_preserves_all_other_settings_and_peer_types() {
        let user = |id, access_hash| {
            enums::InputPeer::User(types::InputPeerUser {
                user_id: id,
                access_hash,
            })
        };
        let channel: enums::InputPeer = types::InputPeerChannel {
            channel_id: 5,
            access_hash: 9,
        }
        .into();
        let mut filter: enums::DialogFilter = types::DialogFilter {
            contacts: true,
            non_contacts: true,
            groups: true,
            broadcasts: false,
            bots: true,
            exclude_muted: true,
            exclude_read: false,
            exclude_archived: true,
            title_noanimate: true,
            id: 4,
            title: types::TextWithEntities {
                text: "Проверочная папка".into(),
                entities: vec![],
            }
            .into(),
            emoticon: Some("🌿".into()),
            color: Some(3),
            pinned_peers: vec![user(5, 1), channel.clone(), enums::InputPeer::PeerSelf],
            include_peers: vec![user(8, 2)],
            exclude_peers: vec![user(9, 3)],
        }
        .into();
        let original = filter.clone();
        let self_id = PeerId::user(5);
        set_folder_pin(&mut filter, user(5, 4), true, self_id).unwrap();
        let mut expected = original.clone();
        let enums::DialogFilter::Filter(value) = &mut expected else {
            unreachable!()
        };
        value.pinned_peers = vec![user(5, 4), channel.clone()];
        assert_eq!(filter, expected);
        set_folder_pin(&mut filter, enums::InputPeer::PeerSelf, false, self_id).unwrap();
        let enums::DialogFilter::Filter(value) = &mut expected else {
            unreachable!()
        };
        value.pinned_peers = vec![channel];
        assert_eq!(filter, expected);
        assert!(set_folder_pin(&mut filter, enums::InputPeer::Empty, true, self_id).is_err());
        assert_eq!(filter, expected);
    }

    #[test]
    fn shared_folder_pins_keep_invites_title_and_included_chats() {
        let peer: enums::InputPeer = types::InputPeerChat { chat_id: 42 }.into();
        let mut filter: enums::DialogFilter = types::DialogFilterChatlist {
            has_my_invites: true,
            title_noanimate: true,
            id: 8,
            title: types::TextWithEntities {
                text: "Общая папка".into(),
                entities: vec![],
            }
            .into(),
            emoticon: Some("💬".into()),
            color: Some(1),
            pinned_peers: vec![],
            include_peers: vec![peer.clone()],
        }
        .into();
        let original = filter.clone();
        set_folder_pin(&mut filter, peer.clone(), true, None).unwrap();
        let mut expected = original.clone();
        let enums::DialogFilter::Chatlist(value) = &mut expected else {
            unreachable!()
        };
        value.pinned_peers = vec![peer.clone()];
        assert_eq!(filter, expected);
        set_folder_pin(&mut filter, peer, false, None).unwrap();
        assert_eq!(filter, original);
        assert!(
            set_folder_pin(
                &mut enums::DialogFilter::Default,
                types::InputPeerChat { chat_id: 42 }.into(),
                true,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn contacts_cache_expires_and_metadata_updates_invalidate_it() {
        let now = Instant::now();
        let cache = ContactSnapshot {
            loaded: now,
            revision: 3,
            contacts: Arc::new(Vec::new()),
        };
        assert!(cache.is_fresh(3, now + Duration::from_secs(299)));
        assert!(!cache.is_fresh(3, now + Duration::from_secs(300)));
        assert!(!cache.is_fresh(4, now));
    }

    #[test]
    fn profile_names_exclude_inactive_aliases_and_deduplicate_case_insensitively() {
        let alias = |name: &str, active| {
            types::Username {
                editable: true,
                active,
                deletable: false,
                expired: false,
                username: name.to_owned(),
            }
            .into()
        };
        let names = public_usernames(
            Some("Demo"),
            Some(&[
                alias("demo", true),
                alias("inactive_demo", false),
                alias("second_demo", true),
                alias("", true),
            ]),
        );
        assert_eq!(names, ["@Demo", "@second_demo"]);
        assert!(public_usernames(None, None).is_empty());
    }

    #[test]
    fn spoiler_entities_round_trip_and_hide_quoted_text() {
        let text = "😀видно: тайна.";
        let start = text.find("тайна").unwrap();
        let end = start + "тайна".len();
        let message = FormattedText {
            text: text.to_owned(),
            spans: vec![TextSpan {
                range: start..end,
                style: TextStyle::Spoiler,
            }],
        };
        let entities = outgoing_entities(&message);
        assert!(matches!(
            entities.as_slice(),
            [enums::MessageEntity::Spoiler(types::MessageEntitySpoiler {
                offset: 9,
                length: 5
            })]
        ));
        assert_eq!(incoming_styles(text, &entities), message.spans);
        assert_eq!(quote_preview(text, &entities), "😀видно: ▰▰▰.");
    }

    #[test]
    fn formatted_entities_use_utf16_and_keep_overlapping_styles() {
        let formatted = FormattedText {
            text: "  я😀ab  ".into(),
            spans: vec![
                TextSpan {
                    range: 2..9,
                    style: TextStyle::Bold,
                },
                TextSpan {
                    range: 4..12,
                    style: TextStyle::Italic,
                },
            ],
        }
        .trimmed();
        assert_eq!(formatted.text, "я😀ab");
        let entities = outgoing_entities(&formatted);
        assert_eq!(
            entities
                .iter()
                .map(|entity| (entity.offset(), entity.length()))
                .collect::<Vec<_>>(),
            [(0, 1), (1, 3), (1, 3), (4, 1)]
        );
        let decoded = incoming_styles(&formatted.text, &entities);
        assert_eq!(decoded[1].range, 2..7);
        assert_eq!(decoded[1].style, TextStyle::Bold);
        assert_eq!(decoded[2].style, TextStyle::Italic);
    }

    #[test]
    fn malformed_offsets_are_ignored_and_code_is_exclusive() {
        let invalid = types::MessageEntityBold {
            offset: 1,
            length: 1,
        };
        assert!(incoming_styles("😀", &[invalid.into()]).is_empty());
        let formatted = FormattedText {
            text: "😀код".into(),
            spans: vec![
                TextSpan {
                    range: 0..10,
                    style: TextStyle::Bold,
                },
                TextSpan {
                    range: 4..10,
                    style: TextStyle::Code,
                },
            ],
        };
        let entities = outgoing_entities(&formatted);
        assert!(matches!(
            entities.as_slice(),
            [
                enums::MessageEntity::Bold(types::MessageEntityBold {
                    offset: 0,
                    length: 2
                }),
                enums::MessageEntity::Code(types::MessageEntityCode {
                    offset: 2,
                    length: 3
                }),
            ]
        ));
    }

    #[test]
    fn notification_mute_expires_at_the_configured_second() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        assert!(is_muted(1_001, now));
        assert!(!is_muted(1_000, now));
        assert!(!is_muted(0, now));
        assert!(!is_muted(-1, now));
    }

    #[test]
    fn changing_mute_preserves_custom_notification_and_story_settings() {
        let settings = types::PeerNotifySettings {
            show_previews: Some(false),
            silent: Some(true),
            mute_until: None,
            ios_sound: None,
            android_sound: None,
            other_sound: Some(enums::NotificationSound::None),
            stories_muted: Some(true),
            stories_hide_sender: Some(true),
            stories_ios_sound: None,
            stories_android_sound: None,
            stories_other_sound: Some(enums::NotificationSound::Default),
        };
        for muted in [true, false] {
            let updated = notification_settings(settings.clone(), muted);
            assert_eq!(updated.show_previews, settings.show_previews);
            assert_eq!(updated.silent, settings.silent);
            assert_eq!(updated.sound, settings.other_sound);
            assert_eq!(updated.stories_muted, settings.stories_muted);
            assert_eq!(updated.stories_hide_sender, settings.stories_hide_sender);
            assert_eq!(updated.stories_sound, settings.stories_other_sound);
            assert_eq!(
                is_muted(
                    updated.mute_until.unwrap(),
                    UNIX_EPOCH + Duration::from_secs(1_000)
                ),
                muted
            );
        }
    }

    #[test]
    fn attachment_thumbnail_uses_a_supported_size_under_the_limit() {
        let size = |name: &str, side, bytes| {
            types::PhotoSize {
                r#type: name.to_owned(),
                w: side,
                h: side,
                size: bytes,
            }
            .into()
        };
        let photo = grammers_client::media::Photo::from_raw(
            types::Photo {
                has_stickers: false,
                id: 1,
                access_hash: 0,
                file_reference: Vec::new(),
                date: 0,
                sizes: vec![
                    size("s", 120, 20_000),
                    size("m", 320, 60_000),
                    size("x", 800, 90_000),
                    size("y", 400, 600_000),
                ],
                video_sizes: None,
                dc_id: 1,
            }
            .into(),
        );
        let selected = pick_thumbnail(photo.thumbs()).unwrap();
        assert_eq!(selected.photo_type(), "m");
    }

    #[test]
    fn download_retries_only_errors_that_require_a_fresh_file_reference() {
        for (name, should_retry) in [
            ("FILE_REFERENCE_EXPIRED", true),
            ("FILE_REFERENCE_INVALID", true),
            ("LOCATION_INVALID", false),
            ("CHANNEL_PRIVATE", false),
        ] {
            let error = InvocationError::Rpc(RpcError {
                code: 400,
                name: name.into(),
                value: None,
                caused_by: None,
            });
            assert_eq!(expired_file_reference(&error), should_retry);
        }
    }

    #[test]
    fn logout_only_accepts_explicit_ended_session_errors_as_confirmation() {
        for (code, name, ended) in [
            (401, "AUTH_KEY_UNREGISTERED", true),
            (401, "SESSION_EXPIRED", true),
            (401, "SESSION_REVOKED", true),
            (401, "AUTH_KEY_PERM_EMPTY", false),
            (500, "INTERNAL_SERVER_ERROR", false),
            (420, "FLOOD_WAIT", false),
        ] {
            let error = InvocationError::Rpc(RpcError {
                code,
                name: name.into(),
                value: None,
                caused_by: None,
            });
            assert_eq!(authorization_already_ended(&error), ended, "{name}");
        }
        assert!(!authorization_already_ended(&InvocationError::Io(
            std::io::ErrorKind::ConnectionReset.into()
        )));
    }

    #[test]
    fn update_retry_respects_server_delay_and_stops_on_revoked_session() {
        let backoff = Duration::from_secs(4);
        let flood_wait = InvocationError::Rpc(RpcError {
            code: 420,
            name: "FLOOD_WAIT".into(),
            value: Some(90),
            caused_by: None,
        });
        let offline = InvocationError::Io(std::io::ErrorKind::NotConnected.into());
        let revoked = InvocationError::Rpc(RpcError {
            code: 401,
            name: "SESSION_REVOKED".into(),
            value: None,
            caused_by: None,
        });
        assert!(retry_update_error(&flood_wait));
        assert_eq!(
            update_retry_delay(&flood_wait, backoff),
            Duration::from_secs(90)
        );
        assert!(retry_update_error(&offline));
        assert_eq!(update_retry_delay(&offline, backoff), backoff);
        assert!(!retry_update_error(&revoked));
    }
}
