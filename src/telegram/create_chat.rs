use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use grammers_client::tl::functions::{
    channels::{CheckUsername, CreateChannel, InviteToChannel, UpdateUsername},
    messages::{CreateChat, EditChatAbout},
};
use grammers_client::tl::{Deserializable, RemoteCall};

use super::{InvocationError, Peer, PeerId, PeerKind, PeerRef, Session, Telegram, enums};
use crate::model::{CreatedChat, NewChatDraft, NewChatKind, NewChatResult, chat_username};

pub(super) const RPC_TIMEOUT: Duration = Duration::from_secs(20);

impl Telegram {
    pub async fn create_chat(&self, mut draft: NewChatDraft) -> NewChatResult {
        draft = match draft.validate() {
            Ok(draft) => draft,
            Err(error) => return NewChatResult::Rejected(error.to_string()),
        };
        let _serial = self.dialog_settings.lock().await;
        let users = match self.creation_users(&draft).await {
            Ok(users) => users,
            Err(error) => return NewChatResult::Rejected(error.to_string()),
        };
        if !draft.username.is_empty() {
            let check = tokio::time::timeout(
                RPC_TIMEOUT,
                self.client.invoke(&CheckUsername {
                    channel: enums::InputChannel::Empty,
                    username: draft.username.clone(),
                }),
            )
            .await;
            match check {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    return NewChatResult::Rejected("Это публичное имя занято.".into());
                }
                Ok(Err(error)) => return NewChatResult::Rejected(username_error(error)),
                Err(_) => {
                    return NewChatResult::Rejected(
                        "Telegram не успел проверить публичное имя.".into(),
                    );
                }
            }
        }
        let creation = match draft.kind {
            NewChatKind::Group => self
                .create_once(&CreateChat {
                    users: users.clone(),
                    title: draft.info.title.clone(),
                    ttl_period: None,
                })
                .await
                .map(|enums::messages::InvitedUsers::Users(result)| {
                    (result.updates, result.missing_invitees)
                }),
            NewChatKind::Supergroup | NewChatKind::Channel => self
                .create_once(&CreateChannel {
                    broadcast: draft.kind == NewChatKind::Channel,
                    megagroup: draft.kind == NewChatKind::Supergroup,
                    for_import: false,
                    forum: false,
                    title: draft.info.title.clone(),
                    about: draft.info.about.clone(),
                    geo_point: None,
                    address: None,
                    ttl_period: None,
                })
                .await
                .map(|updates| (updates, Vec::new())),
        };
        let (updates, mut missing) = match creation {
            Ok(result) => result,
            Err(failure) => {
                if matches!(failure, NewChatResult::Unconfirmed(_)) {
                    self.invalidate_views();
                }
                return failure;
            }
        };
        self.invalidate_views();
        let reference = match self.created_chat_reference(updates, &draft).await {
            Ok(reference) => reference,
            Err(error) => return NewChatResult::Unconfirmed(error.to_string()),
        };
        let Some(id) = reference.id.bot_api_dialog_id() else {
            return NewChatResult::Unconfirmed(
                "Telegram вернул неизвестный идентификатор чата.".into(),
            );
        };
        self.peers.write().await.insert(id, reference);

        let mut warnings = Vec::new();
        if draft.kind == NewChatKind::Group && !draft.info.about.is_empty() {
            let saved = tokio::time::timeout(
                RPC_TIMEOUT,
                self.client.invoke(&EditChatAbout {
                    peer: reference.into(),
                    about: draft.info.about.clone(),
                }),
            )
            .await;
            if !accepted(&saved, &["CHAT_ABOUT_NOT_MODIFIED", "CHAT_NOT_MODIFIED"]) {
                warnings.push(
                    "Telegram не подтвердил описание. Его можно изменить в настройках чата.".into(),
                );
                draft.info.about.clear();
            }
        }

