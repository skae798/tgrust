use std::time::Duration;

use grammers_client::media::Poll as TelegramPoll;
use grammers_client::tl::functions::help::GetAppConfig;

use super::{Media, Telegram, TelegramMessage, enums, poll_results, poll_view};
use crate::model::{POLL_DEFAULT_MAX_SECONDS, POLL_MIN_SECONDS, Poll, PollUpdate};

impl Telegram {
    pub(super) async fn poll_close_limit(&self, timed: bool) -> i32 {
        if !timed {
            return POLL_DEFAULT_MAX_SECONDS;
        }
        let config = tokio::time::timeout(
            Duration::from_secs(4),
            self.client.invoke(&GetAppConfig { hash: 0 }),
        )
        .await;
        match config {
            Ok(Ok(enums::help::AppConfig::Config(config))) => {
                close_limit(&config.config).unwrap_or(POLL_DEFAULT_MAX_SECONDS)
            }
            _ => POLL_DEFAULT_MAX_SECONDS,
        }
    }

    pub(crate) async fn refresh_timed_polls(
        &self,
        chat: i64,
        expected: &[(i32, i64)],
    ) -> anyhow::Result<Vec<PollUpdate>> {
        let peer = self.peer(chat).await?;
        let mut waiting = expected.to_vec();
        let mut confirmed = Vec::new();
        for delay in [0, 3] {
            if waiting.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
            let updates = tokio::time::timeout(
                Duration::from_secs(4),
                self.timed_poll_snapshots(peer, &waiting),
            )
            .await;
            if let Ok(Ok(updates)) = updates {
                for update in updates {
                    if update.definition.as_ref().is_some_and(|poll| poll.closed) {
                        waiting.retain(|&(id, poll)| {
                            update.target != Some((chat, id)) || update.id != poll
                        });
                        confirmed.push(update);
                    }
                }
            }
        }
        Ok(confirmed)
    }

    async fn timed_poll_snapshots(
        &self,
        peer: super::PeerRef,
        expected: &[(i32, i64)],
    ) -> anyhow::Result<Vec<PollUpdate>> {
        let Some(chat) = peer.id.bot_api_dialog_id() else {
            return Ok(Vec::new());
        };
        let mut updates = Vec::new();
        for batch in expected.chunks(100) {
            let ids: Vec<_> = batch.iter().map(|(id, _)| *id).collect();
            let messages = self.client.get_messages_by_id(peer, &ids).await?;
            for ((id, expected), message) in batch.iter().zip(messages) {
                if let Some(message) = message
                    && let Some(Media::Poll(raw)) = message.media()
                    && raw.raw.id == *expected
                {
                    updates.push(poll_results(
                        *expected,
                        Some((chat, *id)),
                        Some(poll_view_at(&raw, &message)),
                        &raw.raw_results,
                    ));
                }
            }
        }
        Ok(updates)
    }
}

pub(super) fn poll_view_at(raw: &TelegramPoll, message: &TelegramMessage) -> Poll {
    let mut poll = poll_view(raw);
    poll.closes_at = poll.closes_at.or_else(|| {
        let period = raw.raw.close_period.filter(|period| *period > 0)?;
        let original_date = message.forward_header().map_or_else(
            || message.date().as_second(),
            |enums::MessageFwdHeader::Header(header)| i64::from(header.date),
        );
        Some(original_date.saturating_add(i64::from(period)))
    });
    poll
}

fn close_limit(config: &enums::Jsonvalue) -> Option<i32> {
    let enums::Jsonvalue::JsonObject(config) = config else {
        return None;
    };
    config.value.iter().find_map(|entry| {
        let enums::JsonobjectValue::JsonObjectValue(entry) = entry;
        if entry.key != "poll_close_period_max" {
            return None;
        }
        let enums::Jsonvalue::JsonNumber(number) = &entry.value else {
            return None;
        };
        let value = number.value;
        (value.fract() == 0.0
            && (f64::from(POLL_MIN_SECONDS)..=f64::from(i32::MAX)).contains(&value))
        .then_some(value as i32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::types;

    fn settings(value: f64) -> enums::Jsonvalue {
        types::JsonObject {
            value: vec![
                types::JsonObjectValue {
                    key: "poll_close_period_max".into(),
                    value: types::JsonNumber { value }.into(),
                }
                .into(),
            ],
        }
        .into()
    }

    #[test]
    fn poll_config_uses_a_positive_whole_server_limit() {
        assert_eq!(close_limit(&settings(5_000.0)), Some(5_000));
        assert_eq!(close_limit(&settings(59.0)), None);
        assert_eq!(close_limit(&settings(120.5)), None);
        assert_eq!(close_limit(&settings(f64::INFINITY)), None);
    }
}
