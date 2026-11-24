use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use grammers_client::peer::Peer;
use grammers_client::tl::functions::channels::ToggleSlowMode;
use grammers_client::tl::functions::messages::EditChatDefaultBannedRights;
use tokio::time::timeout;

use super::chat_addresses::channel_addresses;
use super::create_chat::RPC_TIMEOUT;
use super::{
    InvocationError, PeerKind, PeerRef, Telegram, can_moderate_chat, enums,
    membership_banned_rights, types,
};
use crate::model::{
    GroupPermission, GroupPermissionEdit, GroupPermissionSettings, GroupPermissions,
    slow_mode_change,
};

impl Telegram {
    pub async fn edit_group_permissions(
        &self,
        id: i64,
        original: GroupPermissionSettings,
        requested: GroupPermissionEdit,
    ) -> Result<()> {
        let _serial = self.dialog_settings.lock().await;
        let reference = self.peer(id).await?;
        let (current, mut raw) = self.current_group_permissions(reference).await?;
        let changes = current.changes(original, requested.allowed)?;
        let slow_mode =
            slow_mode_change(current.slow_mode, original.slow_mode, requested.slow_mode)?;
        if changes.is_empty() && slow_mode.is_none() {
            return Ok(());
        }

        if !changes.is_empty() {
            apply_changes(&mut raw, &changes);
            raw.until_date = 0;
            let result = timeout(
                RPC_TIMEOUT,
                self.client.invoke(&EditChatDefaultBannedRights {
                    peer: reference.into(),
                    banned_rights: raw.into(),
                }),
            )
            .await;
            // A timed-out request may still have been applied.
            self.invalidate_views();
            permission_result(result).context("Не удалось изменить общие разрешения")?;
        }
        if let Some(seconds) = slow_mode {
            let result = timeout(
                RPC_TIMEOUT,
                self.client.invoke(&ToggleSlowMode {
                    channel: reference.into(),
                    seconds,
                }),
            )
            .await;
            self.invalidate_views();
            permission_result(result).context("Не удалось изменить медленный режим")?;
        }
        let (saved, _) = self.current_group_permissions(reference).await?;
        ensure!(
            saved.changes(original, requested.allowed)?.is_empty()
                && slow_mode_change(saved.slow_mode, original.slow_mode, requested.slow_mode)?
                    .is_none(),
            "Telegram не подтвердил все изменения разрешений. Повторите сохранение"
        );
        Ok(())
    }

    pub(super) async fn current_group_permissions(
        &self,
        reference: PeerRef,
    ) -> Result<(GroupPermissionSettings, types::ChatBannedRights)> {
        let full = timeout(RPC_TIMEOUT, self.full_chat(reference))
            .await
            .context("Telegram не прислал разрешения группы вовремя")??;
        let peer = full
            .chats
            .into_iter()
            .map(|chat| Peer::from_raw(&self.client, chat))
            .find(|peer| peer.id() == reference.id)
            .context("Группа не найдена")?;
        let (mut settings, raw) = group_permissions(&peer)
            .context("Telegram не разрешает менять разрешения этой группы")?;
        if let enums::ChatFull::ChannelFull(full) = full.full_chat {
            settings.slow_mode = Some(full.slowmode_seconds.unwrap_or_default());
        }
        Ok((settings, raw))
    }

