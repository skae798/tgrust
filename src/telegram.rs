use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use grammers_client::client::{LoginToken, PasswordToken};
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
use tokio::sync::{Mutex, OnceCell, RwLock, Semaphore, mpsc, watch};

use crate::model::{
    AttachmentThumbnail, Avatar, Chat, ChatFolder, ChatImage, ChatKind, FormattedText,
    HISTORY_PAGE_SIZE, HistoryRevision, IncomingMessage, MAX_ALBUM_ITEMS, Message, Reaction,
    ReplyPreview, SearchHit, TextSpan, TextStyle, UpdateStatus, Upload, UploadKind,
};
use crate::storage::{PendingSave, attachment_name, open_session_file};

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

pub struct Telegram {
    client: Client,
    session: Arc<SqliteSession>,
    sender: SenderPoolHandle,
    runner: tokio::task::AbortHandle,
    updates: Mutex<Option<mpsc::Receiver<grammers_session::updates::UpdatesLike>>>,
    api_hash: Option<String>,
    state: Mutex<LoginState>,
    peers: RwLock<HashMap<i64, PeerRef>>,
    changes: watch::Sender<UpdateStatus>,
    history_changes: StdMutex<(u64, HashMap<i64, HistoryRevision>)>,
    image_downloads: Semaphore,
    profile: OnceCell<Peer>,
}

impl Telegram {
    pub async fn connect(api_id: i32, api_hash: Option<String>) -> Result<(Arc<Self>, LoginStep)> {
        let (path, lock) = open_session_file(api_id, api_hash.is_some())?;
        let session = Arc::new(
            SqliteSession::open(path)
                .await
                .context("Не удалось открыть базу сессии")?,
        );
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::new(session.clone(), api_id);
        let sender = (*handle).clone();
        let client = Client::new(handle);
        let runner = tokio::spawn(async move {
            let _lock = lock;
            runner.run().await;
        });
        let authorized = match tokio::time::timeout(Duration::from_secs(20), client.is_authorized())
            .await
            .context("Telegram не ответил вовремя")
            .and_then(|result| result.map_err(anyhow::Error::from))
        {
            Ok(authorized) => authorized,
            Err(error) => {
                runner.abort();
                let _ = runner.await;
                return Err(error);
            }
        };
        let (state, step) = if authorized {
            (LoginState::Authorized, LoginStep::Authorized)
        } else {
            ensure!(
                api_hash.is_some(),
                "Сессия истекла. Запустите tgrust с TGRUST_API_ID и TGRUST_API_HASH для повторного входа."
            );
            (LoginState::Phone, LoginStep::Phone)
        };
        let (changes, _) = watch::channel(UpdateStatus::default());
        Ok((
            Arc::new(Self {
                client,
                session,
                sender,
                runner: runner.abort_handle(),
                updates: Mutex::new(Some(updates)),
                api_hash,
                state: Mutex::new(state),
                peers: RwLock::new(HashMap::new()),
                changes,
                history_changes: StdMutex::default(),
                image_downloads: Semaphore::new(4),
                profile: OnceCell::new(),
            }),
            step,
        ))
    }

    pub async fn login_step(&self) -> LoginStep {
        match &*self.state.lock().await {
            LoginState::Phone => LoginStep::Phone,
            LoginState::Code(_) => LoginStep::Code,
            LoginState::Password(_) => LoginStep::Password,
            LoginState::Authorized => LoginStep::Authorized,
        }
    }

