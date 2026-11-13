use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use grammers_client::client::{LoginToken, PasswordToken};
use grammers_client::media::{Downloadable, Media, PhotoSize};
use grammers_client::message::{InputMessage, Message as TelegramMessage};
use grammers_client::peer::Peer;
use grammers_client::sender::{SenderPool, SenderPoolHandle, UpdatesConfiguration};
use grammers_client::tl::{enums, types};
use grammers_client::update::Update;
use grammers_client::{Client, InvocationError, SignInError};
use grammers_session::{Session, storages::SqliteSession, types::PeerRef};
use tokio::sync::{Mutex, RwLock, mpsc, watch};

use crate::model::{Chat, Message, Reaction, SearchHit, UpdateStatus, Upload, UploadKind};
use crate::storage::{PendingSave, attachment_name, open_session_file};

pub const PAGE_SIZE: usize = 40;

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
    api_hash: String,
    state: Mutex<LoginState>,
    peers: RwLock<HashMap<i64, PeerRef>>,
    changes: watch::Sender<UpdateStatus>,
}

impl Telegram {
    pub async fn connect(api_id: i32, api_hash: String) -> Result<(Arc<Self>, LoginStep)> {
        let (path, lock) = open_session_file(api_id)?;
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
                let token = self
                    .client
                    .request_login_code(value, &self.api_hash)
                    .await?;
                *state = LoginState::Code(token);
                Ok(LoginStep::Code)
            }
            LoginState::Code(token) => match self.client.sign_in(token, value).await {
                Ok(_) => {
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
                    Ok(_) => {
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

    pub async fn listen(self: Arc<Self>, mut stop: watch::Receiver<bool>) -> Result<()> {
        let Some(receiver) = self.updates.lock().await.take() else {
            return Ok(());
        };
        let mut updates = self
            .client
            .stream_updates(receiver, UpdatesConfiguration { catch_up: true })
            .await
            .map_err(anyhow::Error::from_boxed)?;
        let mut retry_delay = Duration::from_secs(1);
        let result = 'listen: loop {
            let result = tokio::select! {
                _ = stop.changed() => break Ok(()),
                update = updates.next() => update,
            };
            match result {
                Ok(update) => {
                    retry_delay = Duration::from_secs(1);
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
                    ) {
                        self.invalidate_views();
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
                    self.changes.send_modify(|status| {
                        status.reconnecting = false;
                        status.revision = status.revision.wrapping_add(1);
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

    pub async fn chats(&self) -> Result<Vec<Chat>> {
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
            chats.push(Chat {
                id,
                title: title.to_owned(),
                preview: dialog
                    .last_message
                    .as_ref()
                    .map(preview)
                    .unwrap_or_default(),
                unread: raw.unread_count,
                broadcast: matches!(peer, Peer::Channel(_)),
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

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
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
                    unread: 0,
                    broadcast: matches!(peer, Peer::Channel(_)),
                },
                message: message_view(&message),
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
        let mut iter = self.client.iter_messages(peer).limit(PAGE_SIZE + 1);
        if let Some(before) = before {
            iter = iter.offset_id(before);
        }
        let mut result = Vec::new();
        let newest = iter.next().await?;
        if let Some(message) = &newest {
            result.push(message_view(message));
        }
        while result.len() < PAGE_SIZE {
            let Some(message) = iter.next().await? else {
                break;
            };
            result.push(message_view(&message));
        }
        let has_older = iter.next().await?.is_some();
        let marked = if mark_read && let Some(message) = newest {
            message.mark_as_read().await.is_ok()
        } else {
            false
        };
        result.reverse();
        Ok((result, has_older, marked))
    }

    pub async fn history_around(&self, chat: i64, anchor: i32) -> Result<(Vec<Message>, bool)> {
        let peer = self.peer(chat).await?;
        let original = self
            .message_by_id(peer, anchor)
            .await?
            .context("Сообщение больше недоступно")?;
        let (mut messages, older, _) = self.history(chat, Some(anchor), false).await?;
        messages.push(message_view(&original));
        let mut newer = self
            .client
            .iter_messages(peer)
            .offset_id(anchor)
            .reverse(true)
            .limit(PAGE_SIZE / 2);
        while let Some(message) = newer.next().await? {
            if message.id() > anchor {
                messages.push(message_view(&message));
            }
        }
        Ok((messages, older))
    }

    pub async fn send(
        &self,
        id: i64,
        text: String,
        upload: Option<Upload>,
        reply_to: Option<i32>,
    ) -> Result<Message> {
        let peer = self.peer(id).await?;
        let mut message = InputMessage::new().text(text).reply_to(reply_to);
        if let Some(upload) = upload {
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
            use tokio::io::AsyncReadExt;
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
            message = match upload.kind {
                UploadKind::File => message.file(uploaded),
                UploadKind::Photo => message.photo(uploaded),
            };
        }
        let message = self.client.send_message(peer, message).await?;
        Ok(message_view(&message))
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

    pub async fn edit_text(&self, chat: i64, id: i32, text: String) -> Result<()> {
        let peer = self.peer(chat).await?;
        let message = self
            .message_by_id(peer, id)
            .await?
            .context("Сообщение больше недоступно")?;
        ensure!(can_edit_text(&message), "Это сообщение нельзя изменить");
        let link_preview = matches!(message.media(), Some(Media::WebPage(_)));
        let edit = self.client.edit_message(
            peer,
            id,
            InputMessage::new().text(text).link_preview(link_preview),
        );
        match edit.await {
            Err(error) if error.is("MESSAGE_NOT_MODIFIED") => Ok(()),
            result => Ok(result?),
        }
    }

    pub async fn delete_message(&self, chat: i64, id: i32) -> Result<()> {
        let peer = self.peer(chat).await?;
        let Some(message) = self.message_by_id(peer, id).await? else {
            return Ok(());
        };
        ensure!(
            message.outgoing() && matches!(message.raw, enums::Message::Message(_)),
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
        Ok(message_view(&message))
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

#[derive(PartialEq, Eq)]
enum AttachmentId {
    Photo(i64),
    Document(i64),
}

fn attachment_id(media: &Media) -> Option<AttachmentId> {
    match media {
        Media::Photo(photo) => Some(AttachmentId::Photo(photo.id())),
        Media::Document(document) => Some(AttachmentId::Document(document.id())),
        _ => None,
    }
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

fn message_view(message: &TelegramMessage) -> Message {
    let outgoing = message.outgoing();
    let attachment = attachment(message);
    let photo_preview = matches!(attachment, Some((Media::Photo(_), _)));
    Message {
        id: message.id(),
        sender: if outgoing {
            "Вы".to_owned()
        } else {
            message
                .sender()
                .and_then(Peer::name)
                .unwrap_or("Сообщение")
                .to_owned()
        },
        timestamp: message.date().as_second(),
        text: preview(message),
        outgoing,
        attachment: attachment.map(|(_, name)| name),
        photo_preview,
        reply_to: match &message.raw {
            enums::Message::Message(raw)
                if matches!(
                    &raw.reply_to,
                    Some(enums::MessageReplyHeader::Header(header)) if header.reply_to_peer_id.is_none()
                ) =>
            {
                message.reply_to_message_id()
            }
            _ => None,
        },
        can_reply: matches!(message.raw, enums::Message::Message(_)),
        can_edit: can_edit_text(message),
        can_delete: outgoing && matches!(message.raw, enums::Message::Message(_)),
        can_forward: can_forward(message),
        forwarded: matches!(&message.raw, enums::Message::Message(raw) if raw.fwd_from.is_some()),
        pinned: message.pinned(),
        reactions: message_reactions(message),
        edited: message.edit_date().is_some() && !message.edit_hide(),
    }
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

fn can_edit_text(message: &TelegramMessage) -> bool {
    let enums::Message::Message(raw) = &message.raw else {
        return false;
    };
    message.outgoing()
        && !message.text().is_empty()
        && raw.fwd_from.is_none()
        && raw.via_bot_id.is_none()
        && raw.ttl_period.is_none()
        && (raw.entities.as_ref().is_none_or(Vec::is_empty))
        && matches!(
            raw.media,
            None | Some(enums::MessageMedia::Empty | enums::MessageMedia::WebPage(_))
        )
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
