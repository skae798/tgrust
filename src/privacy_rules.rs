use std::collections::HashMap;

use anyhow::{Result, ensure};
use grammers_client::tl::enums::{InputPrivacyRule as InputRule, PrivacyRule as Rule};
use grammers_client::tl::{Serializable, enums, types};

use crate::model::{PrivacyAudience, PrivacyKey, PrivacySetting};

pub fn key(key: PrivacyKey) -> enums::InputPrivacyKey {
    match key {
        PrivacyKey::PhoneNumber => enums::InputPrivacyKey::PhoneNumber,
        PrivacyKey::AddedByPhone => enums::InputPrivacyKey::AddedByPhone,
        PrivacyKey::LastSeen => enums::InputPrivacyKey::StatusTimestamp,
        PrivacyKey::ProfilePhoto => enums::InputPrivacyKey::ProfilePhoto,
    }
}

fn split(rules: &[Rule]) -> (&[Rule], PrivacyAudience) {
    use PrivacyAudience::{Contacts, Everyone, Nobody};
    use Rule::*;
    match rules {
        [
            prefix @ ..,
            PrivacyValueAllowContacts,
            PrivacyValueDisallowAll,
        ] => (prefix, Contacts),
        [prefix @ .., PrivacyValueAllowContacts] => (prefix, Contacts),
        [prefix @ .., PrivacyValueAllowAll] => (prefix, Everyone),
        [prefix @ .., PrivacyValueDisallowAll] => (prefix, Nobody),
        rules => (rules, Nobody),
    }
}

fn input_users(users: &[enums::User]) -> HashMap<i64, enums::InputUser> {
    users
        .iter()
        .filter_map(|user| {
            let enums::User::User(user) = user else {
                return None;
            };
            if user.is_self {
                return Some((user.id, enums::InputUser::UserSelf));
            }
            if user.min {
                return None;
            }
            let access_hash = user.access_hash?;
            Some((
                user.id,
                types::InputUser {
                    user_id: user.id,
                    access_hash,
                }
                .into(),
            ))
        })
        .collect()
}

fn exception(rule: &Rule, users: &HashMap<i64, enums::InputUser>) -> Option<InputRule> {
    use Rule::*;
    let references = |ids: &[i64]| {
        ids.iter()
            .map(|id| users.get(id).cloned())
            .collect::<Option<Vec<_>>>()
    };
    Some(match rule {
        PrivacyValueAllowUsers(rule) => types::InputPrivacyValueAllowUsers {
            users: references(&rule.users)?,
        }
        .into(),
        PrivacyValueDisallowUsers(rule) => types::InputPrivacyValueDisallowUsers {
            users: references(&rule.users)?,
        }
        .into(),
        PrivacyValueAllowChatParticipants(rule) => types::InputPrivacyValueAllowChatParticipants {
            chats: rule.chats.clone(),
        }
        .into(),
        PrivacyValueDisallowChatParticipants(rule) => {
            types::InputPrivacyValueDisallowChatParticipants {
                chats: rule.chats.clone(),
            }
            .into()
        }
        PrivacyValueAllowPremium => InputRule::InputPrivacyValueAllowPremium,
        PrivacyValueAllowBots => InputRule::InputPrivacyValueAllowBots,
        PrivacyValueDisallowBots => InputRule::InputPrivacyValueDisallowBots,
        _ => return None,
    })
}

pub fn same_exceptions(
    before: &types::account::PrivacyRules,
    after: &types::account::PrivacyRules,
) -> bool {
    split(&before.rules).0 == split(&after.rules).0
}

pub fn current(key: PrivacyKey, raw: &types::account::PrivacyRules) -> PrivacySetting {
    let (prefix, audience) = split(&raw.rules);
    let users = input_users(&raw.users);
    let custom = prefix.iter().any(|rule| {
        matches!(
            rule,
            Rule::PrivacyValueAllowContacts
                | Rule::PrivacyValueAllowAll
                | Rule::PrivacyValueDisallowContacts
                | Rule::PrivacyValueDisallowAll
                | Rule::PrivacyValueAllowCloseFriends
        )
    });
    let audience = (!custom && key.audiences().contains(&audience)).then_some(audience);
    PrivacySetting {
        audience,
        exception_rules: prefix.len(),
        editable: audience.is_some() && prefix.iter().all(|rule| exception(rule, &users).is_some()),
        revision: raw.rules.to_bytes(),
    }
}

