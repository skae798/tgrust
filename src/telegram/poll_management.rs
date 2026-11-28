use anyhow::{Context, Result, bail, ensure};
use grammers_client::tl::functions::messages::EditMessage;

use super::{
    Media, Peer, PeerId, Telegram, TelegramMessage, enums, poll_definition, poll_results,
    poll_update, raw_results, types,
};
use crate::model::PollUpdate;

impl Telegram {
    pub async fn close_poll(&self, chat: i64, id: i32, expected: i64) -> Result<PollUpdate> {
        let peer = self.peer(chat).await?;
        let message = self
            .message_by_id(peer, id)
            .await?
            .context("Опрос больше недоступен")?;
        let media = message_poll(&message.raw).context("Опрос больше недоступен")?;
        let enums::Poll::Poll(poll) = &media.poll;
        ensure!(poll.id == expected, "Опрос больше недоступен");
        let target = (chat, id);
        if let Some(closed) = closed_snapshot(media, target, expected) {
            return Ok(closed);
        }
        ensure!(
            can_close_poll(&message),
            "У аккаунта нет права завершить этот опрос"
        );

        let request = EditMessage {
            no_webpage: false,
            invert_media: false,
            peer: peer.into(),
            id,
            message: None,
            media: Some(closing_media(media)?.into()),
            reply_markup: None,
            entities: None,
            schedule_date: None,
            schedule_repeat_period: None,
            quick_reply_shortcut_id: None,
            rich_message: None,
        };
        match self.client.invoke(&request).await {
            Ok(updates) => {
                if let Some(closed) = closed_update(updates, target, expected) {
                    return Ok(closed);
                }
            }
            Err(error) if error.is("MESSAGE_NOT_MODIFIED") || error.is("POLL_CLOSED") => {}
            Err(error) => return Err(error.into()),
        }
        let refreshed = self.message_by_id(peer, id).await?;
        refreshed
            .as_ref()
            .and_then(|message| message_poll(&message.raw))
            .and_then(|media| closed_snapshot(media, target, expected))
            .context("Telegram ещё не подтвердил завершение опроса")
    }
}

pub(super) fn can_close_poll(message: &TelegramMessage) -> bool {
    let enums::Message::Message(raw) = &message.raw else {
        return false;
    };
    if raw.id <= 0
        || raw.fwd_from.is_some()
        || raw.via_bot_id.is_some()
        || raw.via_business_bot_id.is_some()
        || raw.ttl_period.is_some()
    {
        return false;
    }
    if !raw.post {
        return raw.out;
    }
    match message.peer() {
        Some(Peer::Channel(channel)) if !channel.raw.left => {
            can_edit_channel_poll(raw.out, channel.admin_rights())
        }
        Some(_) => false,
        None => raw.out,
    }
}

fn can_edit_channel_poll(own: bool, rights: Option<&types::ChatAdminRights>) -> bool {
    rights.is_some_and(|rights| rights.edit_messages || (own && rights.post_messages))
}

fn message_poll(raw: &enums::Message) -> Option<&types::MessageMediaPoll> {
    match raw {
        enums::Message::Message(message) => match &message.media {
            Some(enums::MessageMedia::Poll(media)) => Some(media),
            _ => None,
        },
        _ => None,
    }
}

fn closing_media(media: &types::MessageMediaPoll) -> Result<types::InputMediaPoll> {
    let enums::Poll::Poll(mut poll) = media.poll.clone();
    let results = raw_results(&media.results);
    let mut correct = Vec::new();
    for (index, answer) in poll.answers.iter_mut().enumerate() {
        let enums::PollAnswer::Answer(answer_data) = answer else {
            bail!("Telegram вернул неизвестный вариант ответа");
        };
        if let Some(results) = &results.results
            && results.iter().any(|result| {
                let enums::PollAnswerVoters::Voters(result) = result;
                result.correct && result.option == answer_data.option
            })
        {
            correct.push(index as i32);
        }
        *answer = types::InputPollAnswer {
            text: answer_data.text.clone(),
            media: poll_input_media(answer_data.media.as_ref())?,
        }
        .into();
    }
    poll.closed = true;
    poll.creator = false;
    poll.hash = 0;
    let correct_answers = poll.quiz.then_some(correct);
    Ok(types::InputMediaPoll {
        poll: poll.into(),
        correct_answers,
        solution: results.solution.clone(),
        solution_entities: results
            .solution
            .as_ref()
            .map(|_| results.solution_entities.clone().unwrap_or_default()),
        attached_media: poll_input_media(media.attached_media.as_ref())?,
        solution_media: poll_input_media(results.solution_media.as_ref())?,
    })
}

