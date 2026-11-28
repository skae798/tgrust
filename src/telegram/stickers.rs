mod packs;

use std::collections::HashMap;
use std::future::Future;
use std::io::Cursor;

use anyhow::{Context, Result, bail, ensure};
use grammers_client::media::{Document, Sticker as TelegramSticker};
use grammers_client::tl::functions::messages::{
    FaveSticker, GetAllStickers, GetFavedStickers, GetRecentStickers, GetStickerSet, GetStickers,
    SaveRecentSticker,
};

use super::*;
use crate::model::{Sticker, StickerCollection, StickerKind, StickerSource};

#[derive(Default)]
pub(super) struct StickerCache {
    favorites: HashSet<i64>,
    sets: HashMap<i64, enums::InputStickerSet>,
    documents: HashMap<i64, CachedSticker>,
}

#[derive(Clone)]
struct CachedSticker {
    document: Document,
    origin: Origin,
}

#[derive(Clone)]
enum Origin {
    Collection(StickerSource),
    Search(String),
    Message { chat: i64, message: i32 },
}

impl Telegram {
    pub async fn sticker_tgs_preview(&self, chat: i64, message: i32) -> Result<Vec<u8>> {
        let peer = self.peer(chat).await?;
        let fresh = || async {
            let Media::Sticker(sticker) = self.attachment_media(peer, message).await? else {
                bail!("Стикер в сообщении изменился");
            };
            ensure!(
                sticker_view(&sticker.document)
                    .is_some_and(|view| view.kind == StickerKind::Animated)
                    && sticker
                        .document
                        .size()
                        .is_some_and(|size| (1..=64 * 1024).contains(&size)),
                "TGS в сообщении недоступен или превышает 64 КиБ"
            );
            Ok(sticker.document)
        };
        let document = fresh().await?;
        let expected = document.size();
        with_refreshed_sticker(
            document,
            |document| async move {
                ensure!(document.size() == expected, "Размер TGS изменился");
                let mut bytes = Vec::new();
                let mut download = self.client.iter_download(&document);
                while let Some(chunk) = download.next().await? {
                    ensure!(
                        bytes.len() + chunk.len() <= 64 * 1024,
                        "TGS превышает 64 КиБ"
                    );
                    bytes.extend_from_slice(&chunk);
                }
                ensure!(Some(bytes.len()) == expected, "TGS загружен не полностью");
                Ok(bytes)
            },
            fresh,
        )
        .await
    }

    pub async fn sticker_collections(&self) -> Result<Vec<StickerCollection>> {
        self.sticker_list(StickerSource::Favorites).await?;
        let mut collections = vec![
            StickerCollection {
                source: StickerSource::Recent,
                title: "Недавние".into(),
                installed: None,
            },
            StickerCollection {
                source: StickerSource::Favorites,
                title: "Избранные".into(),
                installed: None,
            },
        ];
        let enums::messages::AllStickers::Stickers(result) =
            self.client.invoke(&GetAllStickers { hash: 0 }).await?
        else {
            bail!("Telegram не прислал список наборов");
        };
        let mut cache = self.stickers.lock().unwrap();
        cache.sets.clear();
        for enums::StickerSet::Set(set) in result.sets {
            if set.archived || set.masks || set.emojis {
                continue;
            }
            cache.sets.insert(
                set.id,
                types::InputStickerSetId {
                    id: set.id,
                    access_hash: set.access_hash,
                }
                .into(),
            );
            collections.push(StickerCollection {
                source: StickerSource::Set(set.id),
                title: set.title,
                installed: Some(true),
            });
        }
        Ok(collections)
    }

