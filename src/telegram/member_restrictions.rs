use anyhow::{Context, Result, bail, ensure};
use grammers_client::tl::functions::channels::{EditBanned, GetParticipant};
use tokio::time::timeout;

use super::create_chat::RPC_TIMEOUT;
use super::group_permissions::{allowed_permissions, apply_changes, permission_result};
use super::{PeerKind, PeerRef, Telegram, channel_member, enums, membership_banned_rights, types};
use crate::model::{
    GroupPermission, MemberRestriction, MemberRestrictionEdit, MemberRestrictionSettings,
    restriction_now,
};

impl Telegram {
    pub async fn member_restriction(
        &self,
        chat: i64,
        member: i64,
    ) -> Result<MemberRestrictionSettings> {
        let chat = self.peer(chat).await?;
        let member = self.peer(member).await?;
        self.check_restriction_peer(chat, member).await?;
        self.load_member_restriction(chat, member, false)
            .await
            .map(|(snapshot, _)| snapshot)
    }

    pub async fn edit_member_restriction(
        &self,
        chat: i64,
        member: i64,
        original: MemberRestrictionSettings,
        edit: MemberRestrictionEdit,
    ) -> Result<()> {
        let _serial = self.dialog_settings.lock().await;
        let chat = self.peer(chat).await?;
        let member = self.peer(member).await?;
        self.check_restriction_peer(chat, member).await?;
        let (current, raw) = self
            .load_member_restriction(chat, member, matches!(edit, MemberRestrictionEdit::Clear))
            .await?;
        let Some(next) = current.prepare(original, edit, restriction_now())? else {
            return Ok(());
        };
        let rights = restriction_rights(raw, current.personal, next);
        let result = timeout(
            RPC_TIMEOUT,
            self.client.invoke(&EditBanned {
                channel: chat.into(),
                participant: member.into(),
                banned_rights: rights.into(),
            }),
        )
        .await;
        self.invalidate_views();
        permission_result(result)?;
        let (saved, _) = self
            .load_member_restriction(chat, member, !next.active())
            .await?;
        ensure!(
            saved.personal == next,
            "Telegram не подтвердил ограничения. Перечитайте список участников"
        );
        Ok(())
    }

    async fn check_restriction_peer(&self, chat: PeerRef, member: PeerRef) -> Result<()> {
        ensure!(
            chat.id.kind() == PeerKind::Channel && member.id.kind() == PeerKind::User,
            "Ограничения доступны для участников супергруппы"
        );
        let own = match self.self_id().await? {
            Some(id) => id,
            None => self.profile().await.context("Аккаунт недоступен")?.id(),
        };
        ensure!(member.id != own, "Свой аккаунт ограничить нельзя");
        Ok(())
    }

    async fn load_member_restriction(
        &self,
        chat: PeerRef,
        member: PeerRef,
        allow_missing: bool,
    ) -> Result<(MemberRestrictionSettings, types::ChatBannedRights)> {
        let (group, _) = self.current_group_permissions(chat).await?;
        let result = timeout(
            RPC_TIMEOUT,
            self.client.invoke(&GetParticipant {
                channel: chat.into(),
                participant: member.into(),
            }),
        )
        .await
        .context("Telegram не прислал сведения об участнике вовремя")?;
        let participant = match result {
            Ok(enums::channels::ChannelParticipant::Participant(result)) => {
                Some(result.participant)
            }
            Err(error) if allow_missing && error.is("USER_NOT_PARTICIPANT") => None,
            Err(error) => return Err(error.into()),
        };
        use enums::ChannelParticipant as P;
        if let Some(value) = &participant {
            ensure!(
                channel_member(value).is_some_and(|(id, _)| id == member.id),
                "Telegram вернул другого участника"
            );
        }
        let raw = match participant {
            Some(P::Participant(_)) => membership_banned_rights(false),
            Some(P::Banned(value)) => {
                let enums::ChatBannedRights::Rights(raw) = value.banned_rights;
                ensure!(!raw.view_messages, "Участник заблокирован");
                ensure!(
                    !value.left
                        || allow_missing
                        || personal_restriction(&raw, restriction_now()).active(),
                    "Пользователь больше не состоит в группе"
                );
                raw
            }
            Some(P::Left(_)) | None if allow_missing => membership_banned_rights(false),
            _ => bail!("Роль участника изменилась. Обновите список"),
        };
        let personal = personal_restriction(&raw, restriction_now());
        Ok((MemberRestrictionSettings { group, personal }, raw))
    }
}

