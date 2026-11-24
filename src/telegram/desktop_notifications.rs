use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use grammers_client::peer::Peer;
use grammers_session::Session;

use super::{
    PeerId, PeerKind, PeerRef, Telegram, TelegramMessage, chat_kind, enums, is_muted, types,
};
use crate::model::{ChatKind, IncomingMessage};

pub(crate) struct PendingNotification {
    message: IncomingMessage,
    discussion: Option<Discussion>,
}

struct Discussion {
    root: i32,
    addressed: bool,
}

#[derive(Default)]
pub(crate) struct NotificationFilter {
    // The origin of an existing thread does not change. Mute settings are checked separately.
    origins: HashMap<(i64, i32), Option<i64>>,
}

pub(super) fn notification_for(
    message: &TelegramMessage,
    started: SystemTime,
) -> Option<PendingNotification> {
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
    let group = match peer {
        Peer::Group(group) => match &group.raw {
            enums::Chat::Channel(raw) if raw.megagroup => Some(raw.has_link),
            _ => None,
        },
        _ => None,
    };
    candidate(
        raw,
        message.peer_id().bot_api_dialog_id()?,
        peer.name().unwrap_or("Telegram"),
        chat_kind(peer),
        group,
    )
}

fn candidate(
    raw: &types::Message,
    chat: i64,
    title: &str,
    kind: ChatKind,
    linked_group: Option<bool>,
) -> Option<PendingNotification> {
    // A channel post has its own notification. Its automatic copy is only the thread root.
    if linked_group.is_some_and(|linked| discussion_origin(raw, chat, linked).is_some()) {
        return None;
    }
    let discussion = if linked_group == Some(true)
        && let Some(enums::MessageReplyHeader::Header(reply)) = &raw.reply_to
        && !reply.forum_topic
        && reply
            .reply_to_peer_id
            .as_ref()
            .is_none_or(|peer| channel_dialog_id(peer) == Some(chat))
    {
        reply
            .reply_to_top_id
            .or(reply.reply_to_msg_id)
            .filter(|&root| root > 0 && root != raw.id)
            .map(|root| Discussion {
                root,
                // Telegram includes direct replies to the current account in this flag.
                addressed: raw.mentioned,
            })
    } else {
        None
    };
    Some(PendingNotification {
        message: IncomingMessage {
            chat,
            origin_channel: None,
            message: raw.id,
            title: title.to_owned(),
            kind,
        },
        discussion,
    })
}

fn channel_dialog_id(peer: &enums::Peer) -> Option<i64> {
    let enums::Peer::Channel(peer) = peer else {
        return None;
    };
    PeerId::channel(peer.channel_id)?.bot_api_dialog_id()
}

fn discussion_origin(raw: &types::Message, chat: i64, linked: bool) -> Option<i64> {
    let enums::MessageFwdHeader::Header(forward) = raw.fwd_from.as_ref()?;
    if forward.imported || channel_dialog_id(&raw.peer_id) != Some(chat) {
        return None;
    }
    if forward.saved_from_msg_id.is_some_and(|id| id > 0)
        && let Some(channel) = forward.saved_from_peer.as_ref().and_then(channel_dialog_id)
        && channel != chat
    {
        return Some(channel);
    }
    let channel = raw.from_id.as_ref().and_then(channel_dialog_id)?;
    // Some auto-forwards only carry the original channel post and the channel as sender.
    (linked
        && channel != chat
        && forward.channel_post.is_some_and(|id| id > 0)
        && forward.from_id.as_ref().and_then(channel_dialog_id) == Some(channel))
    .then_some(channel)
}

impl Telegram {
    pub(crate) async fn desktop_notification(
        &self,
        filter: &mut NotificationFilter,
        pending: PendingNotification,
    ) -> Result<Option<IncomingMessage>> {
        filter
            .resolve(
                pending,
                |chat, root| async move {
                    let peer = self.notification_peer(chat).await?;
                    Ok(self
                        .message_by_id(peer, root)
                        .await?
                        .map(|message| message.raw))
                },
                |chat, kind, root| self.notification_enabled(chat, kind, root),
            )
            .await
    }

    async fn notification_peer(&self, chat: i64) -> Result<PeerRef> {
        let id = PeerId::from_bot_api_dialog_id(chat).context("Чат не найден")?;
        self.session.peer_ref(id).await?.context("Чат не найден")
    }

