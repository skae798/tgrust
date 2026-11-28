use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use grammers_client::tl::functions::help::GetAppConfig;
use grammers_client::tl::functions::messages::{AddPollAnswer, DeletePollAnswer};

use super::poll_timing::poll_view_at;
use super::{
    Media, PeerId, PeerRef, Telegram, can_write_chat, enums, poll_results, poll_update, types,
};
use crate::model::{
    MAX_POLL_CHOICES, POLL_ANSWER_DELETE_PERIOD, PollAnswerAddition, PollAnswerChange, PollUpdate,
    normalized_poll_answer, restriction_now,
};

impl Telegram {
    pub async fn change_poll_answer(
        &self,
        chat: i64,
        id: i32,
        expected: i64,
        change: PollAnswerChange,
    ) -> Result<Option<PollUpdate>> {
        let peer = self.peer(chat).await?;
        let (max_answers, delete_period) = self.poll_answer_limits().await;
        let message = self
            .message_by_id(peer, id)
            .await?
            .context("Опрос больше недоступен")?;
        let Some(Media::Poll(raw)) = message.media() else {
            bail!("Опрос больше недоступен")
        };
        let poll = poll_view_at(&raw, &message);
        ensure!(poll.id == expected, "Опрос больше недоступен");
        ensure!(
            message.forward_header().is_none()
                && message.peer().is_none_or(can_write_chat)
                && poll.answers_open(),
            "Изменение вариантов опроса недоступно",
        );

        let own_id = self.self_id().await?.and_then(PeerId::bot_api_dialog_id);
        let change = match change {
            PollAnswerChange::Add(text) => PollAnswerChange::Add(normalized_poll_answer(&text)?),
            change => change,
        };
        let snapshot = || {
            poll_results(
                expected,
                Some((chat, id)),
                Some(poll.clone()),
                &raw.raw_results,
            )
        };
        let reply = match &change {
            PollAnswerChange::Add(text) => {
                if let Some(choice) = poll.choices.iter().find(|choice| &choice.label == text) {
                    if choice
                        .addition
                        .is_some_and(|addition| Some(addition.author) == own_id)
                    {
                        return Ok(Some(snapshot()));
                    }
                    bail!("Такой вариант уже есть");
                }
                ensure!(
                    poll.choices.len() < max_answers,
                    "Достигнут лимит вариантов ({max_answers})"
                );
                self.client
                    .invoke(&AddPollAnswer {
                        peer: peer.into(),
                        msg_id: id,
                        answer: types::InputPollAnswer {
                            text: types::TextWithEntities {
                                text: text.clone(),
                                entities: Vec::new(),
                            }
                            .into(),
                            media: None,
                        }
                        .into(),
                    })
                    .await
            }
            PollAnswerChange::Delete(option) => {
                let Some(choice) = poll.choices.iter().find(|choice| &choice.option == option)
                else {
                    return Ok(Some(snapshot()));
                };
                ensure!(
                    poll.can_delete_answer(choice, own_id, restriction_now(), delete_period),
                    "Удаление этого варианта недоступно",
                );
                self.client
                    .invoke(&DeletePollAnswer {
                        peer: peer.into(),
                        msg_id: id,
                        option: option.clone(),
                    })
                    .await
            }
        };
        let observes_change = |update: &PollUpdate| {
            update
                .definition
                .as_ref()
                .is_some_and(|definition| match &change {
                    PollAnswerChange::Add(text) => definition
                        .choices
                        .iter()
                        .any(|choice| &choice.label == text && choice.addition.is_some()),
                    PollAnswerChange::Delete(option) => !definition
                        .choices
                        .iter()
                        .any(|choice| &choice.option == option),
                })
        };

        match &reply {
            Ok(updates) => {
                if let Some(update) =
                    answer_update(updates, (chat, id), expected).filter(&observes_change)
                {
                    return Ok(Some(update));
                }
            }
            Err(error) if !super::retry_update_error(error) => {
                return Err(reply.unwrap_err().into());
            }
            Err(_) => {}
        }
        let confirmed = tokio::time::timeout(
            Duration::from_secs(5),
            self.poll_answer_snapshot(peer, chat, id, expected),
        )
        .await;
        match confirmed {
            Ok(Ok(Some(update))) if observes_change(&update) => Ok(Some(update)),
            _ => reply.map(|_| None).map_err(Into::into),
        }
    }

    pub async fn refresh_poll_answers(
        &self,
        chat: i64,
        id: i32,
        expected: i64,
    ) -> Result<Option<PollUpdate>> {
        self.poll_answer_snapshot(self.peer(chat).await?, chat, id, expected)
            .await
    }

    async fn poll_answer_snapshot(
        &self,
        peer: PeerRef,
        chat: i64,
        id: i32,
        expected: i64,
    ) -> Result<Option<PollUpdate>> {
        Ok(self
            .message_by_id(peer, id)
            .await?
            .and_then(|message| match message.media() {
                Some(Media::Poll(raw)) if raw.raw.id == expected => Some(poll_results(
                    expected,
                    Some((chat, id)),
                    Some(poll_view_at(&raw, &message)),
                    &raw.raw_results,
                )),
                _ => None,
            }))
    }