pub(super) fn personal_restriction(raw: &types::ChatBannedRights, now: i32) -> MemberRestriction {
    let until = if raw.until_date == i32::MAX
        || i64::from(raw.until_date) - i64::from(now) > 366 * 86_400
    {
        0
    } else {
        raw.until_date
    };
    if until != 0 && until <= now {
        return MemberRestriction::default();
    }
    let personal = MemberRestriction {
        allowed: allowed_permissions(raw),
        other_flags: raw.manage_linked_peers,
        until,
    };
    if personal.active() {
        personal
    } else {
        MemberRestriction::default()
    }
}

fn restriction_rights(
    mut raw: types::ChatBannedRights,
    current: MemberRestriction,
    next: MemberRestriction,
) -> types::ChatBannedRights {
    if !next.active() {
        return membership_banned_rights(false);
    }
    if !current.active() {
        raw = membership_banned_rights(false);
    }
    let changes: Vec<_> = GroupPermission::OPTIONS
        .into_iter()
        .filter_map(|(permission, _)| {
            let enabled = next.allowed.has(permission);
            (enabled != current.allowed.has(permission)).then_some((permission, enabled))
        })
        .collect();
    apply_changes(&mut raw, &changes);
    raw.until_date = next.until;
    raw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GroupPermission::*, GroupPermissions};

    #[test]
    fn member_edits_preserve_inherited_and_unmodified_personal_permissions() {
        let now = 1_700_000_000;
        let original = MemberRestrictionSettings {
            group: crate::demo::group_permissions(
                GroupPermissions::all().with(Documents, false),
                false,
            ),
            personal: MemberRestriction::default(),
        };
        let allowance = original.view();
        assert!(!allowance.editable.has(Documents));
        let mut raw = membership_banned_rights(false);
        raw.send_voices = true;
        raw.manage_linked_peers = true;
        let current = MemberRestrictionSettings {
            personal: personal_restriction(&raw, now),
            ..original
        };
        let edit = MemberRestrictionEdit::Save {
            allowed: allowance.allowed.with(Photos, false),
            until: Some(now + 3_600),
        };
        let next = current.prepare(original, edit, now).unwrap().unwrap();
        let saved = restriction_rights(raw, current.personal, next);
        assert!(saved.send_photos && saved.send_voices && saved.manage_linked_peers);
        assert!(!saved.send_docs && !saved.view_messages);
        assert_eq!(saved.until_date, now + 3_600);
        let saved_view = MemberRestrictionSettings {
            personal: personal_restriction(&saved, now),
            ..original
        };
        assert!(saved_view.prepare(original, edit, now).unwrap().is_none());
        let clear = MemberRestrictionEdit::Clear;
        assert!(saved_view.prepare(current, clear, now).is_err());
        let empty = saved_view.prepare(saved_view, clear, now).unwrap().unwrap();
        assert_eq!(
            restriction_rights(saved, saved_view.personal, empty),
            membership_banned_rights(false)
        );
    }

    #[test]
    fn expiring_restrictions_do_not_silently_become_permanent() {
        let now = 1_700_000_000;
        let group = crate::demo::group_permissions(GroupPermissions::all(), false);
        let mut raw = membership_banned_rights(false);
        raw.send_voices = true;
        raw.until_date = now - 1;
        assert!(!personal_restriction(&raw, now).active());
        raw.until_date = i32::MAX;
        assert_eq!(personal_restriction(&raw, now).until, 0);
        raw.until_date = now + 25;
        let current = MemberRestrictionSettings {
            group,
            personal: personal_restriction(&raw, now),
        };
        let allowed = current.view().allowed.with(Videos, false);
        assert!(
            current
                .prepare(
                    current,
                    MemberRestrictionEdit::Save {
                        allowed,
                        until: None
                    },
                    now
                )
                .is_err()
        );
        assert!(
            current
                .prepare(
                    current,
                    MemberRestrictionEdit::Save {
                        allowed,
                        until: Some(now + 60)
                    },
                    now
                )
                .unwrap()
                .is_some()
        );
    }
}