    async fn notification_enabled(
        &self,
        chat: i64,
        kind: ChatKind,
        discussion_root: Option<(i64, i32)>,
    ) -> Result<bool> {
        use grammers_client::tl::functions::account::GetNotifySettings;

        let peer = match self.notification_peer(chat).await {
            Ok(peer) => peer.into(),
            Err(error) => {
                let Some((group, msg_id)) = discussion_root else {
                    return Err(error);
                };
                let channel_id = PeerId::from_bot_api_dialog_id(chat)
                    .filter(|id| id.kind() == PeerKind::Channel)
                    .and_then(PeerId::bare_id)
                    .context("Исходный канал недоступен")?;
                types::InputPeerChannelFromMessage {
                    peer: self.notification_peer(group).await?.into(),
                    msg_id,
                    channel_id,
                }
                .into()
            }
        };
        let enums::PeerNotifySettings::Settings(settings) = self
            .client
            .invoke(&GetNotifySettings {
                peer: types::InputNotifyPeer { peer }.into(),
            })
            .await?;
        let mute_until = match settings.mute_until {
            Some(until) => until,
            None => self.default_mute_until(kind).await?,
        };
        Ok(!is_muted(mute_until, SystemTime::now()))
    }
}

impl NotificationFilter {
    async fn resolve<Fetch, Enabled>(
        &mut self,
        pending: PendingNotification,
        mut fetch_root: impl FnMut(i64, i32) -> Fetch,
        mut enabled: impl FnMut(i64, ChatKind, Option<(i64, i32)>) -> Enabled,
    ) -> Result<Option<IncomingMessage>>
    where
        Fetch: Future<Output = Result<Option<enums::Message>>>,
        Enabled: Future<Output = Result<bool>>,
    {
        let mut message = pending.message;
        if let Some(discussion) = pending.discussion {
            let key = (message.chat, discussion.root);
            let channel = if let Some(channel) = self.origins.get(&key) {
                *channel
            } else {
                let Some(root) = fetch_root(key.0, key.1).await? else {
                    return Ok(None);
                };
                if root.id() != discussion.root {
                    return Ok(None);
                }
                let channel = match root {
                    enums::Message::Message(raw)
                        if channel_dialog_id(&raw.peer_id) == Some(message.chat) =>
                    {
                        discussion_origin(&raw, message.chat, true)
                    }
                    enums::Message::Service(raw)
                        if channel_dialog_id(&raw.peer_id) == Some(message.chat) =>
                    {
                        None
                    }
                    _ => return Ok(None),
                };
                if self.origins.len() >= 256 {
                    self.origins.clear();
                }
                self.origins.insert(key, channel);
                channel
            };
            if let Some(channel) = channel {
                if !discussion.addressed
                    || !enabled(channel, ChatKind::Broadcast, Some(key)).await?
                {
                    return Ok(None);
                }
                message.origin_channel = Some(channel);
            }
        }
        Ok(enabled(message.chat, message.kind, None)
            .await?
            .then_some(message))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use grammers_client::Client;
    use grammers_client::message::InputMessage;
    use grammers_client::sender::SenderPool;
    use grammers_session::storages::MemorySession;

    use super::*;

    const GROUP: i64 = -1000000000140;
    const SOURCE: i64 = -1000000000120;
    const ROOT: i32 = 18;

    fn raw_message(id: i32) -> types::Message {
        let client = Client::new(SenderPool::new(Arc::new(MemorySession::default()), 12345).handle);
        let peer = PeerRef {
            id: PeerId::from_bot_api_dialog_id(GROUP).unwrap(),
            auth: Default::default(),
        };
        let enums::Message::Message(raw) = TelegramMessage::from_raw_short_updates(
            &client,
            types::UpdateShortSentMessage {
                out: false,
                id,
                pts: 1,
                pts_count: 1,
                date: 1,
                media: None,
                entities: None,
                ttl_period: None,
            },
            InputMessage::new(),
            peer,
        )
        .raw
        else {
            unreachable!()
        };
        raw
    }

    fn automatic_post() -> types::Message {
        let mut raw = raw_message(ROOT);
        let channel: enums::Peer = PeerId::from_bot_api_dialog_id(SOURCE).unwrap().into();
        raw.from_id = Some(channel.clone());
        raw.fwd_from = Some(
            types::MessageFwdHeader {
                imported: false,
                saved_out: false,
                from_id: Some(channel.clone()),
                from_name: None,
                date: 1,
                channel_post: Some(5),
                post_author: None,
                saved_from_peer: Some(channel),
                saved_from_msg_id: Some(5),
                saved_from_id: None,
                saved_from_name: None,
                saved_date: None,
                psa_type: None,
            }
            .into(),
        );
        raw
    }

    fn reply(addressed: bool, nested: bool) -> types::Message {
        let mut raw = raw_message(23);
        raw.mentioned = addressed;
        raw.reply_to = Some(
            types::MessageReplyHeader {
                reply_to_scheduled: false,
                forum_topic: false,
                quote: false,
                reply_to_ephemeral: false,
                reply_to_msg_id: Some(if nested { 21 } else { ROOT }),
                reply_to_peer_id: None,
                reply_from: None,
                reply_media: None,
                reply_to_top_id: nested.then_some(ROOT),
                quote_text: None,
                quote_entities: None,
                quote_offset: None,
                todo_item_id: None,
                poll_option: None,
                quote_rich: None,
            }
            .into(),
        );
        raw
    }

    fn pending(raw: &types::Message) -> Option<PendingNotification> {
        candidate(raw, GROUP, "Группа обсуждения", ChatKind::Group, Some(true))
    }

    #[tokio::test]
    async fn muted_channel_blocks_comment_despite_unmuted_discussion() {
        let notification = NotificationFilter::default()
            .resolve(
                pending(&reply(true, true)).unwrap(),
                |chat, root| async move {
                    assert_eq!((chat, root), (GROUP, ROOT));
                    Ok(Some(automatic_post().into()))
                },
                |chat, kind, root| async move {
                    assert_eq!(chat, SOURCE);
                    assert!(kind == ChatKind::Broadcast);
                    assert_eq!(root, Some((GROUP, ROOT)));
                    Ok(false)
                },
            )
            .await
            .unwrap();
        assert!(
            notification.is_none(),
            "включённая группа обошла отключение канала"
        );
    }

    #[tokio::test]
    async fn unrelated_comments_are_skipped_but_ordinary_group_replies_are_allowed() {
        let mut filter = NotificationFilter::default();
        let comment = filter
            .resolve(
                pending(&reply(false, false)).unwrap(),
                |_, root| async move {
                    assert_eq!(root, ROOT);
                    Ok(Some(automatic_post().into()))
                },
                |_, _, _| async { panic!("a comment to another user needs no mute lookup") },
            )
            .await
            .unwrap();
        assert!(comment.is_none());

        let normal_reply = NotificationFilter::default()
            .resolve(
                pending(&reply(false, false)).unwrap(),
                |_, root| async move { Ok(Some(raw_message(root).into())) },
                |chat, kind, root| async move {
                    assert_eq!(chat, GROUP);
                    assert!(kind == ChatKind::Group);
                    assert_eq!(root, None);
                    Ok(true)
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(normal_reply.chat, GROUP);
        assert_eq!(normal_reply.origin_channel, None);
    }

    #[tokio::test]
    async fn personal_comment_requires_both_settings_even_when_origin_is_cached() {
        let mut filter = NotificationFilter::default();
        let mut fetches = 0;
        for muted in [None, Some(GROUP), Some(SOURCE)] {
            let result = filter
                .resolve(
                    pending(&reply(true, false)).unwrap(),
                    |_, _| {
                        fetches += 1;
                        async { Ok(Some(automatic_post().into())) }
                    },
                    |chat, _, _| async move { Ok(muted != Some(chat)) },
                )
                .await
                .unwrap();
            assert_eq!(
                result.map(|notice| (notice.chat, notice.origin_channel)),
                muted.is_none().then_some((GROUP, Some(SOURCE)))
            );
        }
        assert_eq!(fetches, 1);
    }

    #[test]
    fn only_the_original_channel_post_and_manual_forward_can_notify() {
        let mut raw = automatic_post();
        assert!(pending(&raw).is_none());
        let Some(enums::MessageFwdHeader::Header(forward)) = &mut raw.fwd_from else {
            unreachable!()
        };
        forward.saved_from_peer = None;
        forward.saved_from_msg_id = None;
        assert!(pending(&raw).is_none());
        raw.from_id = Some(PeerId::user(999).unwrap().into());
        assert!(pending(&raw).is_some());
        raw.peer_id = PeerId::from_bot_api_dialog_id(SOURCE).unwrap().into();
        raw.post = true;
        assert!(candidate(&raw, SOURCE, "Канал", ChatKind::Broadcast, None).is_some());
    }
}