    pub async fn poll_answer_limits(&self) -> (usize, i32) {
        let config = tokio::time::timeout(
            Duration::from_secs(4),
            self.client.invoke(&GetAppConfig { hash: 0 }),
        )
        .await;
        match config {
            Ok(Ok(enums::help::AppConfig::Config(config))) => (
                number_option(&config.config, "poll_answers_max")
                    .filter(|value| *value >= 2)
                    .map_or(MAX_POLL_CHOICES, |value| value as usize),
                number_option(&config.config, "poll_answer_delete_period")
                    .unwrap_or(POLL_ANSWER_DELETE_PERIOD),
            ),
            _ => (MAX_POLL_CHOICES, POLL_ANSWER_DELETE_PERIOD),
        }
    }
}

pub(super) fn answer_addition(answer: &types::PollAnswer) -> Option<PollAnswerAddition> {
    Some(PollAnswerAddition {
        author: super::raw_peer_id(answer.added_by.as_ref()?)?.bot_api_dialog_id()?,
        date: answer.date?,
    })
}

fn number_option(settings: &enums::Jsonvalue, key: &str) -> Option<i32> {
    let enums::Jsonvalue::JsonObject(object) = settings else {
        return None;
    };
    object.value.iter().find_map(|item| {
        let enums::JsonobjectValue::JsonObjectValue(item) = item;
        if item.key != key {
            return None;
        }
        let enums::Jsonvalue::JsonNumber(number) = &item.value else {
            return None;
        };
        let value = number.value;
        (value.fract() == 0.0 && (0.0..=f64::from(i32::MAX)).contains(&value))
            .then_some(value as i32)
    })
}

fn answer_update(
    response: &enums::Updates,
    target: (i64, i32),
    poll_id: i64,
) -> Option<PollUpdate> {
    let updates = match response {
        enums::Updates::Updates(updates) => &updates.updates[..],
        enums::Updates::Combined(updates) => &updates.updates[..],
        enums::Updates::UpdateShort(update) => std::slice::from_ref(&update.update),
        _ => return None,
    };
    updates.iter().find_map(|update| {
        let enums::Update::MessagePoll(raw) = update else {
            return None;
        };
        let mut update = poll_update(raw);
        if update.id != poll_id || update.target.is_some_and(|actual| actual != target) {
            return None;
        }
        update.target = Some(target);
        Some(update)
    })
}

pub(super) fn changed_poll(message: &enums::Message) -> Option<(i64, i32)> {
    let enums::Message::Service(service) = message else {
        return None;
    };
    if !matches!(
        service.action,
        enums::MessageAction::PollAppendAnswer(_) | enums::MessageAction::PollDeleteAnswer(_)
    ) {
        return None;
    }
    let Some(enums::MessageReplyHeader::Header(reply)) = &service.reply_to else {
        return None;
    };
    let peer = super::raw_peer_id(&service.peer_id)?;
    if reply
        .reply_to_peer_id
        .as_ref()
        .is_some_and(|other| super::raw_peer_id(other) != Some(peer))
    {
        return None;
    }
    peer.bot_api_dialog_id()
        .zip(reply.reply_to_msg_id.filter(|id| *id > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_service_message_refreshes_the_referenced_poll_in_the_same_chat() {
        let peer: enums::Peer = types::PeerChat { chat_id: 42 }.into();
        let mut header = types::MessageReplyHeader {
            reply_to_scheduled: false,
            forum_topic: true,
            quote: false,
            reply_to_ephemeral: false,
            reply_to_msg_id: Some(10),
            reply_to_peer_id: None,
            reply_from: None,
            reply_media: None,
            reply_to_top_id: Some(3),
            quote_text: None,
            quote_entities: None,
            quote_offset: None,
            todo_item_id: None,
            poll_option: None,
            quote_rich: None,
        };
        let answer: enums::PollAnswer = types::InputPollAnswer {
            text: types::TextWithEntities {
                text: "Новый вариант".into(),
                entities: Vec::new(),
            }
            .into(),
            media: None,
        }
        .into();
        let mut service = types::MessageService {
            out: false,
            mentioned: false,
            media_unread: false,
            reactions_are_possible: false,
            silent: true,
            post: false,
            legacy: false,
            id: 12,
            from_id: None,
            peer_id: peer,
            saved_peer_id: None,
            reply_to: Some(header.clone().into()),
            date: 100,
            action: types::MessageActionPollAppendAnswer {
                answer: answer.clone(),
            }
            .into(),
            reactions: None,
            ttl_period: None,
        };
        assert_eq!(changed_poll(&service.clone().into()), Some((-42, 10)));
        service.action = types::MessageActionPollDeleteAnswer { answer }.into();
        assert_eq!(changed_poll(&service.clone().into()), Some((-42, 10)));
        header.reply_to_peer_id = Some(types::PeerChat { chat_id: 50 }.into());
        service.reply_to = Some(header.into());
        assert_eq!(changed_poll(&service.into()), None);
    }
}
