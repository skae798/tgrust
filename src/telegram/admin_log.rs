use super::{
    PeerId, PeerKind, Telegram, channel_member, enums, model_admin_rights, peer_author,
    poll_question, quote_preview, raw_peer_id, types,
};
use crate::model::{
    ADMIN_LOG_PAGE_SIZE, AdminLogActor, AdminLogEntry, AdminLogFilter, AdminLogPage, AdminLogQuery,
    AdminRight, MemberRole,
};
use anyhow::{Result, ensure};
use grammers_client::tl::functions::channels::GetAdminLog;

impl Telegram {
    pub async fn admin_log(
        &self,
        chat: i64,
        query: AdminLogQuery,
        before: Option<i64>,
    ) -> Result<AdminLogPage> {
        let channel = self.peer(chat).await?;
        ensure!(
            channel.id.kind() == PeerKind::Channel && before.is_none_or(|id| id > 0),
            "Журнал действий недоступен"
        );
        let admins = if let Some(id) = query.actor {
            let actor = self.peer(id).await?;
            ensure!(actor.id.kind() == PeerKind::User, "Автор недоступен");
            Some(vec![actor.into()])
        } else {
            None
        };
        let enums::channels::AdminLogResults::Results(page) = self
            .client
            .invoke(&GetAdminLog {
                channel: channel.into(),
                q: query.text,
                events_filter: event_filter(query.filter),
                admins,
                max_id: before.unwrap_or_default(),
                min_id: 0,
                limit: ADMIN_LOG_PAGE_SIZE as i32,
            })
            .await?;
        let peers = self.peers_from_entities(page.users, page.chats);
        let mut refs = std::collections::HashMap::new();
        for (id, peer) in &peers {
            if let Some(key) = id.bot_api_dialog_id()
                && let Ok(Some(reference)) = peer.to_ref().await
            {
                refs.insert(key, reference);
            }
        }
        let entries = page
            .events
            .into_iter()
            .map(|enums::ChannelAdminLogEvent::Event(event)| {
                let id = PeerId::user(event.user_id);
                let author = id.and_then(|id| peers.get(&id)).and_then(peer_author);
                let filter_id = id
                    .and_then(PeerId::bot_api_dialog_id)
                    .filter(|key| refs.contains_key(key));
                let actor = AdminLogActor {
                    filter_id,
                    name: author
                        .as_ref()
                        .map_or_else(|| "Недоступный аккаунт".into(), |peer| peer.name.clone()),
                    avatar: author.and_then(|peer| peer.avatar),
                };
                let lookup = |id: Option<PeerId>| {
                    let author = peer_author(peers.get(&id?)?)?;
                    snippet(&author.name)
                };
                let (action, details) = describe(&event.action, &lookup);
                AdminLogEntry {
                    id: event.id,
                    date: event.date,
                    actor,
                    action: action.into(),
                    details,
                }
            })
            .collect();
        self.peers.write().await.extend(refs);
        Ok(AdminLogPage::new(entries, before))
    }
}

fn event_filter(filter: AdminLogFilter) -> Option<enums::ChannelAdminLogEventsFilter> {
    use AdminLogFilter as F;
    if filter == F::All {
        return None;
    }
    Some(
        types::ChannelAdminLogEventsFilter {
            join: filter == F::Members,
            leave: filter == F::Members,
            invite: filter == F::Members,
            ban: filter == F::Permissions,
            unban: filter == F::Permissions,
            kick: filter == F::Permissions,
            unkick: filter == F::Permissions,
            promote: filter == F::Permissions,
            demote: filter == F::Permissions,
            edit_rank: filter == F::Permissions,
            info: filter == F::Settings,
            settings: filter == F::Settings,
            pinned: filter == F::Messages,
            edit: filter == F::Messages,
            delete: filter == F::Messages,
            send: filter == F::Messages,
            group_call: filter == F::Calls,
            invites: filter == F::Invites,
            forums: filter == F::Topics,
            sub_extend: filter == F::Members,
        }
        .into(),
    )
}

