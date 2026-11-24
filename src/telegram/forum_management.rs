use anyhow::{Context, Result, ensure};
use grammers_client::tl::functions::messages::{
    CreateForumTopic, DeleteTopicHistory, EditForumTopic, UpdatePinnedForumTopic,
};
use tokio::time::timeout;

use super::create_chat::RPC_TIMEOUT;
use super::{InvocationError, PeerRef, Telegram, enums, raw_peer_id, types};
use crate::model::{
    ForumPermissions, ForumTopic, ForumTopicChange, ForumWrite, ForumWriteResult, GENERAL_TOPIC,
    forum_topic_title,
};

impl Telegram {
    pub async fn write_forum_topic(
        &self,
        chat: i64,
        write: ForumWrite,
    ) -> Result<ForumWriteResult> {
        let _serial = self.dialog_settings.lock().await;
        let peer = self.forum_peer(chat).await?;
        let permissions = self.forum_permissions(peer).await?;
        match write {
            ForumWrite::Create { title, random_id } => {
                ensure!(permissions.create, "Создание тем запрещено в этой группе");
                self.create_forum_topic(peer, forum_topic_title(&title)?, random_id)
                    .await
            }
            ForumWrite::Change { original, change } => {
                self.change_forum_topic(chat, peer, permissions, original, change)
                    .await
            }
        }
    }

    pub(super) async fn forum_permissions(&self, peer: PeerRef) -> Result<ForumPermissions> {
        let full = timeout(RPC_TIMEOUT, self.full_chat(peer))
            .await
            .context("Telegram не прислал права форума вовремя")??;
        topic_permissions(&full.chats, peer).context("Управление темами этой группы недоступно")
    }

    async fn create_forum_topic(
        &self,
        peer: PeerRef,
        title: String,
        random_id: i64,
    ) -> Result<ForumWriteResult> {
        let result = timeout(
            RPC_TIMEOUT,
            self.client.invoke(&CreateForumTopic {
                title_missing: false,
                peer: peer.into(),
                title: title.clone(),
                icon_color: Some(0x6FB9F0),
                icon_emoji_id: None,
                random_id,
                send_as: None,
            }),
        )
        .await;
        self.invalidate_views();
        let updates = match result {
            Ok(Ok(updates)) => updates,
            Ok(Err(InvocationError::Rpc(error)))
                if error.code < 500 && !error.is("RANDOM_ID_DUPLICATE") =>
            {
                return Err(InvocationError::Rpc(error).into());
            }
            Err(_) | Ok(Err(_)) => return Ok(ForumWriteResult::CreationUnconfirmed),
        };
        let Some(id) = created_topic_id(updates, peer, random_id, &title) else {
            return Ok(ForumWriteResult::CreationUnconfirmed);
        };
        Ok(ForumWriteResult::Created(ForumTopic {
            id,
            title,
            unread: 0,
            closed: false,
            hidden: false,
            pinned: false,
            mine: true,
        }))
    }

    async fn change_forum_topic(
        &self,
        chat: i64,
        peer: PeerRef,
        permissions: ForumPermissions,
        original: ForumTopic,
        change: ForumTopicChange,
    ) -> Result<ForumWriteResult> {
        let id = original.id;
        ensure!(id > 0, "Тема недоступна");
        let deleting = matches!(change, ForumTopicChange::Delete);
        ensure!(
            !deleting || id != GENERAL_TOPIC,
            "Общую тему удалить нельзя"
        );
        let current = match self.forum_topic_status(peer, id).await? {
            Some(current) => current,
            None if deleting => return Ok(ForumWriteResult::Deleted(id)),
            None => anyhow::bail!("Тема больше недоступна"),
        };
        ensure!(
            permissions.allows(&current, &change),
            "У вас нет права менять эту тему"
        );
        if !change.needs_write(&original, &current)? {
            return Ok(ForumWriteResult::Updated(current));
        }
        if deleting {
            if !permissions.delete_messages {
                let messages = timeout(
                    RPC_TIMEOUT,
                    self.client.get_forum_history(peer, id, 0, 0, 12),
                )
                .await
                .context("Telegram не прислал историю темы вовремя")??;
                let replies = messages.iter().filter(|message| message.id() != id);
                ensure!(
                    replies.clone().count() <= 10
                        && replies.clone().all(|message| message.outgoing()),
                    "Для удаления этой темы нужно право удалять сообщения участников"
                );
            }
            self.delete_forum_topic(chat, peer, id).await?;
            return Ok(ForumWriteResult::Deleted(id));
        }

        self.edit_forum_topic(peer, id, &change).await?;
        let saved = self
            .forum_topic_status(peer, id)
            .await?
            .context("Тема больше недоступна")?;
        ensure!(
            !change.needs_write(&original, &saved)?,
            "Telegram не подтвердил изменение темы"
        );
        Ok(ForumWriteResult::Updated(saved))
    }

