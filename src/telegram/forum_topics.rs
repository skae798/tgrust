use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use grammers_client::peer::Peer;
use grammers_client::tl::functions::messages::{
    GetForumTopics, GetForumTopicsById, ReadDiscussion,
};

use super::forum_management::topic_permissions;
use super::{PeerKind, PeerRef, Telegram, TelegramMessage, enums, raw_peer_id};
use crate::model::{
    FORUM_PAGE_SIZE, ForumCursor, ForumPage, ForumTopic, GENERAL_TOPIC, HISTORY_PAGE_SIZE,
    MAX_ALBUM_ITEMS, Message,
};

impl Telegram {
    pub async fn forum_topic_for_message(&self, chat: i64, message: i32) -> Result<i32> {
        let peer = self.forum_peer(chat).await?;
        let message = self
            .message_by_id(peer, message)
            .await?
            .context("Сообщение больше недоступно")?;
        Ok(message_topic(&message.raw).unwrap_or(GENERAL_TOPIC))
    }

    pub async fn forum_topics(&self, id: i64, cursor: ForumCursor) -> Result<ForumPage> {
        let peer = self.forum_peer(id).await?;
        let enums::messages::ForumTopics::Topics(result) = self
            .client
            .invoke(&GetForumTopics {
                peer: peer.into(),
                q: None,
                offset_date: cursor.date,
                offset_id: cursor.message,
                offset_topic: cursor.topic,
                limit: FORUM_PAGE_SIZE as i32,
            })
            .await?;
        let dates: HashMap<_, _> = result
            .messages
            .iter()
            .filter_map(|message| match message {
                enums::Message::Message(message) => Some((message.id, message.date)),
                enums::Message::Service(message) => Some((message.id, message.date)),
                _ => None,
            })
            .collect();
        let last = result.topics.iter().rev().find_map(|topic| {
            let enums::ForumTopic::Topic(topic) = topic else {
                return None;
            };
            (topic.id > 0 && raw_peer_id(&topic.peer) == Some(peer.id)).then(|| ForumCursor {
                message: topic.top_message,
                topic: topic.id,
                date: if result.order_by_create_date {
                    topic.date
                } else {
                    dates.get(&topic.top_message).copied().unwrap_or(topic.date)
                },
            })
        });
        let next = last.filter(|last| {
            *last != cursor && (cursor.topic != 0 || result.count > result.topics.len() as i32)
        });
        let mut permissions = topic_permissions(&result.chats, peer);
        if permissions.is_none() && cursor.topic == 0 {
            permissions =
                tokio::time::timeout(Duration::from_secs(3), self.forum_permissions(peer))
                    .await
                    .ok()
                    .and_then(Result::ok);
        }
        let topics = result
            .topics
            .into_iter()
            .filter_map(|topic| topic_view(topic, peer))
            .collect();
        Ok(ForumPage {
            topics,
            next,
            permissions,
        })
    }

    pub async fn forum_topic(&self, id: i64, topic: i32) -> Result<ForumTopic> {
        ensure!(topic > 0, "Тема недоступна");
        let peer = self.forum_peer(id).await?;
        self.forum_topic_snapshot(peer, topic)
            .await?
            .context("Тема больше недоступна")
    }

