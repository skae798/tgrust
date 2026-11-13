use std::collections::BTreeMap;

#[derive(Clone)]
pub struct Chat {
    pub id: i64,
    pub title: String,
    pub preview: String,
    pub unread: i32,
    pub broadcast: bool,
}

#[derive(Clone)]
pub struct Message {
    pub id: i32,
    pub sender: String,
    pub timestamp: i64,
    pub text: String,
    pub outgoing: bool,
    pub attachment: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub struct UpdateStatus {
    pub revision: u64,
    pub reconnecting: bool,
}
/// Replace the refreshed range. Retain older pages only when the ranges meet,
/// and preserve messages delivered by a send that completed during the request.
pub fn merge_latest_history(
    current: &mut BTreeMap<i32, Message>,
    latest: Vec<Message>,
    has_older: bool,
    requested_through: Option<i32>,
) -> bool {
    let oldest = latest.first().map(|message| message.id);
    let newest = latest.last().map(|message| message.id);
    let keep_older = has_older
        && oldest
            .zip(requested_through)
            .is_some_and(|(oldest, previous)| oldest <= previous);
    let older_loaded = keep_older
        && current
            .first_key_value()
            .zip(oldest)
            .is_some_and(|((&first, _), oldest)| first < oldest);

    current.retain(|&id, _| {
        (keep_older && oldest.is_some_and(|oldest| id < oldest))
            || (requested_through.is_none_or(|previous| id > previous)
                && newest.is_none_or(|newest| id > newest))
    });
    current.extend(latest.into_iter().map(|message| (message.id, message)));
    older_loaded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(ids: &[i32]) -> Vec<Message> {
        ids.iter()
            .map(|&id| Message {
                id,
                sender: "Тест".into(),
                timestamp: 0,
                text: String::new(),
                outgoing: false,
                attachment: None,
            })
            .collect()
    }

    fn history(ids: &[i32]) -> BTreeMap<i32, Message> {
        messages(ids).into_iter().map(|msg| (msg.id, msg)).collect()
    }

    #[test]
    fn overlapping_refresh_keeps_older_pages_and_drops_deleted_messages() {
        let mut current = history(&[1, 4, 6, 8, 9]);
        assert!(merge_latest_history(
            &mut current,
            messages(&[4, 8, 10]),
            true,
            Some(9),
        ));
        assert_eq!(current.into_keys().collect::<Vec<_>>(), [1, 4, 8, 10]);
    }

    #[test]
    fn non_overlapping_refresh_restarts_paging_without_a_gap() {
        let mut current = history(&[2, 4, 7]);
        assert!(!merge_latest_history(
            &mut current,
            messages(&[50, 51]),
            true,
            Some(7),
        ));
        assert_eq!(current.into_keys().collect::<Vec<_>>(), [50, 51]);
    }

    #[test]
    fn full_snapshot_keeps_only_messages_sent_during_refresh() {
        let mut current = history(&[1, 2, 3, 4]);
        assert!(!merge_latest_history(
            &mut current,
            messages(&[1, 2]),
            false,
            Some(3),
        ));
        assert_eq!(current.into_keys().collect::<Vec<_>>(), [1, 2, 4]);
    }
}