    pub async fn login(&self, value: &str) -> Result<LoginStep> {
        let mut state = self.state.lock().await;
        match &*state {
            LoginState::Phone => {
                ensure!(
                    value.starts_with('+')
                        && value.len() > 1
                        && value[1..].bytes().all(|c| c.is_ascii_digit()),
                    "Введите номер в международном формате, например +380…"
                );
                let api_hash = self
                    .api_hash
                    .as_deref()
                    .context("Для входа нужен API hash")?;
                let token = self.client.request_login_code(value, api_hash).await?;
                *state = LoginState::Code(token);
                Ok(LoginStep::Code)
            }
            LoginState::Code(token) => match self.client.sign_in(token, value).await {
                Ok(user) => {
                    let _ = self.profile.set(Peer::User(user));
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
                        let _ = self.profile.set(Peer::User(user));
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

    pub async fn listen(
        self: Arc<Self>,
        mut stop: watch::Receiver<bool>,
        incoming: mpsc::Sender<IncomingMessage>,
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
                    if let Update::NewMessage(message) = &update
                        && let Some(notification) = notification_for(message, started)
                    {
                        let _ = incoming.try_send(notification);
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
                            | enums::Update::NotifySettings(_)
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
        let now = SystemTime::now();
        let mut chats = Vec::new();
        let mut peers = HashMap::new();
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
            chats.push(Chat {
                id,
                title: title.to_owned(),
                preview: last.map(preview).unwrap_or_default(),
                preview_sender: if group {
                    last.and_then(|message| preview_sender(message, profile))
                } else {
                    None
                },
                preview_image: last.and_then(|message| {
                    let (media, _) = attachment(message)?;
                    thumbnail(&media).map(|media| ChatImage::attachment(id, message.id(), media))
                }),
                group,
                unread: raw.unread_count.max(i32::from(raw.unread_mark)),
                broadcast: matches!(peer, Peer::Channel(_)),
                saved_messages: matches!(peer, Peer::User(user) if user.is_self()),
                avatar: peer_avatar(peer),
                archived: raw.folder_id == Some(1),
                kind: chat_kind(peer),
                muted: match &raw.notify_settings {
                    enums::PeerNotifySettings::Settings(settings) => settings
                        .mute_until
                        .is_some_and(|until| is_muted(until, now)),
                },
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

    pub async fn notification_enabled(&self, chat: i64, kind: ChatKind) -> Result<bool> {
        use grammers_client::tl::functions::account::GetNotifySettings;

        let id = PeerId::from_bot_api_dialog_id(chat).context("Чат не найден")?;
        let peer = self.session.peer_ref(id).await?.context("Чат не найден")?;
        let enums::PeerNotifySettings::Settings(settings) = self
            .client
            .invoke(&GetNotifySettings {
                peer: types::InputNotifyPeer { peer: peer.into() }.into(),
            })
            .await?;
        let mute_until = match settings.mute_until {
            Some(until) => until,
            None => {
                let category = match kind {
                    ChatKind::Group => enums::InputNotifyPeer::InputNotifyChats,
                    ChatKind::Broadcast => enums::InputNotifyPeer::InputNotifyBroadcasts,
                    ChatKind::Contact | ChatKind::User | ChatKind::Bot => {
                        enums::InputNotifyPeer::InputNotifyUsers
                    }
                };
                let enums::PeerNotifySettings::Settings(defaults) = self
                    .client
                    .invoke(&GetNotifySettings { peer: category })
                    .await?;
                defaults.mute_until.unwrap_or_default()
            }
        };
        Ok(!is_muted(mute_until, SystemTime::now()))
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

    async fn profile(&self) -> Option<&Peer> {
        self.profile
            .get_or_try_init(|| async {
                let user =
                    tokio::time::timeout(Duration::from_secs(5), self.client.get_me()).await??;
                Ok::<_, anyhow::Error>(Peer::User(user))
            })
            .await
            .ok()
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
        let self_id = self.self_id().await?;
        let profile = self.profile().await;
        let mut messages = self.client.search_all_messages().query(query).limit(50);
        let mut hits = Vec::new();
        let mut peers = HashMap::new();
        while let Some(message) = messages.next().await? {
            let Some(id) = message.peer_id().bot_api_dialog_id() else {
                continue;
            };
            let Some(peer) = message.peer() else { continue };
            let Some(peer_ref) = message
                .peer_ref()
                .await
                .map_err(anyhow::Error::from_boxed)?
            else {
                continue;
            };
            peers.insert(id, peer_ref);
            hits.push(SearchHit {
                chat: Chat {
                    id,
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
                    unread: 0,
                    broadcast: matches!(peer, Peer::Channel(_)),
                    saved_messages: Some(peer_ref.id) == self_id
                        || matches!(peer, Peer::User(user) if user.is_self()),
                    avatar: peer_avatar(peer),
                    archived: false,
                    kind: chat_kind(peer),
                    muted: false,
                },
                message: message_view(&message, self_id, profile),
            });
        }
        self.peers.write().await.extend(peers);
        Ok(hits)
    }

    pub async fn history(
        &self,
        id: i64,
        before: Option<i32>,
        mark_read: bool,
    ) -> Result<(Vec<Message>, bool, bool)> {
        let peer = self.peer(id).await?;
        let self_id = self.self_id().await?;
        let profile = self.profile().await;
        let mut iter = self
            .client
            .iter_messages(peer)
            .limit(HISTORY_PAGE_SIZE + MAX_ALBUM_ITEMS);
        if let Some(before) = before {
            iter = iter.offset_id(before);
        }
        let mut result = Vec::new();
        let newest = iter.next().await?;
        if let Some(message) = &newest {
            result.push(message_view(message, self_id, profile));
        }
        let mut has_older = false;
        while let Some(message) = iter.next().await? {
            // Keep the album at the pagination boundary together.
            if result.len() >= HISTORY_PAGE_SIZE
                && (message.grouped_id().is_none()
                    || result.last().and_then(|item| item.album) != message.grouped_id())
            {
                has_older = true;
                break;
            }
            result.push(message_view(&message, self_id, profile));
        }
        let marked = if mark_read && let Some(message) = newest {
            message.mark_as_read().await.is_ok()
        } else {
            false
        };
        result.reverse();
        self.load_reply_previews(peer, &mut result, self_id, profile)
            .await;
        Ok((result, has_older, marked))
    }

    pub async fn mark_read_through(&self, chat: i64, max_id: i32) -> Result<()> {
        let peer = self.peer(chat).await?;
        if peer.id.kind() == PeerKind::Channel {
            self.client
                .invoke(&grammers_client::tl::functions::channels::ReadHistory {
                    channel: peer.into(),
                    max_id,
                })
                .await?;
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
        let original = self
            .message_by_id(peer, anchor)
            .await?
            .context("Сообщение больше недоступно")?;
        let (mut messages, older, _) = self.history(chat, Some(anchor), false).await?;
        messages.push(message_view(&original, self_id, profile));
        let mut newer = self
            .client
            .iter_messages(peer)
            .offset_id(anchor)
            .reverse(true)
            .limit(HISTORY_PAGE_SIZE / 2);
        while let Some(message) = newer.next().await? {
            if message.id() > anchor {
                messages.push(message_view(&message, self_id, profile));
            }
        }
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
        let previews: HashMap<_, _> = originals.into_iter().flatten()
            .filter(|message| matches!(&message.raw, enums::Message::Message(raw) if raw.ttl_period.is_none()))
            .map(|message| {
                let original = message_view(&message, self_id, profile);
                (original.id, original.reply_preview())
            }).collect();
        for message in messages {
            if let Some(original) = message.reply_to.and_then(|id| previews.get(&id)) {
                let preview = message
                    .reply_preview
                    .get_or_insert_with(|| original.clone());
                preview.sender.clone_from(&original.sender);
            }
        }
    }

    pub async fn send(
        &self,
        id: i64,
        text: FormattedText,
        uploads: Vec<Upload>,
        reply_to: Option<i32>,
    ) -> Result<Vec<Message>> {
        let peer = self.peer(id).await?;
        let self_id = self.self_id().await?;
        let profile = self.profile().await;
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
                        .reply_to(reply_to);
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
                .map(|message| message_view(&message, self_id, profile))
                .collect());
        }

        let mut message = InputMessage::new()
            .text(text.text)
            .fmt_entities(entities)
            .reply_to(reply_to);
        if let Some(upload) = uploads.into_iter().next() {
            let kind = upload.kind;
            let uploaded = self.upload_file(upload).await?;
            message = match kind {
                UploadKind::File => message.file(uploaded),
                UploadKind::Photo => message.photo(uploaded),
            };
        }
        let message = self.client.send_message(peer, message).await?;
        Ok(vec![message_view(&message, self_id, profile)])
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

    pub async fn forward_message(&self, source: i64, id: i32, destination: i64) -> Result<Message> {
        let self_id = self.self_id().await?;
        let profile = self.profile().await;
        let source = self.peer(source).await?;
        let destination = self.peer(destination).await?;
        let original = self
            .message_by_id(source, id)
            .await?
            .context("Сообщение больше недоступно")?;
        ensure!(can_forward(&original), "Это сообщение нельзя переслать");
        let message = self
            .client
            .forward_messages(destination, &[id], source)
            .await?
            .into_iter()
            .next()
            .flatten()
            .context("Telegram не вернул пересланное сообщение")?;
        Ok(message_view(&message, self_id, profile))
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
        _ => None,
    }
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

fn chat_kind(peer: &Peer) -> ChatKind {
    match peer {
        Peer::User(user) if user.is_bot() => ChatKind::Bot,
        Peer::User(user) if user.contact() => ChatKind::Contact,
        Peer::User(_) => ChatKind::User,
        Peer::Channel(_) => ChatKind::Broadcast,
        Peer::Group(_) | Peer::Community(_) => ChatKind::Group,
    }
}

fn folder_view(folder: enums::DialogFilter, self_id: Option<PeerId>) -> Option<ChatFolder> {
    fn peers(peers: Vec<enums::InputPeer>, self_id: Option<PeerId>) -> Vec<i64> {
        peers
            .into_iter()
            .filter_map(|peer| {
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
            })
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
) -> Message {
    let outgoing = message.outgoing();
    let attachment = attachment(message);
    let photo_preview = matches!(attachment, Some((Media::Photo(_), _)));
    let thumbnail = attachment.as_ref().and_then(|(media, _)| thumbnail(media));
    let (reply_to, reply_preview) = match &message.raw {
        enums::Message::Message(raw) => match &raw.reply_to {
            Some(enums::MessageReplyHeader::Header(header)) if !header.reply_to_ephemeral => {
                let same_chat = header
                    .reply_to_peer_id
                    .as_ref()
                    .is_none_or(|peer| PeerId::from(peer.clone()) == message.peer_id());
                let id = header.reply_to_msg_id.filter(|_| same_chat);
                let quote = header
                    .quote_text
                    .as_ref()
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| ReplyPreview {
                        sender: None,
                        text: text.clone(),
                    });
                (id, quote)
            }
            _ => (None, None),
        },
        _ => (None, None),
    };
    Message {
        id: message.id(),
        sender: author_name(message, profile).unwrap_or_else(|| {
            if outgoing {
                "Вы"
            } else {
                "Сообщение"
            }
            .to_owned()
        }),
        sender_avatar: author_peer(message, profile).and_then(peer_avatar),
        timestamp: message.date().as_second(),
        text: if attachment.is_some() {
            message.text().to_owned()
        } else {
            preview(message)
        },
        styles: incoming_styles(
            message.text(),
            message.fmt_entities().map_or(&[], Vec::as_slice),
        ),
        outgoing,
        attachment: attachment.map(|(_, name)| name),
        photo_preview,
        thumbnail,
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
                _ => false,
            },
    }
}

fn author_peer<'a>(message: &'a TelegramMessage, profile: Option<&'a Peer>) -> Option<&'a Peer> {
    message.sender().or_else(|| {
        let sender = message.sender_id();
        if sender == Some(message.peer_id()) || (sender.is_none() && message.post()) {
            message.peer()
        } else if message.outgoing() && sender.is_none_or(|id| id.kind() == PeerKind::User) {
            profile
        } else {
            None
        }
    })
}

fn author_name(message: &TelegramMessage, profile: Option<&Peer>) -> Option<String> {
    match author_peer(message, profile)? {
        Peer::User(user) => Some(user.full_name()).filter(|name| !name.is_empty()),
        peer => peer
            .name()
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
    }
}

fn preview_sender(message: &TelegramMessage, profile: Option<&Peer>) -> Option<String> {
    if message.action().is_some() {
        return None;
    }
    author_name(message, profile)
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
            _ => true,
        }
}

fn incoming_styles(text: &str, entities: &[enums::MessageEntity]) -> Vec<TextSpan> {
    use enums::MessageEntity as Entity;

    if entities.is_empty() {
        return Vec::new();
    }
    let mut utf16 = 0;
    let mut boundaries = HashMap::new();
    for (byte, ch) in text.char_indices() {
        boundaries.insert(utf16, byte);
        utf16 += ch.len_utf16() as i32;
    }
    boundaries.insert(utf16, text.len());
    let mut spans: Vec<_> = entities
        .iter()
        .filter_map(|entity| {
            let style = match entity {
                Entity::Bold(_) => TextStyle::Bold,
                Entity::Italic(_) => TextStyle::Italic,
                Entity::Underline(_) => TextStyle::Underline,
                Entity::Strike(_) => TextStyle::Strike,
                Entity::Code(_) | Entity::Pre(_) => TextStyle::Code,
                _ => return None,
            };
            let start = *boundaries.get(&entity.offset())?;
            let end = *boundaries.get(&entity.offset().checked_add(entity.length())?)?;
            (start < end).then_some(TextSpan {
                range: start..end,
                style,
            })
        })
        .collect();
    spans.sort_by_key(|span| (span.range.start, span.range.end, span.style));
    spans
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

fn preview(message: &TelegramMessage) -> String {
    if !message.text().is_empty() {
        message.text().to_owned()
    } else if message.action().is_some() {
        "Событие в чате".to_owned()
    } else if message.media().is_some() {
        "Вложение".to_owned()
    } else {
        String::new()
    }
}

fn is_muted(until: i32, now: SystemTime) -> bool {
    u64::try_from(until)
        .is_ok_and(|until| until > now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
}

fn notification_for(message: &TelegramMessage, started: SystemTime) -> Option<IncomingMessage> {
    let enums::Message::Message(raw) = &message.raw else {
        return None;
    };
    let timestamp = UNIX_EPOCH + Duration::from_secs(u64::try_from(raw.date).ok()?);
    if raw.out
        || raw.silent
        || raw.ttl_period.is_some()
        || matches!(&raw.media, Some(enums::MessageMedia::Photo(photo)) if photo.ttl_seconds.is_some())
        || matches!(&raw.media, Some(enums::MessageMedia::Document(document)) if document.ttl_seconds.is_some())
        || timestamp.duration_since(UNIX_EPOCH).ok()?.as_secs()
            < started.duration_since(UNIX_EPOCH).ok()?.as_secs()
        || SystemTime::now()
            .duration_since(timestamp)
            .is_ok_and(|age| age > Duration::from_secs(60))
    {
        return None;
    }
    let peer = message.peer()?;
    if matches!(peer, Peer::User(user) if user.is_self()) {
        return None;
    }
    Some(IncomingMessage {
        chat: message.peer_id().bot_api_dialog_id()?,
        message: message.id(),
        title: peer.name().unwrap_or("Telegram").to_owned(),
        kind: chat_kind(peer),
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
    use grammers_client::sender::RpcError;

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
