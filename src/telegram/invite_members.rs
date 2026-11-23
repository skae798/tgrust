use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use grammers_client::tl::functions::{channels::InviteToChannel, messages::AddChatUser};
use tokio::time::{Instant, timeout, timeout_at};

use super::{InvocationError, PeerKind, Telegram, enums};
use crate::model::{ChatKind, Contact, MemberInvitation, MemberInvitations};

const BUDGET: Duration = Duration::from_secs(25);

impl Telegram {
    pub async fn invite_members(
        &self,
        chat: i64,
        contacts: Vec<Contact>,
    ) -> Result<MemberInvitations> {
        let _serial = self.dialog_settings.lock().await;
        let reference = self.peer(chat).await?;
        let full = timeout(BUDGET, self.full_chat(reference))
            .await
            .context("Telegram не прислал данные чата вовремя")??;
        let profile = self.chat_profile_view(reference, full.chats)?;
        ensure!(
            profile.can_add_members,
            "Telegram не разрешает добавлять участников в этот чат"
        );
        let mut seen = HashSet::new();
        let mut outcomes = Vec::new();
        let mut users = Vec::new();
        for contact in contacts
            .into_iter()
            .filter(|contact| seen.insert(contact.id))
        {
            let position = outcomes.len();
            outcomes.push((contact.name.clone(), MemberInvitation::NotSent));
            let eligible = contact.available
                && !contact.is_self
                && (profile.kind != ChatKind::Broadcast || contact.kind != ChatKind::Bot);
            match self.peer(contact.id).await {
                Ok(peer)
                    if eligible
                        && peer.id.kind() == PeerKind::User
                        && enums::InputPeer::from(peer) != enums::InputPeer::PeerSelf =>
                {
                    users.push((position, contact.id, peer));
                }
                _ => outcomes[position].1 = MemberInvitation::Rejected("Контакт недоступен".into()),
            }
        }

        let deadline = Instant::now() + BUDGET;
        match reference.id.kind() {
            PeerKind::Chat => {
                let enums::ChatFull::Full(full) = full.full_chat else {
                    bail!("Профиль группы изменился");
                };
                let existing = match full.participants {
                    enums::ChatParticipants::Participants(list) => list
                        .participants
                        .iter()
                        .map(|member| member.user_id())
                        .collect(),
                    _ => HashSet::new(),
                };
                for (position, member, peer) in users {
                    if existing.contains(&member) {
                        outcomes[position].1 = MemberInvitation::InChat;
                        continue;
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    let reply = timeout_at(
                        deadline,
                        self.client.invoke(&AddChatUser {
                            chat_id: full.id,
                            user_id: peer.into(),
                            fwd_limit: 0,
                        }),
                    )
                    .await;
                    let (status, stop) = classify_for_user(reply, member);
                    outcomes[position].1 = status;
                    if stop {
                        break;
                    }
                }
            }
            PeerKind::Channel if !users.is_empty() => {
                let reply = timeout_at(
                    deadline,
                    self.client.invoke(&InviteToChannel {
                        channel: reference.into(),
                        users: users.iter().map(|(_, _, peer)| (*peer).into()).collect(),
                    }),
                )
                .await;
                let single = users.len() == 1;
                match reply {
                    Ok(Ok(enums::messages::InvitedUsers::Users(result))) => {
                        for (position, id, _) in users {
                            outcomes[position].1 = invitation_status(&result.missing_invitees, id);
                        }
                    }
                    Ok(Err(error)) => {
                        let status = rejected_invitation(&error, single);
                        for (position, _, _) in users {
                            outcomes[position].1 = status.clone();
                        }
                    }
                    Err(_) => {
                        for (position, _, _) in users {
                            outcomes[position].1 = MemberInvitation::Unconfirmed;
                        }
                    }
                }
            }
            PeerKind::Channel => {}
            PeerKind::User => bail!("У пользователя нет списка участников"),
        }
        self.invalidate_history(Some(chat), false);
        self.invalidate_views();
        Ok(outcomes)
    }
}

fn classify_for_user(
    reply: Result<
        Result<enums::messages::InvitedUsers, InvocationError>,
        tokio::time::error::Elapsed,
    >,
    id: i64,
) -> (MemberInvitation, bool) {
    match reply {
        Ok(Ok(enums::messages::InvitedUsers::Users(result))) => {
            (invitation_status(&result.missing_invitees, id), false)
        }
        Ok(Err(error)) => {
            let status = rejected_invitation(&error, true);
            let stop = status == MemberInvitation::Unconfirmed
                || matches!(&error, InvocationError::Rpc(rpc) if rpc.code == 420)
                || error.is("CHAT_ADMIN_REQUIRED")
                || error.is("CHAT_WRITE_FORBIDDEN")
                || error.is("USERS_TOO_MUCH");
            (status, stop)
        }
        Err(_) => (MemberInvitation::Unconfirmed, true),
    }
}

fn rejected_invitation(error: &InvocationError, single: bool) -> MemberInvitation {
    if error.is("USER_ALREADY_PARTICIPANT") && single {
        MemberInvitation::InChat
    } else if error.is("USER_PRIVACY_RESTRICTED") || error.is("USER_NOT_MUTUAL_CONTACT") {
        MemberInvitation::Rejected("Ограничения приватности".into())
    } else if matches!(error, InvocationError::Rpc(rpc) if (400..500).contains(&rpc.code)) {
        MemberInvitation::Rejected(error.to_string())
    } else {
        MemberInvitation::Unconfirmed
    }
}

fn invitation_status(missing: &[enums::MissingInvitee], id: i64) -> MemberInvitation {
    let result = missing
        .iter()
        .find_map(|enums::MissingInvitee::Invitee(value)| (value.user_id == id).then_some(value));
    match result {
        Some(value) if value.premium_would_allow_invite => {
            MemberInvitation::Rejected("Для приглашения нужен Telegram Premium".into())
        }
        Some(_) => MemberInvitation::Rejected("Ограничения приватности".into()),
        None => MemberInvitation::InChat,
    }
}

pub(super) fn invite_restricted(rights: Option<&enums::ChatBannedRights>) -> bool {
    rights.is_some_and(|enums::ChatBannedRights::Rights(rights)| {
        rights.invite_users || rights.view_messages
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_invitees_only_reject_the_matching_users() {
        let failed = [(42, false), (43, true)].map(|(user_id, premium_would_allow_invite)| {
            super::super::types::MissingInvitee {
                user_id,
                premium_would_allow_invite,
                premium_required_for_pm: false,
            }
            .into()
        });
        assert!(
            matches!(invitation_status(&failed, 42), MemberInvitation::Rejected(reason) if reason.contains("приватности"))
        );
        assert!(
            matches!(invitation_status(&failed, 43), MemberInvitation::Rejected(reason) if reason.contains("Premium"))
        );
        assert!(matches!(
            invitation_status(&failed, 44),
            MemberInvitation::InChat
        ));
    }

    #[test]
    fn uncertain_transport_failure_stops_the_remaining_basic_group_requests() {
        let result = InvocationError::Io(std::io::ErrorKind::ConnectionReset.into());
        let (status, stop) = classify_for_user(Ok(Err(result)), 42);
        assert!(matches!(status, MemberInvitation::Unconfirmed));
        assert!(stop);
    }
}