fn describe(
    action: &enums::ChannelAdminLogEventAction,
    name: &impl Fn(Option<PeerId>) -> Option<String>,
) -> (&'static str, Option<String>) {
    use enums::ChannelAdminLogEventAction as A;
    let participant =
        |value: &enums::ChannelParticipant| name(channel_member(value).map(|(id, _)| id));
    let topic = |value: &enums::ForumTopic| match value {
        enums::ForumTopic::Topic(topic) => snippet(&topic.title),
        enums::ForumTopic::Deleted(_) => None,
    };
    let caller = |enums::GroupCallParticipant::Participant(value): &enums::GroupCallParticipant| {
        name(raw_peer_id(&value.peer))
    };
    match action {
        A::ChangeTitle(change) => {
            changed("Изменено название", &change.prev_value, &change.new_value)
        }
        A::ChangeAbout(change) => {
            changed("Изменено описание", &change.prev_value, &change.new_value)
        }
        A::ChangeUsername(change) => changed(
            "Изменено публичное имя",
            &change.prev_value,
            &change.new_value,
        ),
        A::ChangeUsernames(change) => changed(
            "Изменены публичные имена",
            &change.prev_value.join(", "),
            &change.new_value.join(", "),
        ),
        A::ChangePhoto(_) => ("Изменена фотография чата", None),
        A::ToggleInvites(value) => toggle(
            value.new_value,
            "Разрешены приглашения участников",
            "Ограничены приглашения участников",
        ),
        A::ToggleSignatures(value) => toggle(
            value.new_value,
            "Включены подписи авторов",
            "Выключены подписи авторов",
        ),
        A::ToggleSignatureProfiles(value) => toggle(
            value.new_value,
            "Профили авторов показываются",
            "Профили авторов скрыты",
        ),
        A::UpdatePinned(value) => {
            let title = if matches!(&value.message, enums::Message::Message(message) if message.pinned)
            {
                "Сообщение закреплено"
            } else {
                "Сообщение откреплено"
            };
            (title, message_excerpt(&value.message))
        }
        A::EditMessage(value) => {
            let before = message_excerpt(&value.prev_message);
            let after = message_excerpt(&value.new_message);
            if before.is_none() && after.is_none() {
                ("Сообщение изменено", None)
            } else {
                changed(
                    "Сообщение изменено",
                    before.as_deref().unwrap_or(""),
                    after.as_deref().unwrap_or(""),
                )
            }
        }
        A::DeleteMessage(value) => ("Сообщение удалено", message_excerpt(&value.message)),
        A::SendMessage(value) => ("Сообщение отправлено", message_excerpt(&value.message)),
        A::StopPoll(value) => ("Опрос завершён", message_excerpt(&value.message)),
        A::ParticipantJoin => ("Участник вступил в чат", None),
        A::ParticipantLeave => ("Участник вышел из чата", None),
        A::ParticipantInvite(value) => ("Участник добавлен", participant(&value.participant)),
        A::ParticipantToggleBan(value) => {
            let role = |participant| channel_member(participant).and_then(|(_, role)| role);
            let title = match (role(&value.prev_participant), role(&value.new_participant)) {
                (Some(MemberRole::Banned), Some(MemberRole::Banned)) => {
                    "Изменены параметры блокировки"
                }
                (Some(MemberRole::Banned), _) => "Участник разблокирован",
                (_, Some(MemberRole::Banned)) => "Участник заблокирован",
                (Some(_), None) => "Участник удалён",
                _ => "Изменены ограничения участника",
            };
            (
                title,
                participant(&value.new_participant)
                    .or_else(|| participant(&value.prev_participant)),
            )
        }
        A::ParticipantToggleAdmin(value) => {
            let before = participant_rights(&value.prev_participant);
            let after = participant_rights(&value.new_participant);
            let title = match (before.is_some(), after.is_some()) {
                (false, true) => "Назначен администратор",
                (true, false) => "Сняты права администратора",
                _ => "Изменены права администратора",
            };
            let (_, rights) = changed(
                title,
                before.as_deref().unwrap_or("Участник"),
                after.as_deref().unwrap_or("Участник"),
            );
            (title, combine(participant(&value.new_participant), rights))
        }
        A::ParticipantEditRank(value) => {
            let (title, tags) = changed("Изменён тег участника", &value.prev_rank, &value.new_rank);
            (title, combine(name(PeerId::user(value.user_id)), tags))
        }
        A::ParticipantSubExtend(value) => {
            ("Продлена подписка", participant(&value.new_participant))
        }
        A::ChangeStickerSet(_) => ("Изменён набор стикеров", None),
        A::ChangeEmojiStickerSet(_) => ("Изменён набор эмодзи", None),
        A::TogglePreHistoryHidden(value) => toggle(
            value.new_value,
            "История скрыта для новых участников",
            "История видна новым участникам",
        ),
        A::DefaultBannedRights(_) => ("Изменены общие разрешения участников", None),
        A::ChangeLinkedChat(value) => changed(
            "Изменён связанный чат",
            &name(PeerId::channel(value.prev_value)).unwrap_or_default(),
            &name(PeerId::channel(value.new_value)).unwrap_or_default(),
        ),
        A::ChangeLocation(_) => ("Изменена геопозиция", None),
        A::ToggleSlowMode(value) => changed(
            "Изменён медленный режим",
            &seconds(value.prev_value),
            &seconds(value.new_value),
        ),
        A::ChangeHistoryTtl(value) => changed(
            "Изменено автоудаление сообщений",
            &seconds(value.prev_value),
            &seconds(value.new_value),
        ),
        A::ToggleNoForwards(value) => toggle(
            value.new_value,
            "Включена защита содержимого",
            "Выключена защита содержимого",
        ),
        A::ChangeAvailableReactions(_) => ("Изменены доступные реакции", None),
        A::StartGroupCall(_) => ("Начат видеочат", None),
        A::DiscardGroupCall(_) => ("Видеочат завершён", None),
        A::ParticipantMute(value) => ("Микрофон участника выключен", caller(&value.participant)),
        A::ParticipantUnmute(value) => ("Микрофон участника включён", caller(&value.participant)),
        A::ParticipantVolume(value) => ("Изменена громкость участника", caller(&value.participant)),
        A::ToggleGroupCallSetting(value) => toggle(
            value.join_muted,
            "Микрофоны новых участников выключены",
            "Новые участники могут говорить",
        ),
        A::ParticipantJoinByInvite(value) => (
            if value.via_chatlist {
                "Вступление через папку чатов"
            } else {
                "Вступление по ссылке"
            },
            invite_label(&value.invite),
        ),
        A::ParticipantJoinByRequest(value) => (
            "Вступление по одобренной заявке",
            combine(
                name(PeerId::user(value.approved_by))
                    .map(|name| format!("Одобрил заявку · {name}")),
                invite_label(&value.invite),
            ),
        ),
        A::ExportedInviteDelete(value) => (
            "Пригласительная ссылка удалена",
            invite_label(&value.invite),
        ),
        A::ExportedInviteRevoke(value) => (
            "Пригласительная ссылка отозвана",
            invite_label(&value.invite),
        ),
        A::ExportedInviteEdit(value) => changed(
            "Пригласительная ссылка изменена",
            &invite_details(&value.prev_invite),
            &invite_details(&value.new_invite),
        ),
        A::ToggleForum(value) => toggle(value.new_value, "Включены темы", "Выключены темы"),
        A::CreateTopic(value) => ("Тема создана", topic(&value.topic)),
        A::DeleteTopic(value) => ("Тема удалена", topic(&value.topic)),
        A::EditTopic(value) => {
            let before = topic(&value.prev_topic).unwrap_or_default();
            let after = topic(&value.new_topic).unwrap_or_default();
            if before == after {
                ("Тема изменена", Some(after).filter(|text| !text.is_empty()))
            } else {
                changed("Тема изменена", &before, &after)
            }
        }
        A::PinTopic(value) => {
            if let Some(new) = &value.new_topic {
                ("Тема закреплена", topic(new))
            } else {
                ("Тема откреплена", value.prev_topic.as_ref().and_then(topic))
            }
        }
        A::ToggleAntiSpam(value) => {
            toggle(value.new_value, "Включён антиспам", "Выключен антиспам")
        }
        A::ChangePeerColor(_) => ("Изменён цвет чата", None),
        A::ChangeProfilePeerColor(_) => ("Изменён цвет профиля чата", None),
        A::ChangeWallpaper(_) => ("Изменены обои чата", None),
        A::ChangeEmojiStatus(_) => ("Изменён эмодзи-статус чата", None),
        A::ToggleAutotranslation(value) => toggle(
            value.new_value,
            "Включён автоматический перевод",
            "Выключен автоматический перевод",
        ),
    }
}

