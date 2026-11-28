use std::collections::HashSet;
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use grammers_client::tl::{
    enums,
    functions::messages::{GetStickerSet, InstallStickerSet, UninstallStickerSet},
    types,
};

use super::{Telegram, reconcile_sticker_membership};
use crate::model::{StickerCollection, StickerPackChange, StickerSource, sticker_pack_name};

impl Telegram {
    pub async fn open_sticker_pack(&self, input: &str) -> Result<StickerCollection> {
        let name = sticker_pack_name(input)?;
        self.sticker_pack_snapshot(types::InputStickerSetShortName { short_name: name }.into())
            .await
    }

    async fn sticker_pack_snapshot(
        &self,
        set: enums::InputStickerSet,
    ) -> Result<StickerCollection> {
        let enums::messages::StickerSet::Set(result) = tokio::time::timeout(
            Duration::from_secs(8),
            self.client.invoke(&GetStickerSet {
                stickerset: set,
                hash: 0,
            }),
        )
        .await
        .context("Telegram не прислал набор вовремя")??
        else {
            bail!("Набор недоступен");
        };
        let enums::StickerSet::Set(set) = result.set;
        let collection = pack_collection(&set)?;
        self.stickers.lock().unwrap().sets.insert(
            set.id,
            types::InputStickerSetId {
                id: set.id,
                access_hash: set.access_hash,
            }
            .into(),
        );
        Ok(collection)
    }

    pub async fn set_sticker_pack_installed(
        &self,
        id: i64,
        installed: bool,
    ) -> Result<StickerPackChange> {
        let _serial = self.sticker_settings.lock().await;
        let set = self
            .stickers
            .lock()
            .unwrap()
            .sets
            .get(&id)
            .cloned()
            .context("Набор недоступен. Откройте его заново")?;
        let latest = StdMutex::new(None);
        let archived = AtomicUsize::new(usize::MAX);
        let read = || async {
            let collection = self.sticker_pack_snapshot(set.clone()).await?;
            ensure!(
                collection.source == StickerSource::Set(id),
                "Набор изменился во время запроса"
            );
            let membership = if collection.installed == Some(true) {
                HashSet::from([id])
            } else {
                HashSet::new()
            };
            latest.lock().unwrap().replace(collection);
            Ok(membership)
        };
        let write = async {
            if installed {
                let result = tokio::time::timeout(
                    Duration::from_secs(12),
                    self.client.invoke(&InstallStickerSet {
                        stickerset: set.clone(),
                        archived: false,
                    }),
                )
                .await
                .context("Результат установки неизвестен. Откройте набор заново")??;
                let count = match result {
                    enums::messages::StickerSetInstallResult::Success => 0,
                    enums::messages::StickerSetInstallResult::Archive(result) => result.sets.len(),
                };
                archived.store(count, Ordering::Relaxed);
            } else {
                tokio::time::timeout(
                    Duration::from_secs(12),
                    self.client.invoke(&UninstallStickerSet {
                        stickerset: set.clone(),
                    }),
                )
                .await
                .context("Результат удаления неизвестен. Откройте набор заново")??;
                archived.store(0, Ordering::Relaxed);
            }
            Ok(())
        };
        reconcile_sticker_membership(id, installed, read, write).await?;
        let collection = latest
            .into_inner()
            .unwrap()
            .context("Не удалось проверить набор")?;
        let count = archived.load(Ordering::Relaxed);
        Ok(StickerPackChange {
            collection,
            archived: (count != usize::MAX).then_some(count),
        })
    }
}

fn pack_collection(set: &types::StickerSet) -> Result<StickerCollection> {
    ensure!(
        !set.masks && !set.emojis,
        "Это набор масок или пользовательских эмодзи"
    );
    Ok(StickerCollection {
        source: StickerSource::Set(set.id),
        title: set.title.clone(),
        installed: Some(set.installed_date.is_some() && !set.archived),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_membership_distinguishes_archived_sets_and_refuses_masks_and_emoji() {
        let mut set = types::StickerSet {
            archived: false,
            official: false,
            masks: false,
            emojis: false,
            text_color: false,
            channel_emoji_status: false,
            creator: false,
            installed_date: Some(1),
            id: 17,
            access_hash: 0,
            title: "Демонстрационный набор".into(),
            short_name: "DemoPack".into(),
            thumbs: None,
            thumb_dc_id: None,
            thumb_version: None,
            thumb_document_id: None,
            count: 2,
            hash: 0,
        };
        assert_eq!(pack_collection(&set).unwrap().installed, Some(true));
        set.archived = true;
        assert_eq!(pack_collection(&set).unwrap().installed, Some(false));
        set.archived = false;
        set.installed_date = None;
        assert_eq!(pack_collection(&set).unwrap().installed, Some(false));
        set.masks = true;
        assert!(pack_collection(&set).is_err());
        set.masks = false;
        set.emojis = true;
        assert!(pack_collection(&set).is_err());
    }
}