    pub async fn sticker_list(&self, source: StickerSource) -> Result<Vec<Sticker>> {
        let _favorites = if source == StickerSource::Favorites {
            Some(self.sticker_settings.lock().await)
        } else {
            None
        };
        let documents = match source {
            StickerSource::Recent => {
                let enums::messages::RecentStickers::Stickers(result) = self
                    .client
                    .invoke(&GetRecentStickers {
                        attached: false,
                        hash: 0,
                    })
                    .await?
                else {
                    bail!("Telegram не прислал недавние стикеры");
                };
                result.stickers
            }
            StickerSource::Favorites => self.favorite_documents().await?,
            StickerSource::Set(id) => {
                let set = self
                    .stickers
                    .lock()
                    .unwrap()
                    .sets
                    .get(&id)
                    .cloned()
                    .context("Набор больше недоступен. Откройте выбор стикеров заново")?;
                let enums::messages::StickerSet::Set(result) = self
                    .client
                    .invoke(&GetStickerSet {
                        stickerset: set,
                        hash: 0,
                    })
                    .await?
                else {
                    bail!("Telegram не прислал стикеры набора");
                };
                result.documents
            }
        };
        Ok(self.remember_stickers(documents, Origin::Collection(source)))
    }

    pub async fn search_stickers(&self, input: &str) -> Result<Vec<Sticker>> {
        let query = crate::model::sticker_emoji_query(input)?.context("Введите эмодзи")?;
        let enums::messages::Stickers::Stickers(result) = self
            .client
            .invoke(&GetStickers {
                emoticon: query.clone(),
                hash: 0,
            })
            .await?
        else {
            bail!("Telegram не прислал результаты поиска стикеров")
        };
        Ok(self.remember_stickers(result.stickers, Origin::Search(query)))
    }

    fn remember_stickers(&self, documents: Vec<enums::Document>, origin: Origin) -> Vec<Sticker> {
        let mut cache = self.stickers.lock().unwrap();
        // References contain no image data. Bound the account-local cache nevertheless.
        if cache.documents.len().saturating_add(documents.len()) > 4096 {
            cache.documents.clear();
        }
        if matches!(&origin, Origin::Collection(StickerSource::Favorites)) {
            cache.favorites = favorite_ids(&documents);
        }
        let mut seen = HashSet::new();
        let mut stickers = Vec::new();
        for raw in documents {
            let document = document_media(raw);
            let Some(mut sticker) = sticker_view(&document) else {
                continue;
            };
            sticker.favorite = cache.favorites.contains(&sticker.id);
            if !seen.insert(sticker.id) {
                continue;
            }
            cache.documents.insert(
                sticker.id,
                CachedSticker {
                    document,
                    origin: origin.clone(),
                },
            );
            stickers.push(sticker);
        }
        stickers
    }

    async fn favorite_documents(&self) -> Result<Vec<enums::Document>> {
        let enums::messages::FavedStickers::Stickers(result) = tokio::time::timeout(
            Duration::from_secs(8),
            self.client.invoke(&GetFavedStickers { hash: 0 }),
        )
        .await
        .context("Telegram не прислал избранные стикеры вовремя")??
        else {
            bail!("Telegram не прислал избранные стикеры");
        };
        Ok(result.stickers)
    }

    pub async fn set_sticker_favorite(&self, id: i64, favorite: bool) -> Result<()> {
        let cached = self.cached_sticker(id)?;
        with_refreshed_sticker(
            cached.document,
            |document| self.change_sticker_favorite(document, favorite),
            || self.refresh_sticker(id, cached.origin.clone()),
        )
        .await
    }

    async fn change_sticker_favorite(&self, document: Document, favorite: bool) -> Result<()> {
        let _serial = self.sticker_settings.lock().await;
        let read = || async {
            self.favorite_documents()
                .await
                .map(|documents| favorite_ids(&documents))
        };
        let write = async {
            tokio::time::timeout(
                Duration::from_secs(12),
                self.client.invoke(&FaveSticker { id: document.to_raw_input_document(), unfave: !favorite }),
            ).await.context("Telegram не подтвердил изменение избранного вовремя. Обновите набор перед повтором")??;
            Ok(())
        };
        let ids = reconcile_sticker_membership(document.id(), favorite, read, write).await?;
        self.stickers.lock().unwrap().favorites = ids;
        Ok(())
    }