fn participant_rights(participant: &enums::ChannelParticipant) -> Option<String> {
    match participant {
        enums::ChannelParticipant::Creator(_) => Some("Владелец".into()),
        enums::ChannelParticipant::Admin(admin) => {
            let rights = model_admin_rights(&admin.admin_rights);
            let items = AdminRight::OPTIONS
                .iter()
                .filter_map(|(right, label)| rights.has(*right).then_some(*label))
                .collect::<Vec<_>>();
            Some(if items.is_empty() {
                "Основные права администратора".into()
            } else {
                items.join(", ")
            })
        }
        _ => None,
    }
}

fn invite_label(invite: &enums::ExportedChatInvite) -> Option<String> {
    let enums::ExportedChatInvite::ChatInviteExported(invite) = invite else {
        return None;
    };
    snippet(invite.title.as_deref().unwrap_or("")).or_else(|| snippet(&invite.link))
}

fn invite_details(invite: &enums::ExportedChatInvite) -> String {
    let enums::ExportedChatInvite::ChatInviteExported(raw) = invite else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(label) = invite_label(invite) {
        parts.push(label);
    }
    parts.push(if raw.request_needed {
        "Требуется одобрение".into()
    } else {
        "Свободное вступление".into()
    });
    if let Some(limit) = raw.usage_limit.filter(|limit| *limit > 0) {
        parts.push(format!("Лимит вступлений · {limit}"));
    }
    if let Some(expires) = raw.expire_date.filter(|date| *date > 0)
        && let Ok(date) = gtk::glib::DateTime::from_unix_local(i64::from(expires))
        && let Ok(formatted) = date.format("%d.%m.%Y %H:%M")
    {
        parts.push(format!("До {formatted}"));
    }
    parts.join("\n")
}

