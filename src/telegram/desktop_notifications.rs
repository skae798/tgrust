use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use grammers_client::peer::Peer;
use grammers_session::Session;

use super::{
    PeerId, PeerKind, PeerRef, Telegram, TelegramMessage, chat_kind, enums, is_muted, types,
};
use crate::model::{ChatKind, IncomingMessage};

pub(crate) struct PendingNotification {
    message: IncomingMessage,
    channel: Option<ChannelHint>,
    reply_root: Option<i32>,
    addressed: bool,
}

#[derive(Clone, Copy)]
struct ChannelHint {
    member: Option<bool>,
    linked_group: bool,
}

impl ChannelHint {
    fn from_channel(channel: &types::Channel) -> Self {
        Self {
            // A minimal record does not carry reliable membership information.
            member: (!channel.min).then_some(!channel.left),
            linked_group: channel.megagroup && channel.has_link,
        }
    }
}

#[derive(Clone, Copy)]
struct ChannelAudience {
    member: bool,
    source: Option<i64>,
}

#[derive(Default)]
pub(crate) struct NotificationFilter {
    channels: HashMap<i64, (Instant, ChannelAudience)>,
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
    let channel = match peer {
        Peer::User(user) if user.is_self() => return None,
        Peer::Group(group) => match &group.raw {
            enums::Chat::Channel(raw) => Some(ChannelHint::from_channel(raw)),
            enums::Chat::Chat(raw) if !raw.left && !raw.deactivated => None,
            _ => return None,
        },
        Peer::Channel(channel) => Some(ChannelHint::from_channel(&channel.raw)),
        _ => None,
    };
    candidate(
        raw,
        message.peer_id().bot_api_dialog_id()?,
        peer.name().unwrap_or("Telegram"),
        chat_kind(peer),
        channel,
    )
}

