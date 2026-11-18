use anyhow::{Result, ensure};

use crate::model::{OwnProfile, ProfileEdit};

#[derive(Debug, PartialEq, Eq)]
pub enum Patch {
    Details {
        first_name: Option<String>,
        last_name: Option<String>,
        about: Option<String>,
    },
    Username(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Current,
    Conflict,
    Write(Patch),
}

pub fn prepare(original: &OwnProfile, fresh: &OwnProfile, edit: &ProfileEdit) -> Result<Plan> {
    ensure!(original.id == fresh.id, "Telegram вернул другой аккаунт");
    match edit {
        ProfileEdit::Details(proposed) => {
            let first = field(
                &original.details.first_name,
                &fresh.details.first_name,
                &proposed.first_name,
            );
            let last = field(
                &original.details.last_name,
                &fresh.details.last_name,
                &proposed.last_name,
            );
            let about = field(
                &original.details.about,
                &fresh.details.about,
                &proposed.about,
            );
            let (Ok(first_name), Ok(last_name), Ok(about)) = (first, last, about) else {
                return Ok(Plan::Conflict);
            };
            if let Some(first) = &first_name {
                ensure!(!first.is_empty(), "Введите имя");
            }
            ensure!(
                [first_name.as_deref(), last_name.as_deref()]
                    .into_iter()
                    .flatten()
                    .all(|value| !value.contains(['\r', '\n', '\0'])),
                "Имя и фамилия должны занимать одну строку"
            );
            ensure!(
                about.as_ref().is_none_or(|value| !value.contains('\0')),
                "Описание содержит недопустимый символ"
            );
            if first_name.is_none() && last_name.is_none() && about.is_none() {
                return Ok(Plan::Current);
            }
            Ok(Plan::Write(Patch::Details {
                first_name,
                last_name,
                about,
            }))
        }
        ProfileEdit::Username(proposed) => {
            let proposed = proposed.trim().strip_prefix('@').unwrap_or(proposed.trim());
            let username = match field(&original.username, &fresh.username, proposed) {
                Ok(Some(username)) => username,
                Ok(None) => return Ok(Plan::Current),
                Err(()) => return Ok(Plan::Conflict),
            };
            ensure!(
                username.is_empty()
                    || ((5..=32).contains(&username.len())
                        && username
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')),
                "Имя пользователя должно содержать 5–32 латинские буквы, цифры или подчёркивания"
            );
            Ok(Plan::Write(Patch::Username(username)))
        }
    }
}

fn field(original: &str, fresh: &str, proposed: &str) -> Result<Option<String>, ()> {
    if proposed == original {
        return Ok(None);
    }
    let proposed = proposed.trim();
    if proposed == original || proposed == fresh {
        Ok(None)
    } else if fresh != original {
        Err(())
    } else {
        Ok(Some(proposed.to_owned()))
    }
}

impl Patch {
    pub fn apply(self, snapshot: &mut OwnProfile) {
        match self {
            Self::Details {
                first_name,
                last_name,
                about,
            } => {
                if let Some(value) = first_name {
                    snapshot.details.first_name = value;
                }
                if let Some(value) = last_name {
                    snapshot.details.last_name = value;
                }
                if let Some(value) = about {
                    snapshot.details.about = value;
                }
            }
            Self::Username(value) => snapshot.username = value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProfileDetails;

    fn sample() -> OwnProfile {
        OwnProfile {
            id: 1,
            details: ProfileDetails {
                first_name: "Первое".into(),
                last_name: "Фамилия".into(),
                about: "Старое описание".into(),
            },
            username: "demo_name".into(),
            avatar: None,
        }
    }

    #[test]
    fn sends_only_changed_fields_and_preserves_multiline_text() {
        let before = sample();
        let mut fresh = before.clone();
        fresh.details.last_name = "Изменено извне".into();
        let mut edited = before.details.clone();
        edited.about = "Первая строка\nВторая строка".into();
        let Plan::Write(patch) =
            prepare(&before, &fresh, &ProfileEdit::Details(edited.clone())).unwrap()
        else {
            panic!("expected an about-only patch");
        };
        assert_eq!(
            patch,
            Patch::Details {
                first_name: None,
                last_name: None,
                about: Some(edited.about.clone())
            }
        );
        patch.apply(&mut fresh);
        assert_eq!(fresh.details.last_name, "Изменено извне");
        assert_eq!(fresh.details.about, edited.about);
    }

    #[test]
    fn rejects_same_field_conflicts_and_wrong_accounts() {
        let before = sample();
        let mut fresh = before.clone();
        fresh.details.first_name = "Другой клиент".into();
        let mut edited = before.details.clone();
        edited.first_name = "tgrust".into();
        assert_eq!(
            prepare(&before, &fresh, &ProfileEdit::Details(edited.clone())).unwrap(),
            Plan::Conflict
        );
        fresh.id += 1;
        assert!(prepare(&before, &fresh, &ProfileEdit::Details(edited)).is_err());
    }

    #[test]
    fn username_can_be_removed_and_an_at_prefix_is_optional() {
        let before = sample();
        assert_eq!(
            prepare(
                &before,
                &before,
                &ProfileEdit::Username(" @demo_name ".into())
            )
            .unwrap(),
            Plan::Current
        );
        assert_eq!(
            prepare(&before, &before, &ProfileEdit::Username(String::new())).unwrap(),
            Plan::Write(Patch::Username(String::new()))
        );
        assert!(prepare(&before, &before, &ProfileEdit::Username("@имя".into())).is_err());
    }

    #[test]
    fn required_first_name_and_empty_optional_fields_are_distinct() {
        let before = sample();
        let mut edited = before.details.clone();
        edited.first_name = " ".into();
        assert!(prepare(&before, &before, &ProfileEdit::Details(edited)).is_err());
        let mut edited = before.details.clone();
        edited.last_name.clear();
        edited.about.clear();
        assert_eq!(
            prepare(&before, &before, &ProfileEdit::Details(edited)).unwrap(),
            Plan::Write(Patch::Details {
                first_name: None,
                last_name: Some(String::new()),
                about: Some(String::new())
            })
        );
    }
}