    pub(super) fn remember_sticker_message(&self, message: &TelegramMessage) {
        if message_sticker(message).is_none() {
            return;
        }
        let (Some(chat), Some(Media::Sticker(sticker))) =
            (message.peer_id().bot_api_dialog_id(), message.media())
        else {
            return;
        };
        let mut cache = self.stickers.lock().unwrap();
        if cache.documents.len() >= 4096 && !cache.documents.contains_key(&sticker.document.id()) {
            cache.documents.clear();
        }
        cache.documents.insert(
            sticker.document.id(),
            CachedSticker {
                document: sticker.document,
                origin: Origin::Message {
                    chat,
                    message: message.id(),
                },
            },
        );
    }

    fn cached_sticker(&self, id: i64) -> Result<CachedSticker> {
        self.stickers
            .lock()
            .unwrap()
            .documents
            .get(&id)
            .cloned()
            .context("Стикер больше недоступен. Откройте набор заново")
    }

    async fn refresh_sticker(&self, id: i64, origin: Origin) -> Result<Document> {
        match origin {
            Origin::Collection(source) => {
                ensure!(
                    self.sticker_list(source)
                        .await?
                        .iter()
                        .any(|sticker| sticker.id == id),
                    "Стикер больше недоступен в наборе"
                );
            }
            Origin::Search(query) => {
                ensure!(
                    self.search_stickers(&query)
                        .await?
                        .iter()
                        .any(|sticker| sticker.id == id),
                    "Стикер больше недоступен в результатах поиска"
                );
            }
            Origin::Message { chat, message } => {
                let message = self
                    .message_by_id(self.peer(chat).await?, message)
                    .await?
                    .context("Сообщение больше недоступно")?;
                ensure!(
                    message_sticker(&message).is_some_and(|sticker| sticker.id == id),
                    "Стикер в сообщении изменился"
                );
                self.remember_sticker_message(&message);
            }
        }
        Ok(self.cached_sticker(id)?.document)
    }

    pub(super) async fn sticker_image(&self, id: i64) -> Result<Vec<u8>> {
        let cached = self.cached_sticker(id)?;
        let bytes = with_refreshed_sticker(
            cached.document,
            |document| async move { self.sticker_image_bytes(&document).await },
            || self.refresh_sticker(id, cached.origin.clone()),
        )
        .await?;
        tokio::task::spawn_blocking(move || decode_sticker_image(&bytes)).await?
    }

    async fn sticker_image_bytes(&self, document: &Document) -> Result<Vec<u8>> {
        let sticker = sticker_view(document).context("Это не стикер")?;
        if sticker.kind == StickerKind::Static {
            ensure!(
                document.size().is_some_and(|size| size <= 512 * 1024),
                "Стикер слишком большой"
            );
            self.load_image(document).await
        } else {
            let thumbnail =
                pick_thumbnail(document.thumbs()).context("У стикера нет доступной миниатюры")?;
            self.load_image(&PhotoPreview {
                thumbnail,
                dc: document.dc_id(),
            })
            .await
        }
    }

    pub async fn send_sticker(
        &self,
        target: crate::model::Conversation,
        id: i64,
        reply_to: Option<i32>,
        random_id: i64,
    ) -> Result<Message> {
        let peer = self.peer(target.chat).await?;
        let own = self.self_id().await?;
        let profile = self.profile().await;
        let cached = self.cached_sticker(id)?;
        let sent = with_refreshed_sticker(
            cached.document.clone(),
            |document| async move {
                let input = InputMessage::new()
                    .random_id(random_id)
                    .media(document.to_raw_input_media())
                    .reply_to(reply_to)
                    .forum_topic(target.topic);
                self.client
                    .send_message(peer, input)
                    .await
                    .map_err(Into::into)
            },
            || self.refresh_sticker(id, cached.origin.clone()),
        )
        .await?;
        // Saving recency is best effort: its failure must not turn a delivered sticker into a retry.
        let document = self.cached_sticker(id).unwrap_or(cached).document;
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            self.client.invoke(&SaveRecentSticker {
                attached: false,
                id: document.to_raw_input_document(),
                unsave: false,
            }),
        )
        .await;
        Ok(self.message_view(&sent, own, profile.as_deref()))
    }
}