fn candidate(
    raw: &types::Message,
    chat: i64,
    title: &str,
    kind: ChatKind,
    channel: Option<ChannelHint>,
) -> Option<PendingNotification> {
    // A channel post has its own notification. Its automatic copy is only the thread root.
    if kind == ChatKind::Group
        && channel.is_some_and(|hint| discussion_origin(raw, chat, hint.linked_group).is_some())
    {
        return None;
    }
    let reply_root = match &raw.reply_to {
        Some(enums::MessageReplyHeader::Header(reply))
            if kind == ChatKind::Group
                && !reply.forum_topic
                && reply
                    .reply_to_peer_id
                    .as_ref()
                    .is_none_or(|peer| channel_dialog_id(peer) == Some(chat)) =>
        {
            reply
                .reply_to_top_id
                .or(reply.reply_to_msg_id)
                .filter(|&root| root > 0 && root != raw.id)
        }
        _ => None,
    };
    // Telegram includes replies to the current account in the mention flag.
    let addressed = raw.mentioned;
    if channel.is_some_and(|hint| {
        hint.member == Some(false) && (!hint.linked_group || !addressed || reply_root.is_none())
    }) {
        return None;
    }
    Some(PendingNotification {
        message: IncomingMessage {
            chat,
            origin_channel: None,
            message: raw.id,
            title: title.to_owned(),
            kind,
        },
        channel,
        reply_root,
        addressed,
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
                |chat| self.notification_audience(chat),
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

    async fn notification_audience(&self, chat: i64) -> Result<ChannelAudience> {
        let peer = self.notification_peer(chat).await?;
        let full = self.full_chat(peer).await?;
        let enums::ChatFull::ChannelFull(details) = full.full_chat else {
            anyhow::bail!("Telegram вернул другой тип чата");
        };
        let raw = full
            .chats
            .iter()
            .find_map(|chat| match chat {
                enums::Chat::Channel(raw) if Some(raw.id) == peer.id.bare_id() && !raw.min => {
                    Some(raw)
                }
                _ => None,
            })
            .context("Telegram не подтвердил участие в чате")?;
        let member = !raw.left;
        let linked = if raw.megagroup {
            details.linked_chat_id.and_then(PeerId::channel)
        } else {
            None
        };
        if let Some(linked) = linked
            && let Some(source) = full.chats.into_iter().find_map(|chat| match chat {
                enums::Chat::Channel(channel)
                    if Some(channel.id) == linked.bare_id()
                        && channel.broadcast
                        && !channel.min =>
                {
                    Some(channel)
                }
                _ => None,
            })
            && let (Some(id), Some(auth)) = (linked.bot_api_dialog_id(), source.access_hash)
        {
            self.peers.write().await.insert(
                id,
                PeerRef {
                    id: linked,
                    auth: grammers_session::types::PeerAuth::from_hash(auth),
                },
            );
        }
        Ok(ChannelAudience {
            member,
            source: linked.and_then(PeerId::bot_api_dialog_id),
        })
    }

    async fn notification_peer(&self, chat: i64) -> Result<PeerRef> {
        let id = PeerId::from_bot_api_dialog_id(chat).context("Чат не найден")?;
        if let Some(peer) = self.session.peer_ref(id).await? {
            return Ok(peer);
        }
        self.peer(chat).await
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
    async fn resolve<Audience, Fetch, Enabled>(
        &mut self,
        pending: PendingNotification,
        mut lookup_audience: impl FnMut(i64) -> Audience,
        mut fetch_root: impl FnMut(i64, i32) -> Fetch,
        mut enabled: impl FnMut(i64, ChatKind, Option<(i64, i32)>) -> Enabled,
    ) -> Result<Option<IncomingMessage>>
    where
        Audience: Future<Output = Result<ChannelAudience>>,
        Fetch: Future<Output = Result<Option<enums::Message>>>,
        Enabled: Future<Output = Result<bool>>,
    {
        let mut message = pending.message;
        if let Some(hint) = pending.channel
            && (hint.member.is_none() || hint.linked_group)
        {
            let audience = match self.channels.get(&message.chat) {
                Some((checked, audience)) if checked.elapsed() < Duration::from_secs(60) => {
                    *audience
                }
                _ => {
                    let audience = lookup_audience(message.chat).await?;
                    if self.channels.len() >= 256 {
                        self.channels.clear();
                    }
                    self.channels
                        .insert(message.chat, (Instant::now(), audience));
                    audience
                }
            };
            if !audience.member
                && (audience.source.is_none() || !pending.addressed || pending.reply_root.is_none())
            {
                return Ok(None);
            }
            let mut source = audience.source;
            let mut source_context = None;
            if (audience.source.is_some() || hint.linked_group)
                && let Some(root) = pending.reply_root
            {
                let key = (message.chat, root);
                let origin = if let Some(origin) = self.origins.get(&key) {
                    *origin
                } else {
                    let Some(message_root) = fetch_root(key.0, key.1).await? else {
                        return Ok(None);
                    };
                    if message_root.id() != root {
                        return Ok(None);
                    }
                    let origin = match message_root {
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
                    self.origins.insert(key, origin);
                    origin
                };
                if let Some(origin) = origin {
                    if !pending.addressed {
                        return Ok(None);
                    }
                    source = Some(origin);
                    source_context = Some(key);
                }
            }
            if let Some(source) = source {
                if !enabled(source, ChatKind::Broadcast, source_context).await? {
                    return Ok(None);
                }
                message.origin_channel = Some(source);
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
        group_pending(raw, Some(true), true)
    }

    fn group_pending(
        raw: &types::Message,
        member: Option<bool>,
        linked_group: bool,
    ) -> Option<PendingNotification> {
        candidate(
            raw,
            GROUP,
            "Группа обсуждения",
            ChatKind::Group,
            Some(ChannelHint {
                member,
                linked_group,
            }),
        )
    }

    async fn linked_audience(chat: i64) -> Result<ChannelAudience> {
        assert_eq!(chat, GROUP);
        Ok(ChannelAudience {
            member: true,
            source: Some(SOURCE),
        })
    }

    #[tokio::test]
    async fn muted_channel_blocks_comment_despite_unmuted_discussion() {
        let notification = NotificationFilter::default()
            .resolve(
                pending(&reply(true, true)).unwrap(),
                linked_audience,
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
                linked_audience,
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
                linked_audience,
                |_, root| async move { Ok(Some(raw_message(root).into())) },
                |chat, kind, context| async move {
                    assert!(
                        (chat == GROUP && kind == ChatKind::Group)
                            || (chat == SOURCE && kind == ChatKind::Broadcast)
                    );
                    assert_eq!(context, None);
                    Ok(true)
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(normal_reply.chat, GROUP);
        assert_eq!(normal_reply.origin_channel, Some(SOURCE));
    }

    #[tokio::test]
    async fn linked_channel_mute_also_covers_messages_without_a_post_root() {
        for raw in [raw_message(23), reply(false, false)] {
            let notification = NotificationFilter::default()
                .resolve(
                    pending(&raw).unwrap(),
                    linked_audience,
                    |_, root| async move {
                        Ok(Some(
                            types::MessageService {
                                out: false,
                                mentioned: false,
                                media_unread: false,
                                reactions_are_possible: false,
                                silent: false,
                                post: false,
                                legacy: false,
                                id: root,
                                from_id: None,
                                peer_id: PeerId::from_bot_api_dialog_id(GROUP).unwrap().into(),
                                saved_peer_id: None,
                                reply_to: None,
                                date: 1,
                                action: enums::MessageAction::Empty,
                                reactions: None,
                                ttl_period: None,
                            }
                            .into(),
                        ))
                    },
                    |chat, _, _| async move { Ok(chat != SOURCE) },
                )
                .await
                .unwrap();
            assert!(
                notification.is_none(),
                "мута связанного канала недостаточно без корня поста"
            );
        }
    }

    #[tokio::test]
    async fn personal_comment_requires_both_settings_even_when_origin_is_cached() {
        let mut filter = NotificationFilter::default();
        let mut fetches = 0;
        for muted in [None, Some(GROUP), Some(SOURCE)] {
            let result = filter
                .resolve(
                    pending(&reply(true, false)).unwrap(),
                    linked_audience,
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

    #[tokio::test]
    async fn nonmember_and_partial_group_records_do_not_emit_unaddressed_updates() {
        for raw in [raw_message(23), reply(false, false), reply(false, true)] {
            assert!(group_pending(&raw, Some(false), true).is_none());
            let result = NotificationFilter::default()
                .resolve(
                    group_pending(&raw, None, false).unwrap(),
                    |_| async {
                        Ok(ChannelAudience {
                            member: false,
                            source: Some(SOURCE),
                        })
                    },
                    |_, _| async { panic!("unaddressed updates need no thread lookup") },
                    |_, _, _| async { panic!("unaddressed updates need no mute lookup") },
                )
                .await
                .unwrap();
            assert!(result.is_none());
        }
    }

    #[tokio::test]
    async fn a_member_in_a_regular_supergroup_keeps_group_notifications() {
        let result = NotificationFilter::default()
            .resolve(
                group_pending(&reply(false, false), Some(true), false).unwrap(),
                |_| async { panic!("a full unlinked group needs no audience lookup") },
                |_, _| async { panic!("ordinary groups need no thread lookup") },
                |chat, kind, context| async move {
                    assert_eq!(chat, GROUP);
                    assert!(kind == ChatKind::Group);
                    assert_eq!(context, None);
                    Ok(true)
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.origin_channel, None);
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