    pub async fn slow_mode_remaining(&self, id: i64) -> Result<Duration> {
        let reference = self.peer(id).await?;
        if reference.id.kind() != PeerKind::Channel {
            return Ok(Duration::ZERO);
        }
        let full = timeout(RPC_TIMEOUT, self.full_chat(reference)).await??;
        let enums::ChatFull::ChannelFull(full) = full.full_chat else {
            return Ok(Duration::ZERO);
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let until = full.slowmode_next_send_date.unwrap_or_default();
        let until = Duration::from_secs(u64::try_from(until).unwrap_or_default());
        Ok(until.saturating_sub(now))
    }
}

pub(super) fn permission_result(
    result: Result<Result<enums::Updates, InvocationError>, tokio::time::error::Elapsed>,
) -> Result<()> {
    if let Err(error) = result.context("Telegram не подтвердил сохранение вовремя")?
        && !error.is("CHAT_NOT_MODIFIED")
    {
        return Err(error.into());
    }
    Ok(())
}

pub(super) fn slow_mode_enabled(peer: &Peer) -> bool {
    matches!(peer, Peer::Group(group) if matches!(&group.raw, enums::Chat::Channel(channel)
        if channel.slowmode_enabled && !channel.creator && channel.admin_rights.is_none()))
}

pub fn slow_mode_wait(error: &anyhow::Error) -> Option<Duration> {
    let InvocationError::Rpc(error) = error.downcast_ref::<InvocationError>()? else {
        return None;
    };
    let seconds = error.value.filter(|seconds| *seconds > 0)?;
    error
        .is("SLOWMODE_WAIT")
        .then(|| Duration::from_secs(seconds.into()))
}

pub(super) fn group_permissions(
    peer: &Peer,
) -> Option<(GroupPermissionSettings, types::ChatBannedRights)> {
    let Peer::Group(group) = peer else {
        return None;
    };
    let (raw, creator, admin_rights, forum, public_or_linked) = match &group.raw {
        enums::Chat::Chat(chat)
            if !chat.left && !chat.deactivated && chat.migrated_to.is_none() =>
        {
            (
                chat.default_banned_rights.as_ref(),
                chat.creator,
                chat.admin_rights.as_ref(),
                false,
                false,
            )
        }
        enums::Chat::Channel(channel)
            if channel.megagroup
                && !channel.broadcast
                && !channel.left
                && !channel.min
                && !channel.monoforum =>
        {
            (
                channel.default_banned_rights.as_ref(),
                channel.creator,
                channel.admin_rights.as_ref(),
                channel.forum,
                channel.has_link
                    || channel_addresses(channel.username.as_deref(), channel.usernames.as_deref())
                        .is_public(),
            )
        }
        _ => return None,
    };
    if !can_moderate_chat(creator, admin_rights) {
        return None;
    }
    let raw = raw
        .map(|enums::ChatBannedRights::Rights(raw)| raw.clone())
        .unwrap_or_else(|| membership_banned_rights(false));
    Some((
        settings(&raw, creator, admin_rights, forum, public_or_linked),
        raw,
    ))
}

fn settings(
    raw: &types::ChatBannedRights,
    creator: bool,
    admin_rights: Option<&enums::ChatAdminRights>,
    forum: bool,
    public_or_linked: bool,
) -> GroupPermissionSettings {
    use GroupPermission::*;
    let mut editable = GroupPermissions::all()
        .with(Topics, forum)
        .with(Pin, !public_or_linked)
        .with(ChangeInfo, !public_or_linked);
    if !creator {
        let rights = admin_rights.map(|enums::ChatAdminRights::Rights(rights)| rights);
        editable = editable
            .with(Invite, rights.is_some_and(|rights| rights.invite_users))
            .with(
                Pin,
                !public_or_linked && rights.is_some_and(|rights| rights.pin_messages),
            )
            .with(
                ChangeInfo,
                !public_or_linked && rights.is_some_and(|rights| rights.change_info),
            )
            .with(
                Topics,
                forum && rights.is_some_and(|rights| rights.manage_topics),
            );
    }
    let allowed = allowed_permissions(raw);
    GroupPermissionSettings {
        allowed: allowed
            .with(LinkPreviews, allowed.has(Text) && allowed.has(LinkPreviews))
            .with(Pin, !public_or_linked && allowed.has(Pin))
            .with(ChangeInfo, !public_or_linked && allowed.has(ChangeInfo)),
        editable,
        forum,
        slow_mode: None,
    }
}

pub(super) fn allowed_permissions(raw: &types::ChatBannedRights) -> GroupPermissions {
    use GroupPermission::*;
    let mut granular = raw.clone();
    expand_legacy(&mut granular);
    GroupPermission::OPTIONS.into_iter().fold(
        GroupPermissions::default(),
        |allowed, (permission, _)| {
            let banned = match permission {
                Text => granular.send_plain,
                Photos => granular.send_photos,
                Videos => granular.send_videos,
                VideoNotes => granular.send_roundvideos,
                Music => granular.send_audios,
                Voice => granular.send_voices,
                Documents => granular.send_docs,
                StickersAndBots => {
                    granular.send_stickers
                        || granular.send_gifs
                        || granular.send_games
                        || granular.send_inline
                }
                LinkPreviews => granular.embed_links,
                Polls => granular.send_polls,
                Reactions => granular.send_reactions,
                Invite => granular.invite_users,
                Pin => granular.pin_messages,
                ChangeInfo => granular.change_info,
                Topics => granular.manage_topics,
                EditTag => granular.edit_rank,
            };
            allowed.with(permission, !banned)
        },
    )
}

fn expand_legacy(raw: &mut types::ChatBannedRights) {
    raw.send_plain |= raw.send_messages;
    raw.send_stickers |= raw.send_messages;
    raw.send_gifs |= raw.send_messages;
    raw.send_games |= raw.send_messages;
    raw.send_inline |= raw.send_messages;
    raw.send_polls |= raw.send_messages;
    let media = raw.send_media || raw.send_messages;
    raw.send_photos |= media;
    raw.send_videos |= media;
    raw.send_roundvideos |= media;
    raw.send_audios |= media;
    raw.send_voices |= media;
    raw.send_docs |= media;
    raw.send_messages = false;
    raw.send_media = false;
}

pub(super) fn apply_changes(
    raw: &mut types::ChatBannedRights,
    changes: &[(GroupPermission, bool)],
) {
    use GroupPermission::*;
    if changes
        .iter()
        .any(|(permission, _)| permission.is_sending() && *permission != Reactions)
    {
        expand_legacy(raw);
    }
    for &(permission, allowed) in changes {
        let banned = !allowed;
        match permission {
            Text => {
                raw.send_plain = banned;
                raw.embed_links |= banned;
            }
            Photos => raw.send_photos = banned,
            Videos => raw.send_videos = banned,
            VideoNotes => raw.send_roundvideos = banned,
            Music => raw.send_audios = banned,
            Voice => raw.send_voices = banned,
            Documents => raw.send_docs = banned,
            StickersAndBots => {
                raw.send_stickers = banned;
                raw.send_gifs = banned;
                raw.send_games = banned;
                raw.send_inline = banned;
            }
            LinkPreviews => raw.embed_links = banned,
            Polls => raw.send_polls = banned,
            Reactions => raw.send_reactions = banned,
            Invite => raw.invite_users = banned,
            Pin => raw.pin_messages = banned,
            ChangeInfo => raw.change_info = banned,
            Topics => raw.manage_topics = banned,
            EditTag => raw.edit_rank = banned,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use GroupPermission::*;
    use grammers_client::sender::RpcError;

    #[test]
    fn slow_mode_edits_ignore_unchanged_fields_and_acknowledged_retries() {
        assert_eq!(
            slow_mode_change(Some(30), Some(10), Some(10)).unwrap(),
            None
        );
        assert_eq!(
            slow_mode_change(Some(30), Some(10), Some(30)).unwrap(),
            None
        );
        assert_eq!(
            slow_mode_change(Some(30), Some(10), Some(5)).unwrap(),
            Some(5)
        );
        assert_eq!(
            slow_mode_change(Some(30), Some(10), Some(0)).unwrap(),
            Some(0)
        );
        assert!(slow_mode_change(None, Some(10), Some(0)).is_err());
        assert!(slow_mode_change(Some(10), Some(10), Some(25)).is_err());
    }

    #[test]
    fn only_a_positive_slow_mode_rpc_wait_blocks_the_composer() {
        let make_error = |name: &str, value: Option<u32>| {
            anyhow::Error::from(InvocationError::Rpc(RpcError {
                code: 420,
                name: name.into(),
                value,
                caused_by: None,
            }))
        };
        assert_eq!(
            slow_mode_wait(&make_error("SLOWMODE_WAIT", Some(12))),
            Some(Duration::from_secs(12))
        );
        assert_eq!(slow_mode_wait(&make_error("SLOWMODE_WAIT", Some(0))), None);
        assert_eq!(slow_mode_wait(&make_error("SLOWMODE_WAIT", None)), None);
        assert_eq!(slow_mode_wait(&make_error("FLOOD_WAIT", Some(12))), None);
    }

    #[test]
    fn a_media_edit_preserves_other_legacy_restrictions_and_server_fields() {
        let mut raw = membership_banned_rights(false);
        raw.send_media = true;
        raw.send_gifs = true;
        raw.manage_linked_peers = true;
        let before = settings(&raw, true, None, false, false);
        assert!(!before.allowed.has(Photos));
        assert!(before.allowed.has(Text));
        apply_changes(&mut raw, &[(Photos, true)]);
        let after = settings(&raw, true, None, false, false);
        assert_eq!(after.allowed, before.allowed.with(Photos, true));
        assert!(!raw.send_media);
        assert!(raw.send_gifs);
        assert!(!raw.send_stickers);
        assert!(raw.manage_linked_peers);
    }

    #[test]
    fn restrictions_for_public_and_linked_groups_stay_read_only() {
        let mut raw = membership_banned_rights(false);
        let current = settings(&raw, true, None, false, true);
        for permission in [Pin, ChangeInfo] {
            assert!(!current.editable.has(permission));
            assert!(!current.allowed.has(permission));
            assert!(
                current
                    .changes(current, current.allowed.with(permission, true))
                    .is_err()
            );
        }
        apply_changes(&mut raw, &[(Reactions, false)]);
        assert!(!raw.pin_messages);
        assert!(!raw.change_info);
        assert!(raw.send_reactions);
    }

    #[test]
    fn write_uses_current_rights_and_preserves_changes_from_another_admin() {
        let mut raw = membership_banned_rights(false);
        let original = settings(&raw, true, None, true, false);
        assert!(original.editable.has(Topics));
        raw.send_voices = true;
        let current = settings(&raw, true, None, true, false);
        let requested = original.allowed.with(Text, false).with(Pin, false);
        let changes = current.changes(original, requested).unwrap();
        assert_eq!(
            changes,
            [(Text, false), (LinkPreviews, false), (Pin, false)]
        );
        apply_changes(&mut raw, &changes);
        assert!(raw.send_voices);
        assert!(raw.send_plain && raw.embed_links && raw.pin_messages);
        let saved = settings(&raw, true, None, true, false);
        assert!(saved.changes(original, requested).unwrap().is_empty());
        let demoted = settings(&raw, false, None, true, false);
        assert!(
            demoted
                .changes(original, original.allowed.with(Invite, false))
                .is_err()
        );
        assert!(!demoted.editable.has(Topics));
        assert!(demoted.editable.has(Photos));
    }

    #[test]
    fn enabling_text_preserves_the_selected_link_preview_setting() {
        let mut raw = membership_banned_rights(false);
        raw.send_plain = true;
        let before = settings(&raw, true, None, false, false);
        let requested = before.allowed.with(Text, true);
        apply_changes(&mut raw, &before.changes(before, requested).unwrap());
        assert!(!raw.send_plain && raw.embed_links);
        let after = settings(&raw, true, None, false, false);
        assert!(after.changes(before, requested).unwrap().is_empty());
        let requested = after.allowed.with(LinkPreviews, true);
        let mut concurrent = raw;
        concurrent.send_plain = true;
        assert!(
            settings(&concurrent, true, None, false, false)
                .changes(after, requested)
                .is_err()
        );
    }
}