    async fn edit_forum_topic(
        &self,
        peer: PeerRef,
        id: i32,
        change: &ForumTopicChange,
    ) -> Result<()> {
        if let ForumTopicChange::Pinned(pinned) = change {
            let result = timeout(
                RPC_TIMEOUT,
                self.client.invoke(&UpdatePinnedForumTopic {
                    peer: peer.into(),
                    topic_id: id,
                    pinned: *pinned,
                }),
            )
            .await;
            self.invalidate_views();
            if let Err(error) = result.context("Telegram не подтвердил закрепление вовремя")?
                && !error.is("TOPIC_NOT_MODIFIED")
            {
                return Err(error.into());
            }
        } else {
            let mut edit = EditForumTopic {
                peer: peer.into(),
                topic_id: id,
                title: None,
                icon_emoji_id: None,
                closed: None,
                hidden: None,
            };
            match change {
                ForumTopicChange::Rename(title) => edit.title = Some(forum_topic_title(title)?),
                ForumTopicChange::Closed(closed) => edit.closed = Some(*closed),
                ForumTopicChange::Hidden(hidden) => edit.hidden = Some(*hidden),
                _ => unreachable!("delete and pin have separate Telegram methods"),
            }
            let result = timeout(RPC_TIMEOUT, self.client.invoke(&edit)).await;
            self.invalidate_views();
            if let Err(error) = result.context("Telegram не подтвердил изменение темы вовремя")?
                && !error.is("TOPIC_NOT_MODIFIED")
            {
                return Err(error.into());
            }
        }
        Ok(())
    }

    async fn delete_forum_topic(&self, chat: i64, peer: PeerRef, id: i32) -> Result<()> {
        loop {
            let result = timeout(
                RPC_TIMEOUT,
                self.client.invoke(&DeleteTopicHistory {
                    peer: peer.into(),
                    top_msg_id: id,
                }),
            )
            .await;
            self.invalidate_history(Some(chat), true);
            self.invalidate_views();
            match result.context("Telegram не подтвердил удаление вовремя. Обновите список тем")?
            {
                Ok(enums::messages::AffectedHistory::History(result)) if result.offset > 0 => {
                    continue;
                }
                Ok(_) => break,
                Err(error) if error.is("TOPIC_ID_INVALID") || error.is("TOPIC_DELETED") => break,
                Err(error) => return Err(error.into()),
            }
        }
        ensure!(
            self.forum_topic_status(peer, id).await?.is_none(),
            "Telegram не подтвердил удаление"
        );
        Ok(())
    }

    async fn forum_topic_status(&self, peer: PeerRef, id: i32) -> Result<Option<ForumTopic>> {
        timeout(RPC_TIMEOUT, self.forum_topic_snapshot(peer, id))
            .await
            .context("Telegram не прислал сведения о теме вовремя")?
    }
}

pub(super) fn topic_permissions(chats: &[enums::Chat], peer: PeerRef) -> Option<ForumPermissions> {
    let channel = chats.iter().find_map(|chat| {
        let enums::Chat::Channel(channel) = chat else {
            return None;
        };
        (Some(channel.id) == peer.id.bare_id()
            && channel.forum
            && channel.megagroup
            && !channel.left
            && !channel.min
            && !channel.monoforum)
            .then_some(channel)
    })?;
    Some(channel_topic_permissions(channel))
}

fn channel_topic_permissions(channel: &types::Channel) -> ForumPermissions {
    let permits = |field: fn(&types::ChatAdminRights) -> bool| {
        channel.creator
            || channel
                .admin_rights
                .as_ref()
                .is_some_and(|enums::ChatAdminRights::Rights(rights)| field(rights))
    };
    let banned = |rights: &Option<enums::ChatBannedRights>| {
        rights
            .as_ref()
            .is_some_and(|enums::ChatBannedRights::Rights(rights)| {
                rights.view_messages || rights.manage_topics
            })
    };
    let manage = permits(|rights| rights.manage_topics);
    ForumPermissions {
        create: manage
            || (!banned(&channel.banned_rights) && !banned(&channel.default_banned_rights)),
        manage,
        delete_messages: permits(|rights| rights.delete_messages),
    }
}

fn created_topic_id(
    updates: enums::Updates,
    peer: PeerRef,
    random_id: i64,
    title: &str,
) -> Option<i32> {
    let updates = match updates {
        enums::Updates::Updates(updates) => updates.updates,
        enums::Updates::Combined(updates) => updates.updates,
        enums::Updates::UpdateShort(update) => vec![update.update],
        _ => return None,
    };
    if let Some(id) = updates.iter().find_map(|update| {
        let enums::Update::MessageId(message) = update else {
            return None;
        };
        (message.random_id == random_id && message.id > GENERAL_TOPIC).then_some(message.id)
    }) {
        return Some(id);
    }
    let mut matches = updates.iter().filter_map(|update| {
        let enums::Update::NewChannelMessage(update) = update else {
            return None;
        };
        let enums::Message::Service(message) = &update.message else {
            return None;
        };
        let enums::MessageAction::TopicCreate(action) = &message.action else {
            return None;
        };
        (message.id > GENERAL_TOPIC
            && message.out
            && action.title == title
            && raw_peer_id(&message.peer_id) == Some(peer.id))
        .then_some(message.id)
    });
    let id = matches.next()?;
    matches.next().is_none().then_some(id)
}