        let public_error = if draft.username.is_empty() {
            None
        } else {
            self.update_channel_username(reference, &draft.username)
                .await
                .err()
                .map(|error| error.to_string())
        };
        if draft.kind != NewChatKind::Group && !users.is_empty() {
            match tokio::time::timeout(
                RPC_TIMEOUT,
                self.client.invoke(&InviteToChannel {
                    channel: reference.into(),
                    users,
                }),
            )
            .await
            {
                Ok(Ok(enums::messages::InvitedUsers::Users(result))) => {
                    missing = result.missing_invitees
                }
                Ok(Err(error)) => warnings.push(format!(
                    "Telegram не подтвердил добавление участников. {error}"
                )),
                Err(_) => {
                    warnings.push("Telegram не подтвердил добавление участников вовремя.".into())
                }
            }
        }
        if let Some(warning) = missing_members(&missing, &draft) {
            warnings.push(warning);
        }
        self.invalidate_views();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64);
        NewChatResult::Created(Box::new(CreatedChat {
            chat: draft.chat(id, now),
            draft,
            warnings,
            public_error,
        }))
    }

    async fn creation_users(&self, draft: &NewChatDraft) -> Result<Vec<enums::InputUser>> {
        let mut users = Vec::with_capacity(draft.members.len());
        for member in &draft.members {
            let peer = self.peer(member.id).await?;
            ensure!(
                peer.id.kind() == PeerKind::User
                    && enums::InputPeer::from(peer) != enums::InputPeer::PeerSelf,
                "Выбранный участник недоступен"
            );
            users.push(peer.into());
        }
        Ok(users)
    }

    // The creation methods don't accept an idempotency key. Avoid the client's
    // generic I/O retry and require reconciliation after an uncertain response.
    async fn create_once<R: RemoteCall>(&self, request: &R) -> Result<R::Return, NewChatResult> {
        let dc = self
            .session
            .home_dc_id()
            .map_err(|error| NewChatResult::Rejected(error.to_string()))?;
        let reply = tokio::time::timeout(
            RPC_TIMEOUT,
            self.sender.raw_invoke_in_dc(dc, request.to_bytes()),
        )
        .await;
        match reply {
            Ok(Ok(body)) => R::Return::from_bytes(&body).map_err(|_| {
                NewChatResult::Unconfirmed("Не удалось прочитать ответ Telegram.".into())
            }),
            Ok(Err(error @ InvocationError::Rpc(_))) if matches!(&error, InvocationError::Rpc(rpc) if (400..500).contains(&rpc.code)) => {
                Err(NewChatResult::Rejected(error.to_string()))
            }
            Ok(Err(error)) => Err(NewChatResult::Unconfirmed(error.to_string())),
            Err(_) => Err(NewChatResult::Unconfirmed(
                "Telegram не прислал ответ вовремя.".into(),
            )),
        }
    }

    async fn created_chat_reference(
        &self,
        updates: enums::Updates,
        draft: &NewChatDraft,
    ) -> Result<PeerRef> {
        let (chats, events) = match updates {
            enums::Updates::Updates(value) => (value.chats, value.updates),
            enums::Updates::Combined(value) => (value.chats, value.updates),
            _ => bail!("Telegram не вернул данные нового чата."),
        };
        let service_id = events
            .iter()
            .find_map(|event| creation_service_id(event, draft.kind));
        let chat = choose_created_chat(chats, draft, service_id)
            .context("Telegram не вернул однозначный идентификатор нового чата.")?;
        let peer = Peer::from_raw(&self.client, chat);
        self.remember_chat_author(&peer);
        peer.to_ref()
            .await
            .map_err(anyhow::Error::from_boxed)?
            .context("Новый чат пока недоступен.")
    }

    pub async fn set_created_chat_username(&self, chat: i64, username: &str) -> Result<()> {
        let username = chat_username(username)?;
        ensure!(!username.is_empty(), "Введите публичное имя.");
        let _serial = self.dialog_settings.lock().await;
        let reference = self.peer(chat).await?;
        ensure!(
            reference.id.kind() == PeerKind::Channel,
            "У этого чата нет публичного имени."
        );
        self.update_channel_username(reference, &username).await?;
        self.invalidate_views();
        Ok(())
    }

    pub(super) async fn update_channel_username(
        &self,
        channel: PeerRef,
        username: &str,
    ) -> Result<()> {
        let result = tokio::time::timeout(
            RPC_TIMEOUT,
            self.client.invoke(&UpdateUsername {
                channel: channel.into(),
                username: username.to_owned(),
            }),
        )
        .await;
        if accepted(&result, &["USERNAME_NOT_MODIFIED", "CHAT_NOT_MODIFIED"]) {
            return Ok(());
        }
        match result {
            Ok(Err(error)) => bail!("{}", username_error(error)),
            Ok(Ok(_)) => bail!("Telegram не подтвердил публичное имя."),
            Err(_) => bail!("Telegram не подтвердил публичное имя вовремя."),
        }
    }
}

pub(super) fn accepted(
    result: &Result<Result<bool, InvocationError>, tokio::time::error::Elapsed>,
    already_set: &[&str],
) -> bool {
    match result {
        Ok(Ok(value)) => *value,
        Ok(Err(error)) => already_set.iter().any(|code| error.is(code)),
        Err(_) => false,
    }
}

pub(super) fn username_error(error: InvocationError) -> String {
    if error.is("USERNAME_OCCUPIED") {
        "Это публичное имя занято.".into()
    } else if error.is("USERNAME_INVALID") {
        "Telegram считает это публичное имя недопустимым.".into()
    } else if error.is("USERNAME_PURCHASE_AVAILABLE") {
        "Telegram предлагает это публичное имя на Fragment.".into()
    } else if error.is("CHANNELS_ADMIN_PUBLIC_TOO_MUCH") {
        "Достигнут лимит публичных чатов Telegram.".into()
    } else {
        format!("Не удалось установить публичное имя. {error}")
    }
}