pub(super) fn message_sticker(message: &TelegramMessage) -> Option<Sticker> {
    if !matches!(&message.raw, enums::Message::Message(raw) if raw.ttl_period.is_none()) {
        return None;
    }
    let Media::Sticker(sticker) = message.media()? else {
        return None;
    };
    sticker_view(&sticker.document)
}

pub(super) fn video_playback(sticker: &TelegramSticker) -> Option<crate::model::PlayableMedia> {
    let document = &sticker.document;
    if sticker_view(document)?.kind != StickerKind::Video {
        return None;
    }
    let size = document.size()? as u64;
    if !(1..=256 * 1024).contains(&size) {
        return None;
    }
    let Some(enums::Document::Document(raw)) = &document.raw.document else {
        return None;
    };
    raw.attributes.iter().find_map(|attribute| {
        let enums::DocumentAttribute::Video(video) = attribute else {
            return None;
        };
        ((1..=512).contains(&video.w) && (1..=512).contains(&video.h)).then_some(
            crate::model::PlayableMedia::Video {
                width: video.w,
                height: video.h,
                size,
                duration: (video.duration.is_finite()
                    && video.duration > 0.0
                    && video.duration <= 4.0)
                    .then(|| video.duration.ceil() as u32),
            },
        )
    })
}

fn sticker_view(document: &Document) -> Option<Sticker> {
    if document.raw.ttl_seconds.is_some() || document.is_spoiler() {
        return None;
    }
    let sticker = TelegramSticker::from_document(document)?;
    if sticker.raw_attrs.mask {
        return None;
    }
    let kind = match document.mime_type()? {
        "image/webp" => StickerKind::Static,
        "application/x-tgsticker" => StickerKind::Animated,
        "video/webm" => StickerKind::Video,
        _ => return None,
    };
    Some(Sticker {
        favorite: false,
        id: document.id(),
        emoji: sticker.emoji().to_owned(),
        kind,
    })
}

async fn reconcile_sticker_membership<R, F>(
    id: i64,
    desired: bool,
    mut read: R,
    write: impl Future<Output = Result<()>>,
) -> Result<HashSet<i64>>
where
    R: FnMut() -> F,
    F: Future<Output = Result<HashSet<i64>>>,
{
    let before = read().await?;
    if before.contains(&id) == desired {
        return Ok(before);
    }
    let result = write.await;
    // Even a failed response may follow a committed write. Read before offering a retry.
    let readback = read().await;
    if let Ok(ids) = &readback
        && ids.contains(&id) == desired
    {
        return readback;
    }
    result?;
    readback?;
    bail!("Telegram не подтвердил изменение списка стикеров. Обновите набор перед повтором")
}

fn favorite_ids(documents: &[enums::Document]) -> HashSet<i64> {
    documents
        .iter()
        .filter_map(|raw| sticker_view(&document_media(raw.clone())).map(|sticker| sticker.id))
        .collect()
}

fn document_media(document: enums::Document) -> Document {
    Document::from_raw_media(types::MessageMediaDocument {
        nopremium: false,
        spoiler: false,
        video: false,
        round: false,
        voice: false,
        document: Some(document),
        alt_documents: None,
        video_cover: None,
        video_timestamp: None,
        ttl_seconds: None,
    })
}

// Both image downloads and sends may retry only an explicit stale-reference rejection.
async fn with_refreshed_sticker<T, O, F, R, G>(
    document: Document,
    operation: O,
    refresh: R,
) -> Result<T>
where
    O: Fn(Document) -> F,
    F: Future<Output = Result<T>>,
    R: FnOnce() -> G,
    G: Future<Output = Result<Document>>,
{
    let id = document.id();
    match operation(document).await {
        Err(error)
            if error
                .downcast_ref::<InvocationError>()
                .is_some_and(expired_file_reference) =>
        {
            let document = refresh().await?;
            ensure!(
                document.id() == id && sticker_view(&document).is_some(),
                "Стикер изменился при обновлении ссылки"
            );
            operation(document).await
        }
        result => result,
    }
}