    pub(super) async fn forum_topic_snapshot(
        &self,
        peer: PeerRef,
        topic: i32,
    ) -> Result<Option<ForumTopic>> {
        let result = self
            .client
            .invoke(&GetForumTopicsById {
                peer: peer.into(),
                topics: vec![topic],
            })
            .await;
        let enums::messages::ForumTopics::Topics(result) = match result {
            Ok(result) => result,
            Err(error) if error.is("TOPIC_ID_INVALID") || error.is("TOPIC_DELETED") => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        for item in result.topics {
            if matches!(&item, enums::ForumTopic::Deleted(item) if item.id == topic) {
                return Ok(None);
            }
            if let Some(item) = topic_view(item, peer).filter(|item| item.id == topic) {
                return Ok(Some(item));
            }
        }
        anyhow::bail!("Telegram не вернул сведения об этой теме")
    }

    pub async fn forum_history(
        &self,
        chat: i64,
        topic: i32,
        before: Option<i32>,
        anchor: Option<i32>,
        mark_read: bool,
    ) -> Result<(Vec<Message>, bool, bool)> {
        ensure!(topic > 0, "Тема недоступна");
        let peer = self.forum_peer(chat).await?;
        let limit = HISTORY_PAGE_SIZE + MAX_ALBUM_ITEMS;
        let mut page = self
            .client
            .get_forum_history(
                peer,
                topic,
                before.or(anchor).unwrap_or_default(),
                anchor.map_or(0, |_| -(HISTORY_PAGE_SIZE as i32 / 2)),
                limit as i32,
            )
            .await?;
        let full = page.len() >= limit;
        page.retain(|message| {
            before.is_none_or(|id| message.id() < id)
                && message_topic(&message.raw).is_none_or(|id| id == topic)
        });
        if let Some(anchor) = anchor
            && !page.iter().any(|message| message.id() == anchor)
            && let Some(message) = self.message_by_id(peer, anchor).await?
            && message_topic(&message.raw).unwrap_or(GENERAL_TOPIC) == topic
        {
            page.push(message);
        }
        page.sort_unstable_by_key(|message| std::cmp::Reverse(message.id()));
        page.dedup_by_key(|message| message.id());
        let mut end = HISTORY_PAGE_SIZE.min(page.len());
        if end < page.len()
            && let Some(album) = page[end - 1].grouped_id()
        {
            while end < page.len() && page[end].grouped_id() == Some(album) {
                end += 1;
            }
        }
        let older = page.len() > end || (full && !page.is_empty());
        page.truncate(end);
        let marked = mark_read
            && before.is_none()
            && anchor.is_none()
            && self
                .read_forum_topic(chat, topic, page.first().map_or(0, TelegramMessage::id))
                .await
                .unwrap_or(false);
        let own = self.self_id().await?;
        let profile = self.profile().await;
        self.resolve_chat_authors(peer, page.iter()).await;
        let mut messages: Vec<_> = page
            .iter()
            .rev()
            .map(|message| self.message_view(message, own, profile.as_deref()))
            .collect();
        for message in &mut messages {
            message.topic = Some(topic);
        }
        self.load_reply_previews(peer, &mut messages, own, profile.as_deref())
            .await;
        Ok((messages, older, marked))
    }

    pub async fn read_forum_topic(&self, chat: i64, topic: i32, max_id: i32) -> Result<bool> {
        if max_id <= 0 {
            return Ok(false);
        }
        let peer = self.forum_peer(chat).await?;
        let read = self
            .client
            .invoke(&ReadDiscussion {
                peer: peer.into(),
                msg_id: topic,
                read_max_id: max_id,
            })
            .await?;
        Ok(read)
    }

    pub(super) async fn forum_peer(&self, chat: i64) -> Result<PeerRef> {
        let peer = self.peer(chat).await?;
        ensure!(
            peer.id.kind() == PeerKind::Channel,
            "Темы доступны в супергруппах"
        );
        Ok(peer)
    }
}

fn topic_view(raw: enums::ForumTopic, peer: PeerRef) -> Option<ForumTopic> {
    let enums::ForumTopic::Topic(topic) = raw else {
        return None;
    };
    (topic.id > 0 && raw_peer_id(&topic.peer) == Some(peer.id)).then(|| ForumTopic {
        id: topic.id,
        title: if topic.title.trim().is_empty() {
            if topic.id == GENERAL_TOPIC {
                "Общая".into()
            } else {
                "Тема без названия".into()
            }
        } else {
            topic.title
        },
        unread: topic.unread_count.max(0),
        closed: topic.closed,
        hidden: topic.hidden,
        pinned: topic.pinned,
        mine: topic.my,
    })
}

pub(super) fn is_forum(peer: &Peer) -> bool {
    matches!(peer, Peer::Group(group) if matches!(&group.raw, enums::Chat::Channel(channel)
        if channel.forum && channel.megagroup && !channel.monoforum))
}

pub(super) fn message_topic(message: &enums::Message) -> Option<i32> {
    let reply_to = match message {
        enums::Message::Message(message) => message.reply_to.as_ref(),
        enums::Message::Service(message) => {
            if matches!(message.action, enums::MessageAction::TopicCreate(_)) {
                return (message.id > 0).then_some(message.id);
            }
            message.reply_to.as_ref()
        }
        _ => None,
    };
    match reply_to {
        Some(enums::MessageReplyHeader::Header(header)) if header.forum_topic => header
            .reply_to_top_id
            .or(header.reply_to_msg_id)
            .filter(|&topic| topic > 0),
        _ => None,
    }
}