fn creation_service_id(update: &enums::Update, kind: NewChatKind) -> Option<PeerId> {
    let message = match update {
        enums::Update::NewMessage(value) => &value.message,
        enums::Update::NewChannelMessage(value) => &value.message,
        _ => return None,
    };
    let enums::Message::Service(message) = message else {
        return None;
    };
    match (&message.action, kind) {
        (enums::MessageAction::ChatCreate(_), NewChatKind::Group)
        | (
            enums::MessageAction::ChannelCreate(_),
            NewChatKind::Supergroup | NewChatKind::Channel,
        ) => Some(PeerId::from(message.peer_id.clone())),
        _ => None,
    }
}

fn choose_created_chat(
    chats: Vec<enums::Chat>,
    draft: &NewChatDraft,
    service_id: Option<PeerId>,
) -> Option<enums::Chat> {
    let mut candidates = chats.into_iter().filter(|chat| {
        let (id, title, eligible) = match chat {
            enums::Chat::Chat(chat) => (
                PeerId::chat(chat.id),
                &chat.title,
                draft.kind == NewChatKind::Group
                    && chat.creator
                    && !chat.left
                    && !chat.deactivated
                    && chat.migrated_to.is_none(),
            ),
            enums::Chat::Channel(chat) => (
                PeerId::channel(chat.id),
                &chat.title,
                chat.creator
                    && !chat.left
                    && !chat.min
                    && ((draft.kind == NewChatKind::Channel && chat.broadcast)
                        || (draft.kind == NewChatKind::Supergroup && chat.megagroup)),
            ),
            _ => return false,
        };
        eligible
            && id.is_some()
            && service_id.map_or(title == &draft.info.title, |expected| id == Some(expected))
    });
    let candidate = candidates.next()?;
    candidates.next().is_none().then_some(candidate)
}

fn missing_members(missing: &[enums::MissingInvitee], draft: &NewChatDraft) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    let names = draft
        .members
        .iter()
        .filter(|member| {
            missing
                .iter()
                .any(|enums::MissingInvitee::Invitee(raw)| raw.user_id == member.id)
        })
        .map(|member| member.name.as_str())
        .collect::<Vec<_>>();
    if names.is_empty() {
        Some("Telegram не добавил часть выбранных участников.".into())
    } else {
        Some(format!(
            "Telegram не добавил следующих участников.\n{}",
            names.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatInfo, ChatKind, Contact};
    use grammers_client::tl::types;

    fn sample_group() -> NewChatDraft {
        NewChatDraft {
            kind: NewChatKind::Group,
            info: ChatInfo {
                title: "Группа разработки".into(),
                about: String::new(),
            },
            username: String::new(),
            members: Vec::new(),
        }
    }

    #[test]
    fn service_chat_id_disambiguates_chats_with_the_same_title() {
        let draft = sample_group();
        let chat = |id| {
            types::Chat {
                id,
                title: draft.info.title.clone(),
                creator: true,
                left: false,
                deactivated: false,
                call_active: false,
                call_not_empty: false,
                noforwards: false,
                participants_count: 2,
                date: 100,
                version: 1,
                photo: enums::ChatPhoto::Empty,
                migrated_to: None,
                admin_rights: None,
                default_banned_rights: None,
            }
            .into()
        };
        let matches = || vec![chat(23), chat(24)];
        assert!(choose_created_chat(matches(), &draft, None).is_none());
        let Some(enums::Chat::Chat(found)) =
            choose_created_chat(matches(), &draft, PeerId::chat(24))
        else {
            panic!("expected the peer identified by the creation event");
        };
        assert_eq!(found.id, 24);
        assert!(choose_created_chat(matches(), &draft, PeerId::chat(25)).is_none());
    }

    #[test]
    fn partial_invite_result_identifies_missing_contacts() {
        let mut draft = sample_group();
        draft.members = [("Леся", 41), ("Катерина", 42)]
            .map(|(name, id)| Contact {
                id,
                name: name.into(),
                usernames: Vec::new(),
                available: true,
                is_self: false,
                kind: ChatKind::Contact,
                phone: None,
                avatar: None,
            })
            .into();
        let missing = types::MissingInvitee {
            user_id: 42,
            premium_would_allow_invite: false,
            premium_required_for_pm: false,
        }
        .into();
        let warning = missing_members(&[missing], &draft).unwrap();
        assert!(warning.contains("Катерина"));
        assert!(!warning.contains("Леся"));
        assert!(missing_members(&[], &draft).is_none());
    }
}
