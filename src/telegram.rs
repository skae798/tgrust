use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use grammers_client::client::{LoginToken, PasswordToken};
use grammers_client::media::{Downloadable, Media};
use grammers_client::message::Message as TelegramMessage;
use grammers_client::peer::Peer;
use grammers_client::sender::{SenderPool, SenderPoolHandle, UpdatesConfiguration};
use grammers_client::tl::enums;
use grammers_client::update::Update;
use grammers_client::{Client, InvocationError, SignInError};
use grammers_session::{Session, storages::SqliteSession, types::PeerRef};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock, mpsc, watch};

use crate::model::{Chat, Message, UpdateStatus};
use crate::storage::{attachment_name, open_session_file};

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
                        enums::Update::ReadHistoryInbox(_) | enums::Update::ReadChannelInbox(_)
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

    pub async fn history(
        &self,
        id: i64,
        before: Option<i32>,
        mark_read: bool,
    ) -> Result<(Vec<Message>, bool)> {
        let peer = self.peer(id).await?;
        let mut iter = self.client.iter_messages(peer).limit(PAGE_SIZE);
        if let Some(before) = before {
            iter = iter.offset_id(before);
        }
        let mut result = Vec::new();
        let newest = iter.next().await?;
        if let Some(message) = &newest {
            result.push(message_view(message));
        }
        while let Some(message) = iter.next().await? {
            result.push(message_view(&message));
        }
        let marked = if mark_read && let Some(message) = newest {
            message.mark_as_read().await.is_ok()
        } else {
            false
        };
        result.reverse();
        Ok((result, marked))
    }

    pub async fn send(&self, id: i64, text: String) -> Result<Message> {
        let message = self.client.send_message(self.peer(id).await?, text).await?;
        Ok(message_view(&message))
    }

    pub async fn save(&self, chat: i64, id: i32, destination: PathBuf) -> Result<()> {
        ensure!(!destination.exists(), "Файл с таким именем уже существует");
        let message = self
            .client
            .get_messages_by_id(self.peer(chat).await?, &[id])
            .await?
            .into_iter()
            .next()
            .flatten()
            .context("Сообщение больше недоступно")?;
        let (media, _) = attachment(&message).context("Вложение больше недоступно")?;
        let directory = destination.parent().context("У файла нет каталога")?;
        let temporary = tempfile::Builder::new()
            .prefix(".tgrust-")
            .tempfile_in(directory)?;
        let mut output = tokio::fs::File::from_std(temporary.reopen()?);
        let mut download = self.client.iter_download(&media);
        let mut bytes = 0;
        while let Some(chunk) = download.next().await? {
            output.write_all(&chunk).await?;
            bytes += chunk.len();
        }
        ensure!(
            media.size().is_none_or(|expected| expected == bytes),
            "Вложение загружено не полностью"
        );
        output.sync_all().await?;
        drop(output);
        temporary.persist_noclobber(destination)?;
        Ok(())
    }
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
        attachment: attachment(message).map(|(_, name)| name),
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