fn toggle(enabled: bool, on: &'static str, off: &'static str) -> (&'static str, Option<String>) {
    (if enabled { on } else { off }, None)
}

fn changed(title: &'static str, before: &str, after: &str) -> (&'static str, Option<String>) {
    let before = snippet(before).unwrap_or_else(|| "—".into());
    let after = snippet(after).unwrap_or_else(|| "—".into());
    (title, Some(format!("Было\n{before}\n\nСтало\n{after}")))
}

fn combine(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}\n\n{second}")),
        (first, second) => first.or(second),
    }
}

fn seconds(value: i32) -> String {
    if value > 0 {
        format!("{value} с")
    } else {
        "Выключено".into()
    }
}

fn snippet(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut chars = text.chars();
    let mut text: String = chars.by_ref().take(500).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    Some(text)
}

fn message_excerpt(message: &enums::Message) -> Option<String> {
    let enums::Message::Message(message) = message else {
        return None;
    };
    excerpt(
        &message.message,
        message.entities.as_deref().unwrap_or_default(),
        message.media.as_ref(),
        message.noforwards || message.ttl_period.is_some(),
    )
}

fn excerpt(
    text: &str,
    entities: &[enums::MessageEntity],
    media: Option<&enums::MessageMedia>,
    protected: bool,
) -> Option<String> {
    let ephemeral = match media {
        Some(enums::MessageMedia::Photo(photo)) => photo.ttl_seconds.is_some(),
        Some(enums::MessageMedia::Document(document)) => document.ttl_seconds.is_some(),
        _ => false,
    };
    if protected || ephemeral {
        return None;
    }
    if let Some(text) = snippet(&quote_preview(text, entities)) {
        return Some(text);
    }
    match media {
        Some(enums::MessageMedia::Poll(poll)) => {
            let enums::Poll::Poll(poll) = &poll.poll;
            snippet(&poll_question(&poll.question))
        }
        Some(enums::MessageMedia::Empty) | None => None,
        _ => Some("Вложение".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_log_filters_map_to_the_matching_server_events() {
        assert!(event_filter(AdminLogFilter::All).is_none());
        let Some(enums::ChannelAdminLogEventsFilter::Filter(roles)) =
            event_filter(AdminLogFilter::Permissions)
        else {
            panic!("missing permissions filter");
        };
        assert!(roles.ban && roles.unban && roles.promote && roles.demote && roles.edit_rank);
        assert!(!roles.join && !roles.invites && !roles.pinned);
        let Some(enums::ChannelAdminLogEventsFilter::Filter(messages)) =
            event_filter(AdminLogFilter::Messages)
        else {
            panic!("missing messages filter");
        };
        assert!(messages.pinned && messages.edit && messages.delete && messages.send);
        assert!(!messages.ban && !messages.group_call && !messages.info);
    }

    #[test]
    fn journal_excerpts_hide_spoilers_and_expiring_or_protected_media_captions() {
        let text = "Тест и секрет";
        let mark = types::MessageEntitySpoiler {
            offset: 7,
            length: 6,
        }
        .into();
        let visible = excerpt(text, &[mark], None, false).unwrap();
        assert!(visible.starts_with("Тест и ") && !visible.contains("секрет"));
        assert!(excerpt(text, &[], None, true).is_none());
        let photo = types::MessageMediaPhoto {
            spoiler: false,
            live_photo: false,
            photo: None,
            ttl_seconds: Some(10),
            video: None,
        }
        .into();
        assert!(excerpt(text, &[], Some(&photo), false).is_none());
    }
}