fn decode_sticker_image(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(512);
    limits.max_image_height = Some(512);
    limits.max_alloc = Some(4 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .context("Не удалось прочитать изображение стикера")?;
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, image::ImageFormat::Png)?;
    Ok(output.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(mime: &str) -> Document {
        document_media(
            types::Document {
                id: 17,
                access_hash: 0,
                file_reference: vec![],
                date: 0,
                mime_type: mime.into(),
                size: 100,
                thumbs: None,
                video_thumbs: None,
                dc_id: 1,
                attributes: vec![
                    types::DocumentAttributeSticker {
                        mask: false,
                        alt: "🐱".into(),
                        stickerset: enums::InputStickerSet::Empty,
                        mask_coords: None,
                    }
                    .into(),
                ],
            }
            .into(),
        )
    }

    #[test]
    fn video_sticker_playback_requires_safe_size_dimensions_and_non_timed_media() {
        let mut document = sample("video/webm");
        if let Some(enums::Document::Document(raw)) = &mut document.raw.document {
            raw.attributes.push(
                types::DocumentAttributeVideo {
                    round_message: false,
                    supports_streaming: true,
                    nosound: true,
                    duration: 2.0,
                    w: 512,
                    h: 512,
                    preload_prefix_size: None,
                    video_start_ts: None,
                    video_codec: Some("vp9".into()),
                }
                .into(),
            );
        }
        let playable = |document: &Document| {
            video_playback(&TelegramSticker::from_document(document).unwrap())
        };
        assert_eq!(
            playable(&document),
            Some(crate::model::PlayableMedia::Video {
                width: 512,
                height: 512,
                duration: Some(2),
                size: 100
            })
        );
        assert!(
            super::super::playable_media(&Media::Sticker(
                TelegramSticker::from_document(&document).unwrap()
            ))
            .is_some()
        );
        let mut timed = document.clone();
        timed.raw.ttl_seconds = Some(1);
        assert!(playable(&timed).is_none());
        timed.raw.ttl_seconds = None;
        timed.raw.spoiler = true;
        assert!(playable(&timed).is_none());
        let mut oversized = document.clone();
        if let Some(enums::Document::Document(raw)) = &mut oversized.raw.document {
            raw.size = 256 * 1024 + 1;
        }
        assert!(playable(&oversized).is_none());
        if let Some(enums::Document::Document(raw)) = &mut oversized.raw.document {
            raw.size = 0;
        }
        assert!(playable(&oversized).is_none());
        let mut dimensions = document.clone();
        if let Some(enums::Document::Document(raw)) = &mut dimensions.raw.document
            && let enums::DocumentAttribute::Video(video) = &mut raw.attributes[1]
        {
            video.w = 513;
        }
        assert!(playable(&dimensions).is_none());
        for mime in ["image/webp", "application/x-tgsticker"] {
            let mut other = document.clone();
            if let Some(enums::Document::Document(raw)) = &mut other.raw.document {
                raw.mime_type = mime.into();
            }
            assert!(playable(&other).is_none());
        }
    }

    #[test]
    fn stickers_require_the_attribute_and_a_supported_format_and_exclude_ttl() {
        for (mime, kind) in [
            ("image/webp", StickerKind::Static),
            ("application/x-tgsticker", StickerKind::Animated),
            ("video/webm", StickerKind::Video),
        ] {
            let mut document = sample(mime);
            let sticker = sticker_view(&document).unwrap();
            assert_eq!(sticker.id, 17);
            assert_eq!(sticker.emoji, "🐱");
            assert_eq!(sticker.kind, kind);
            document.raw.ttl_seconds = Some(1);
            assert!(sticker_view(&document).is_none());
            document.raw.ttl_seconds = None;
            document.raw.spoiler = true;
            assert!(sticker_view(&document).is_none());
        }
        let mut document = sample("image/webp");
        let Some(enums::Document::Document(raw)) = &mut document.raw.document else {
            panic!()
        };
        raw.attributes.clear();
        assert!(sticker_view(&document).is_none());
        assert!(sticker_view(&sample("image/png")).is_none());
        let mut document = sample("image/webp");
        let Some(enums::Document::Document(raw)) = &mut document.raw.document else {
            panic!()
        };
        let enums::DocumentAttribute::Sticker(attribute) = &mut raw.attributes[0] else {
            panic!()
        };
        attribute.mask = true;
        assert!(sticker_view(&document).is_none());
    }

    #[test]
    fn stale_sticker_reference_is_refreshed_once_and_other_failures_are_not_retried() {
        use std::cell::Cell;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let error = |name: &str| {
                anyhow::Error::from(InvocationError::Rpc(grammers_client::sender::RpcError {
                    code: 400,
                    name: name.into(),
                    value: None,
                    caused_by: None,
                }))
            };
            for stale in ["FILE_REFERENCE_EXPIRED", "FILE_REFERENCE_INVALID"] {
                let attempts = Cell::new(0);
                let refreshes = Cell::new(0);
                let result = with_refreshed_sticker(
                    sample("image/webp"),
                    |_| {
                        let attempt = attempts.get();
                        attempts.set(attempt + 1);
                        let result = if attempt == 0 {
                            Err(error(stale))
                        } else {
                            Ok(17)
                        };
                        async move { result }
                    },
                    || async {
                        refreshes.set(refreshes.get() + 1);
                        Ok(sample("image/webp"))
                    },
                )
                .await;
                assert_eq!(result.unwrap(), 17);
                assert_eq!((attempts.get(), refreshes.get()), (2, 1));
            }
            let attempts = Cell::new(0);
            let result: Result<()> = with_refreshed_sticker(
                sample("image/webp"),
                |_| {
                    attempts.set(attempts.get() + 1);
                    let failure = error("FILE_REFERENCE_EXPIRED");
                    async move { Err(failure) }
                },
                || async { Ok(sample("image/webp")) },
            )
            .await;
            assert!(result.is_err());
            assert_eq!(attempts.get(), 2);
            let refreshed = Cell::new(false);
            let result: Result<()> = with_refreshed_sticker(
                sample("image/webp"),
                |_| {
                    let failure = error("CHAT_WRITE_FORBIDDEN");
                    async move { Err(failure) }
                },
                || async {
                    refreshed.set(true);
                    Ok(sample("image/webp"))
                },
            )
            .await;
            assert!(result.is_err());
            assert!(!refreshed.get());
            let attempts = Cell::new(0);
            let result: Result<()> = with_refreshed_sticker(
                sample("image/webp"),
                |_| {
                    attempts.set(attempts.get() + 1);
                    let failure = error("FILE_REFERENCE_EXPIRED");
                    async move { Err(failure) }
                },
                || async {
                    let mut document = sample("image/webp");
                    if let Some(enums::Document::Document(raw)) = &mut document.raw.document {
                        raw.id = 99;
                    }
                    Ok(document)
                },
            )
            .await;
            assert!(result.is_err());
            assert_eq!(attempts.get(), 1);
        });
    }

    #[test]
    fn protected_stickers_keep_manual_saving_but_timed_stickers_have_no_preview_or_forward() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let session = Arc::new(SqliteSession::open(":memory:").await.unwrap());
            // No runner is started and no network request is made.
            let pool = SenderPool::new(session, 12345);
            let client = Client::new(pool.handle);
            let peer = PeerRef {
                id: PeerId::user(42).unwrap(),
                auth: Default::default(),
            };
            let document = sample("image/webp");
            let mut message = TelegramMessage::from_raw_short_updates(
                &client,
                types::UpdateShortSentMessage {
                    out: false,
                    id: 8,
                    pts: 1,
                    pts_count: 1,
                    date: 1,
                    media: Some(document.raw.into()),
                    entities: None,
                    ttl_period: None,
                },
                InputMessage::new(),
                peer,
            );
            let view = super::super::message_view(&message, None, None, None, None);
            assert!(view.sticker.is_some());
            assert_eq!(view.attachment.as_deref(), Some("sticker-8.webp"));
            assert_eq!(view.preview_text(), "🐱 Стикер");
            if let enums::Message::Message(raw) = &mut message.raw {
                raw.noforwards = true;
            }
            assert!(attachment(&message).is_some());
            assert!(!can_forward(&message));
            if let enums::Message::Message(raw) = &mut message.raw {
                raw.ttl_period = Some(60);
            }
            assert!(message_sticker(&message).is_none());
            assert!(attachment(&message).is_none());
            assert!(super::super::message_view(&message, None, None, None, None).ephemeral);
            if let enums::Message::Message(raw) = &mut message.raw {
                raw.noforwards = false;
                raw.ttl_period = None;
                if let Some(enums::MessageMedia::Document(media)) = &mut raw.media {
                    media.ttl_seconds = Some(1);
                }
            }
            assert!(message_sticker(&message).is_none());
            assert!(attachment(&message).is_none());
            assert!(!can_forward(&message));
            assert!(super::super::message_view(&message, None, None, None, None).ephemeral);
        });
    }

    #[test]
    fn favorite_writes_reconcile_lost_responses_and_skip_already_confirmed_changes() {
        use std::cell::Cell;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for desired in [false, true] {
                let reads = Cell::new(0);
                let writes = Cell::new(0);
                let result = reconcile_sticker_membership(
                    17,
                    desired,
                    || {
                        let changed = reads.replace(reads.get() + 1) > 0;
                        let ids = if changed == desired {
                            HashSet::from([17])
                        } else {
                            HashSet::new()
                        };
                        async move { Ok(ids) }
                    },
                    async {
                        writes.set(writes.get() + 1);
                        bail!("Synthetic lost write response")
                    },
                )
                .await
                .unwrap();
                assert_eq!(result.contains(&17), desired);
                assert_eq!((reads.get(), writes.get()), (2, 1));
                let result = reconcile_sticker_membership(
                    17,
                    desired,
                    || {
                        let ids = result.clone();
                        async move { Ok(ids) }
                    },
                    async { panic!("An already confirmed favorite must not be written again") },
                )
                .await
                .unwrap();
                assert_eq!(result.contains(&17), desired);
            }
            let result =
                reconcile_sticker_membership(17, true, || async { Ok(HashSet::new()) }, async {
                    Ok(())
                })
                .await;
            assert!(
                result.is_err(),
                "A successful RPC without the requested state is not confirmation"
            );
            let reads = Cell::new(0);
            let result = reconcile_sticker_membership(
                17,
                true,
                || {
                    let first = reads.replace(reads.get() + 1) == 0;
                    async move {
                        if first {
                            Ok(HashSet::new())
                        } else {
                            bail!("Synthetic readback failure")
                        }
                    }
                },
                async { Ok(()) },
            )
            .await;
            assert!(result.is_err());
        });
    }

    fn webp(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut encoded)
            .encode(pixels, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        encoded
    }

    #[test]
    fn sticker_webp_keeps_transparency_and_rejects_oversized_or_corrupt_images() {
        let pixels = [240, 100, 80, 0, 20, 40, 180, 128];
        let encoded = webp(2, 1, &pixels);
        let png = decode_sticker_image(&encoded).unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 1));
        // Fully transparent RGB is not meaningful to WebP, but alpha must survive.
        assert_eq!(decoded.as_raw()[3], 0);
        assert_eq!(&decoded.as_raw()[4..], &pixels[4..]);
        assert!(decode_sticker_image(&webp(513, 1, &vec![0; 513 * 4])).is_err());
        assert!(decode_sticker_image(&encoded[..encoded.len() / 2]).is_err());
        assert!(decode_sticker_image(b"synthetic invalid sticker").is_err());
    }
}