fn poll_input_media(media: Option<&enums::MessageMedia>) -> Result<Option<enums::InputMedia>> {
    media
        .map(|media| {
            Media::from_raw(media.clone())
                .and_then(|media| media.to_raw_input_media())
                .context("Telegram не вернул пригодное для отправки вложение опроса")
        })
        .transpose()
}

fn closed_snapshot(
    media: &types::MessageMediaPoll,
    target: (i64, i32),
    expected: i64,
) -> Option<PollUpdate> {
    let enums::Poll::Poll(poll) = &media.poll;
    (poll.id == expected && poll.closed).then(|| {
        poll_results(
            expected,
            Some(target),
            Some(poll_definition(poll)),
            raw_results(&media.results),
        )
    })
}

fn closed_update(updates: enums::Updates, target: (i64, i32), expected: i64) -> Option<PollUpdate> {
    let updates = match updates {
        enums::Updates::Updates(updates) => updates.updates,
        enums::Updates::Combined(updates) => updates.updates,
        enums::Updates::UpdateShort(update) => vec![update.update],
        _ => return None,
    };
    updates.into_iter().find_map(|update| {
        let edited = match update {
            enums::Update::EditMessage(update) => update.message,
            enums::Update::EditChannelMessage(update) => update.message,
            enums::Update::MessagePoll(raw) if raw.poll_id == expected => {
                let mut update = poll_update(&raw);
                if update.definition.as_ref().is_some_and(|poll| poll.closed)
                    && update.target.is_none_or(|address| address == target)
                {
                    update.target = Some(target);
                    return Some(update);
                }
                return None;
            }
            _ => return None,
        };
        let enums::Message::Message(message) = &edited else {
            return None;
        };
        if message.id != target.1
            || PeerId::from(message.peer_id.clone()).bot_api_dialog_id() != Some(target.0)
        {
            return None;
        }
        closed_snapshot(message_poll(&edited)?, target, expected)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AdminRight, AdminRights};
    use crate::telegram::telegram_admin_rights;

    fn quiz() -> types::MessageMediaPoll {
        let text = |text: &str| {
            types::TextWithEntities {
                text: text.into(),
                entities: Vec::new(),
            }
            .into()
        };
        let option = |name, key, media| {
            types::PollAnswer {
                text: text(name),
                option: vec![key],
                media,
                added_by: None,
                date: None,
            }
            .into()
        };
        let photo: enums::MessageMedia = types::MessageMediaPhoto {
            spoiler: true,
            live_photo: false,
            photo: Some(
                types::Photo {
                    has_stickers: false,
                    id: 48,
                    access_hash: 15,
                    file_reference: vec![42],
                    date: 0,
                    sizes: Vec::new(),
                    video_sizes: None,
                    dc_id: 2,
                }
                .into(),
            ),
            ttl_seconds: None,
            video: None,
        }
        .into();
        types::MessageMediaPoll {
            poll: types::Poll {
                id: 123,
                closed: false,
                public_voters: true,
                multiple_choice: true,
                quiz: true,
                open_answers: false,
                revoting_disabled: true,
                shuffle_answers: true,
                hide_results_until_close: true,
                creator: true,
                subscribers_only: true,
                question: text("Сколько дней в неделе?"),
                answers: vec![
                    option("Пять", 28, None),
                    option("Семь", 5, Some(photo.clone())),
                ],
                close_period: Some(1800),
                close_date: None,
                countries_iso2: None,
                hash: 28,
            }
            .into(),
            results: types::PollResults {
                min: false,
                has_unread_votes: false,
                can_view_stats: false,
                results: Some(vec![
                    types::PollAnswerVoters {
                        chosen: true,
                        correct: true,
                        option: vec![5],
                        voters: Some(3),
                        recent_voters: Some(Vec::new()),
                    }
                    .into(),
                ]),
                total_voters: Some(3),
                recent_voters: None,
                solution: Some("Семь дней.".into()),
                solution_entities: Some(Vec::new()),
                solution_media: Some(photo.clone()),
            }
            .into(),
            attached_media: Some(photo),
        }
    }

    #[test]
    fn channel_close_requires_edit_right_or_own_post_with_post_right() {
        let post = telegram_admin_rights(AdminRights::from_rights([AdminRight::PostMessages]));
        let edit = telegram_admin_rights(AdminRights::from_rights([AdminRight::EditMessages]));
        let delete = telegram_admin_rights(AdminRights::from_rights([AdminRight::DeleteMessages]));
        assert!(can_edit_channel_poll(false, Some(&edit)));
        assert!(can_edit_channel_poll(true, Some(&post)));
        assert!(!can_edit_channel_poll(false, Some(&post)));
        assert!(!can_edit_channel_poll(false, Some(&delete)));
        assert!(!can_edit_channel_poll(true, None));
    }

    #[test]
    fn close_encodes_quiz_and_media_without_changing_options_or_settings() {
        let source = quiz();
        let edit = closing_media(&source).unwrap();
        let enums::Poll::Poll(poll) = edit.poll;
        let enums::Poll::Poll(original) = source.poll;
        assert!(poll.closed && poll.quiz && poll.hide_results_until_close && poll.subscribers_only);
        assert!(!poll.creator);
        assert_eq!(poll.hash, 0);
        assert_eq!(poll.id, original.id);
        assert_eq!(poll.question, original.question);
        assert_eq!(poll.close_period, original.close_period);
        let enums::PollAnswer::InputPollAnswer(answer) = &poll.answers[1] else {
            panic!("expected an outgoing poll answer");
        };
        let enums::PollAnswer::Answer(original_answer) = &original.answers[1] else {
            panic!("expected a stored poll answer");
        };
        assert_eq!(answer.text, original_answer.text);
        assert!(matches!(answer.media, Some(enums::InputMedia::Photo(_))));
        assert_eq!(edit.correct_answers, Some(vec![1]));
        assert_eq!(edit.solution.as_deref(), Some("Семь дней."));
        assert_eq!(edit.solution_entities, Some(vec![]));
        assert!(matches!(
            edit.attached_media,
            Some(enums::InputMedia::Photo(_))
        ));
        assert!(matches!(
            edit.solution_media,
            Some(enums::InputMedia::Photo(_))
        ));
    }

    #[test]
    fn only_the_requested_closed_poll_is_applied_and_stays_closed() {
        let target = (61, 17);
        let mut source = quiz();
        assert!(closed_snapshot(&source, target, 123).is_none());
        let enums::Poll::Poll(poll) = &mut source.poll;
        let mut view = poll_definition(poll);
        let stale_definition = view.clone();
        poll.closed = true;
        assert!(closed_snapshot(&source, target, 456).is_none());
        let mut closed = closed_snapshot(&source, target, 123).unwrap();
        assert!(!closed.apply(61, 18, &mut view));
        assert!(closed.apply(61, 17, &mut view));
        assert_eq!(view.total, Some(3));
        assert!(view.has_vote());
        closed.results = None;
        closed.definition = Some(stale_definition);
        closed.total = Some(1);
        assert!(!closed.apply(61, 17, &mut view));
        assert_eq!(view.total, Some(3));
        assert!(!view.can_vote());
    }

    #[test]
    fn short_edit_response_requires_a_matching_peer_and_closed_definition() {
        let peer: enums::Peer = types::PeerUser { user_id: 61 }.into();
        let target = (61, 17);
        let mut source = quiz();
        let enums::Poll::Poll(poll) = &mut source.poll;
        poll.closed = true;
        let ack: enums::Updates = types::UpdateShort {
            update: types::UpdateMessagePoll {
                peer: Some(peer),
                msg_id: Some(17),
                top_msg_id: None,
                poll_id: 123,
                poll: Some(source.poll),
                results: source.results,
            }
            .into(),
            date: 0,
        }
        .into();
        assert!(closed_update(ack.clone(), (62, 17), 123).is_none());
        assert!(closed_update(ack.clone(), (61, 18), 123).is_none());
        assert!(closed_update(ack.clone(), target, 456).is_none());
        assert_eq!(
            closed_update(ack, target, 123).unwrap().target,
            Some(target)
        );
    }
}