/// Compare the rules the user saw and change only the default audience, leaving
/// higher-priority rules in their original order. Never omit an unresolved peer.
pub fn prepare(
    key: PrivacyKey,
    baseline: &PrivacySetting,
    raw: &types::account::PrivacyRules,
    audience: PrivacyAudience,
) -> Result<Option<Vec<InputRule>>> {
    let fresh = current(key, raw);
    if fresh.revision != baseline.revision {
        return Ok(None);
    }
    ensure!(
        fresh.editable,
        "tgrust не может изменить эти правила, сохранив их исключения"
    );
    ensure!(key.audiences().contains(&audience), "Недоступная аудитория");
    let (prefix, _) = split(&raw.rules);
    let users = input_users(&raw.users);
    let mut rules: Vec<_> = prefix
        .iter()
        .map(|rule| exception(rule, &users))
        .collect::<Option<_>>()
        .ok_or_else(|| anyhow::anyhow!("Не удалось восстановить исключения Telegram"))?;
    let base = match audience {
        PrivacyAudience::Everyone => InputRule::InputPrivacyValueAllowAll,
        PrivacyAudience::Contacts => InputRule::InputPrivacyValueAllowContacts,
        PrivacyAudience::Nobody => InputRule::InputPrivacyValueDisallowAll,
    };
    rules.push(base);
    Ok(Some(rules))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(rules: Vec<Rule>) -> types::account::PrivacyRules {
        types::account::PrivacyRules {
            rules,
            users: Vec::new(),
            chats: Vec::new(),
        }
    }

    #[test]
    fn changes_audience_without_reordering_group_and_bot_rules() {
        use InputRule::*;
        use Rule::*;
        let key = PrivacyKey::PhoneNumber;
        let before = raw(vec![
            types::PrivacyValueDisallowChatParticipants {
                chats: vec![41, 23],
            }
            .into(),
            PrivacyValueAllowBots,
            types::PrivacyValueAllowChatParticipants { chats: vec![62] }.into(),
            PrivacyValueAllowContacts,
            PrivacyValueDisallowAll,
        ]);
        let setting = current(key, &before);
        assert_eq!(setting.audience, Some(PrivacyAudience::Contacts));
        assert_eq!(setting.exception_rules, 3);
        assert_eq!(
            prepare(key, &setting, &before, PrivacyAudience::Nobody).unwrap(),
            Some(vec![
                types::InputPrivacyValueDisallowChatParticipants {
                    chats: vec![41, 23]
                }
                .into(),
                InputPrivacyValueAllowBots,
                types::InputPrivacyValueAllowChatParticipants { chats: vec![62] }.into(),
                InputPrivacyValueDisallowAll,
            ])
        );
        let mut after = before.clone();
        after.rules.remove(3);
        assert!(same_exceptions(&before, &after));
        after.rules.remove(1);
        assert!(!same_exceptions(&before, &after));
    }

    #[test]
    fn keeps_user_order_and_access_hashes_and_rejects_unresolved_references() {
        let first = types::InputUser {
            user_id: 21,
            access_hash: 112,
        }
        .into();
        let me = enums::InputUser::UserSelf;
        let users = HashMap::from([(21, first), (32, me)]);
        let resolved = types::PrivacyValueAllowUsers {
            users: vec![32, 21],
        }
        .into();
        assert_eq!(
            exception(&resolved, &users),
            Some(
                types::InputPrivacyValueAllowUsers {
                    users: vec![users[&32].clone(), users[&21].clone()],
                }
                .into()
            )
        );
        let key = PrivacyKey::ProfilePhoto;
        let before = raw(vec![resolved, Rule::PrivacyValueAllowContacts]);
        let setting = current(key, &before);
        assert_eq!(setting.audience, Some(PrivacyAudience::Contacts));
        assert!(!setting.editable);
        assert!(prepare(key, &setting, &before, PrivacyAudience::Everyone).is_err());
    }

    #[test]
    fn rejects_changes_based_on_stale_exceptions() {
        let key = PrivacyKey::LastSeen;
        let snapshot = |id| {
            raw(vec![
                types::PrivacyValueDisallowChatParticipants { chats: vec![id] }.into(),
                Rule::PrivacyValueAllowAll,
            ])
        };
        let before = current(key, &snapshot(41));
        let fresh = snapshot(42);
        assert_eq!(current(key, &fresh).audience, before.audience);
        assert_eq!(
            prepare(key, &before, &fresh, PrivacyAudience::Nobody).unwrap(),
            None
        );
    }

    #[test]
    fn unsupported_audiences_are_never_written() {
        let close_friends = raw(vec![Rule::PrivacyValueAllowCloseFriends]);
        let key = PrivacyKey::ProfilePhoto;
        let setting = current(key, &close_friends);
        assert_eq!(setting.audience, None);
        assert!(!setting.editable);
        assert!(prepare(key, &setting, &close_friends, PrivacyAudience::Nobody).is_err());

        let everyone = raw(vec![Rule::PrivacyValueAllowAll]);
        let key = PrivacyKey::AddedByPhone;
        let setting = current(key, &everyone);
        assert!(setting.editable);
        assert!(prepare(key, &setting, &everyone, PrivacyAudience::Nobody).is_err());
    }
}
