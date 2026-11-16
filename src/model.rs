use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const MAX_ALBUM_ITEMS: usize = 10;
pub const HISTORY_PAGE_SIZE: usize = 40;
const CACHED_CHATS: usize = 150;
const CACHED_MESSAGES: usize = 400;
const CACHE_RETENTION: Duration = Duration::from_hours(6);
const CACHE_FRESHNESS: Duration = Duration::from_mins(15);

#[derive(Clone)]
pub struct Chat {
    pub id: i64,
    pub title: String,
    pub preview: String,
    pub preview_sender: Option<String>,
    pub preview_image: Option<ChatImage>,
    pub group: bool,
    pub unread: i32,
    pub broadcast: bool,
    pub saved_messages: bool,
    pub archived: bool,
    pub kind: ChatKind,
    pub muted: bool,
    pub avatar: Option<Avatar>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatKind {
    Contact,
    User,
    Group,
    Broadcast,
    Bot,
}

#[derive(Clone, Default)]
pub struct ChatFolder {
    pub id: i32,
    pub title: String,
    pub pinned: Vec<i64>,
    pub included: Vec<i64>,
    pub excluded: Vec<i64>,
    pub kinds: Vec<ChatKind>,
    pub exclude_muted: bool,
    pub exclude_read: bool,
    pub exclude_archived: bool,
}

impl ChatFolder {
    pub fn contains(&self, chat: &Chat) -> bool {
        if self.excluded.contains(&chat.id) {
            return false;
        }
        if self.pinned.contains(&chat.id) || self.included.contains(&chat.id) {
            return true;
        }
        self.kinds.contains(&chat.kind)
            && (!self.exclude_muted || !chat.muted)
            && (!self.exclude_read || chat.unread > 0)
            && (!self.exclude_archived || !chat.archived)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Avatar {
    pub peer: i64,
    pub photo: i64,
    pub dc: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttachmentThumbnail {
    Photo(i64),
    Document(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChatImage {
    Avatar(Avatar),
    Attachment {
        chat: i64,
        message: i32,
        media: AttachmentThumbnail,
    },
}

impl ChatImage {
    pub fn attachment(chat: i64, message: i32, media: AttachmentThumbnail) -> Self {
        Self::Attachment {
            chat,
            message,
            media,
        }
    }
}

#[derive(Clone)]
pub struct SearchHit {
    pub chat: Chat,
    pub message: Message,
}

pub const SEARCH_PAGE_SIZE: usize = 50;

pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub has_more: bool,
}

pub struct SearchHandle {
    pub requests:
        tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<anyhow::Result<SearchPage>>>,
}

impl SearchHandle {
    pub async fn next_page(&self) -> anyhow::Result<SearchPage> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.requests.send(sender).await?;
        receiver.await?
    }
}

#[derive(Default)]
pub struct SearchBuffer {
    pending: VecDeque<SearchHit>,
    seen: HashSet<(i64, i32)>,
    pub exhausted: bool,
}

impl SearchBuffer {
    pub fn push(&mut self, hit: SearchHit) {
        if self.seen.insert((hit.chat.id, hit.message.id)) {
            self.pending.push_back(hit);
        }
    }

    pub fn needs_more(&self) -> bool {
        !self.exhausted && self.pending.len() <= SEARCH_PAGE_SIZE
    }

    pub fn take_page(&mut self) -> SearchPage {
        SearchPage {
            hits: self
                .pending
                .drain(..self.pending.len().min(SEARCH_PAGE_SIZE))
                .collect(),
            has_more: !self.pending.is_empty() || !self.exhausted,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TextStyle {
    Bold,
    Italic,
    Underline,
    Strike,
    Code,
    Spoiler,
}

pub const SPOILER_PLACEHOLDER: &str = "▰▰▰";

pub fn spoiler_ranges(spans: &[TextSpan]) -> Vec<Range<usize>> {
    let mut ranges: Vec<_> = spans
        .iter()
        .filter(|span| span.style == TextStyle::Spoiler)
        .map(|span| span.range.clone())
        .collect();
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && last.end >= range.start
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

pub fn redact_spoilers<'a>(text: &'a str, spans: &[TextSpan]) -> Cow<'a, str> {
    let ranges = spoiler_ranges(spans);
    if ranges.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut redacted = String::new();
    let mut end = 0;
    for range in ranges {
        redacted.push_str(&text[end..range.start]);
        redacted.push_str(SPOILER_PLACEHOLDER);
        end = range.end;
    }
    redacted.push_str(&text[end..]);
    Cow::Owned(redacted)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextSpan {
    // Byte offsets into the UTF-8 message text.
    pub range: Range<usize>,
    pub style: TextStyle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormattedText {
    pub text: String,
    pub spans: Vec<TextSpan>,
}

impl FormattedText {
    pub fn trimmed(&self) -> Self {
        let text = self.text.trim();
        let start = self.text.len() - self.text.trim_start().len();
        let end = start + text.len();
        let spans = self
            .spans
            .iter()
            .filter_map(|span| {
                let from = span.range.start.max(start);
                let to = span.range.end.min(end);
                (from < to).then(|| TextSpan {
                    range: from - start..to - start,
                    style: span.style,
                })
            })
            .collect();
        Self {
            text: text.to_owned(),
            spans,
        }
    }
}

#[derive(Clone)]
pub struct Message {
    pub id: i32,
    pub sender: String,
    pub sender_avatar: Option<Avatar>,
    pub timestamp: i64,
    pub text: String,
    pub styles: Vec<TextSpan>,
    pub outgoing: bool,
    pub attachment: Option<String>,
    pub photo_preview: bool,
    pub thumbnail: Option<AttachmentThumbnail>,
    pub reply_to: Option<i32>,
    pub reply_preview: Option<ReplyPreview>,
    pub can_reply: bool,
    pub can_edit: bool,
    pub can_delete: bool,
    pub can_forward: bool,
    pub forwarded: bool,
    pub pinned: bool,
    pub reactions: Vec<Reaction>,
    pub edited: bool,
    pub ephemeral: bool,
    pub album: Option<i64>,
}

#[derive(Clone)]
pub struct Reaction {
    pub emoji: String,
    pub count: i32,
    pub chosen: bool,
}

pub const COMMON_REACTIONS: &[&str] = &["👍", "❤️", "🔥", "😂", "👏", "😢"];

#[derive(Clone, PartialEq, Eq)]
pub struct Reply {
    pub id: i32,
    pub preview: ReplyPreview,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ReplyPreview {
    pub sender: Option<String>,
    pub text: String,
}

impl Message {
    pub fn formatted_text(&self) -> FormattedText {
        FormattedText {
            text: self.text.clone(),
            spans: self.styles.clone(),
        }
    }

    pub fn reply_preview(&self) -> ReplyPreview {
        let preview = self.preview_text();
        let text = preview.lines().find(|line| !line.trim().is_empty());
        ReplyPreview {
            sender: Some(self.sender.clone()),
            text: text
                .or(self.attachment.as_deref())
                .unwrap_or("Сообщение")
                .to_owned(),
        }
    }

    pub fn reply_summary(&self) -> String {
        let preview = self.preview_text();
        let text = preview.lines().find(|line| !line.trim().is_empty());
        match text.or(self.attachment.as_deref()) {
            Some(text) => format!("{} · {text}", self.sender),
            None => self.sender.clone(),
        }
    }

    pub fn preview_text(&self) -> Cow<'_, str> {
        redact_spoilers(&self.text, &self.styles)
    }
}

#[derive(Clone, Copy, Default)]
pub struct UpdateStatus {
    pub revision: u64,
    pub folders_revision: u64,
    pub reconnecting: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryRevision {
    pub all: u64,
    pub chat: u64,
    pub rewritten: u64,
}

impl HistoryRevision {
    pub fn is_fresh(self, refreshed: Instant, current: Self, now: Instant) -> bool {
        self == current && now.duration_since(refreshed) < CACHE_FRESHNESS
    }

    pub fn can_keep_older(self, previous: Self) -> bool {
        self.all == previous.all && self.rewritten == previous.rewritten
    }
}

#[derive(Default)]
pub struct HistoryCache {
    chats: HashMap<i64, CachedHistory>,
}

pub struct CachedHistory {
    pub messages: BTreeMap<i32, Message>,
    pub has_older: bool,
    pub revision: HistoryRevision,
    pub refreshed: Instant,
    used: Instant,
}

impl HistoryCache {
    pub fn remove(&mut self, chat: i64) {
        self.chats.remove(&chat);
    }

    pub fn restore(&mut self, chat: i64, now: Instant) -> Option<CachedHistory> {
        self.expire(now);
        self.chats.remove(&chat)
    }

    pub fn store(
        &mut self,
        chat: i64,
        mut messages: BTreeMap<i32, Message>,
        has_older: bool,
        revision: HistoryRevision,
        refreshed: Instant,
        now: Instant,
    ) {
        self.expire(now);
        // Keep a contiguous range. A page with a timer stays in the open chat only.
        let uncached = messages.len().saturating_sub(CACHED_MESSAGES);
        let mut boundary_album = None;
        for _ in 0..uncached {
            boundary_album = messages.pop_first().and_then(|(_, message)| message.album);
        }
        if let Some(album) = boundary_album {
            while messages
                .first_key_value()
                .is_some_and(|(_, message)| message.album == Some(album))
            {
                messages.pop_first();
            }
        }
        if messages.values().any(|message| message.ephemeral) {
            return;
        }
        if self.chats.len() == CACHED_CHATS
            && let Some((&oldest, _)) = self.chats.iter().min_by_key(|(_, history)| history.used)
        {
            self.chats.remove(&oldest);
        }
        self.chats.insert(
            chat,
            CachedHistory {
                messages,
                has_older: has_older || uncached > 0,
                revision,
                refreshed,
                used: now,
            },
        );
    }

    fn expire(&mut self, now: Instant) {
        self.chats
            .retain(|_, history| now.duration_since(history.used) < CACHE_RETENTION);
    }
}

pub struct IncomingMessage {
    pub chat: i64,
    pub message: i32,
    pub title: String,
    pub kind: ChatKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UploadKind {
    File,
    Photo,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Upload {
    pub path: PathBuf,
    pub name: String,
    pub kind: UploadKind,
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

    #[test]
    fn search_pages_keep_cross_chat_ids_and_skip_repeated_results() {
        let chat = crate::demo::chats().remove(0);
        let hit = |chat_id, message_id| SearchHit {
            chat: Chat {
                id: chat_id,
                ..chat.clone()
            },
            message: messages(&[message_id]).remove(0),
        };
        let mut buffer = SearchBuffer::default();
        for id in 1..=SEARCH_PAGE_SIZE as i32 {
            buffer.push(hit(1, id));
        }
        buffer.push(hit(1, 1));
        assert!(buffer.needs_more());
        buffer.push(hit(2, SEARCH_PAGE_SIZE as i32));
        assert!(!buffer.needs_more());
        let first = buffer.take_page();
        assert_eq!(first.hits.len(), SEARCH_PAGE_SIZE);
        assert!(first.has_more);
        // A repeat of a previously delivered result must not reappear on the next page.
        buffer.push(hit(1, 1));
        buffer.push(hit(2, SEARCH_PAGE_SIZE as i32));
        buffer.exhausted = true;
        let last = buffer.take_page();
        assert_eq!(last.hits.len(), 1);
        assert_eq!(last.hits[0].chat.id, 2);
        assert!(!last.has_more);
    }

    #[test]
    fn exact_search_page_does_not_offer_an_empty_next_page() {
        let chat = crate::demo::chats().remove(0);
        let mut buffer = SearchBuffer::default();
        for message in messages(&(1..=SEARCH_PAGE_SIZE as i32).collect::<Vec<_>>()) {
            buffer.push(SearchHit {
                chat: chat.clone(),
                message,
            });
        }
        buffer.exhausted = true;
        let page = buffer.take_page();
        assert_eq!(page.hits.len(), SEARCH_PAGE_SIZE);
        assert!(!page.has_more);
    }

    #[test]
    fn explicit_folder_members_override_categories_and_archive_filter() {
        let chat = Chat {
            id: 1,
            title: "Тест".into(),
            preview: String::new(),
            preview_sender: None,
            preview_image: None,
            group: true,
            unread: 0,
            broadcast: false,
            saved_messages: false,
            archived: true,
            kind: ChatKind::Group,
            muted: false,
            avatar: None,
        };
        let mut folder = ChatFolder {
            kinds: vec![ChatKind::Group],
            exclude_archived: true,
            ..ChatFolder::default()
        };
        assert!(!folder.contains(&chat));
        folder.pinned.push(chat.id);
        assert!(folder.contains(&chat));
        folder.excluded.push(chat.id);
        assert!(!folder.contains(&chat));
    }

    fn messages(ids: &[i32]) -> Vec<Message> {
        ids.iter()
            .map(|&id| Message {
                id,
                sender: "Тест".into(),
                sender_avatar: None,
                timestamp: 0,
                text: String::new(),
                styles: Vec::new(),
                outgoing: false,
                attachment: None,
                photo_preview: false,
                thumbnail: None,
                reply_to: None,
                reply_preview: None,
                can_reply: true,
                can_edit: false,
                can_delete: false,
                can_forward: false,
                forwarded: false,
                pinned: false,
                reactions: Vec::new(),
                edited: false,
                ephemeral: false,
                album: None,
            })
            .collect()
    }

    fn history(ids: &[i32]) -> BTreeMap<i32, Message> {
        messages(ids).into_iter().map(|msg| (msg.id, msg)).collect()
    }

    #[test]
    fn cached_history_preserves_the_latest_contiguous_range_and_expires() {
        let now = Instant::now();
        let revision = HistoryRevision::default();
        let mut cache = HistoryCache::default();
        let ids: Vec<_> = (1..=450).collect();
        cache.store(1, history(&ids), false, revision, now, now);
        let cached = cache.restore(1, now + Duration::from_hours(5)).unwrap();
        assert_eq!(cached.messages.len(), CACHED_MESSAGES);
        assert_eq!(cached.messages.first_key_value().unwrap().0, &51);
        assert!(cached.has_older);
        assert!(!cached.revision.is_fresh(
            cached.refreshed,
            revision,
            now + Duration::from_hours(1)
        ));
        cache.store(1, cached.messages, true, revision, now, now);
        assert!(cache.restore(1, now + CACHE_RETENTION).is_none());
    }

    #[test]
    fn cache_evicts_oldest_chat_and_excludes_timed_messages() {
        let now = Instant::now();
        let revision = HistoryRevision::default();
        let mut cache = HistoryCache::default();
        for chat in 0..=CACHED_CHATS {
            let used = now + Duration::from_secs(chat as u64);
            cache.store(chat as i64, history(&[1]), false, revision, now, used);
        }
        let now = now + Duration::from_mins(5);
        assert!(cache.restore(0, now).is_none());
        assert!(cache.restore(1, now).is_some());
        let mut timed = history(&[1, 2, 3]);
        timed.get_mut(&2).unwrap().ephemeral = true;
        cache.store(1, timed, false, revision, now, now);
        assert!(cache.restore(1, now).is_none());
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
