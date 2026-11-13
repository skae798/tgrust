use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use grammers_client::client::{LoginToken, PasswordToken};
use grammers_client::media::{Downloadable, Media};
use grammers_client::message::Message as TelegramMessage;
use grammers_client::peer::Peer;
use grammers_client::sender::{SenderPool, UpdatesConfiguration};
use grammers_client::tl::enums;
use grammers_client::update::Update;
use grammers_client::{Client, SignInError};
use grammers_session::{storages::SqliteSession, types::PeerRef};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock, mpsc, watch};

use crate::model::{Chat, Message};
use crate::storage::{attachment_name, session_path};

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
    updates: Mutex<Option<mpsc::Receiver<grammers_session::updates::UpdatesLike>>>,
    api_hash: Option<String>,
    state: Mutex<LoginState>,
    peers: RwLock<HashMap<i64, PeerRef>>,
    changes: watch::Sender<u64>,
}

impl Telegram {
    pub async fn connect(api_id: i32, api_hash: Option<String>) -> Result<(Arc<Self>, LoginStep)> {
        let session = Arc::new(SqliteSession::open(session_path(api_id)?).await?);
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::new(session, api_id);
        let client = Client::new(handle);
        tokio::spawn(runner.run());
        let (state, step) = if client.is_authorized().await? {
            (LoginState::Authorized, LoginStep::Authorized)
        } else {
            ensure!(
                api_hash.is_some(),
                "Сессия истекла. Запустите tgrust с TGRUST_API_ID и TGRUST_API_HASH для повторного входа."
            );
            (LoginState::Phone, LoginStep::Phone)
        };
        let (changes, _) = watch::channel(0);
        Ok((
            Arc::new(Self {
                client,
                updates: Mutex::new(Some(updates)),
                api_hash,
                state: Mutex::new(state),
                peers: RwLock::new(HashMap::new()),
                changes,
            }),
            step,
        ))
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

    pub fn changes(&self) -> watch::Receiver<u64> {
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
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                update = updates.next() => {
                    let update = update?;
                    if matches!(update, Update::NewMessage(_) | Update::MessageEdited(_) | Update::MessageDeleted(_))
                        || matches!(update.raw(), enums::Update::ReadHistoryInbox(_) | enums::Update::ReadChannelInbox(_))
                    {
                        self.changes.send_modify(|version| *version = version.wrapping_add(1));
                    }
                }
            }
        }
        updates
            .sync_update_state()
            .await
            .map_err(anyhow::Error::from_boxed)
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
