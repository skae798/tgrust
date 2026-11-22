use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::{
    AttachmentThumbnail, AudioInfo, Avatar, BotCommand, Chat, ChatFolder, ChatImage, ChatKind,
    ChatMember, Contact, MEMBER_PAGE_SIZE, MemberFilter, MemberPage, MemberRole, Message,
    PeerProfile, PlayableMedia, PlaybackProgress, PlaybackSource, PollDraft, TextLink,
};

pub const OWN_NAME: &str = "Алексей";
pub const OWN_AVATAR: Avatar = Avatar {
    peer: 4,
    photo: 4,
    dc: 0,
};
pub const FILE_NAME: &str = "заметки.txt";
pub const FILE_CONTENT: &str = "Подготовить макеты, согласовать сроки и проверить сборку.\n";

pub fn has_sample_attachment(chat: i64, message: i32) -> bool {
    chat == 1 && message == 83
}

pub fn chats() -> Vec<Chat> {
    [
        (1, "Дизайн-команда", "Пробный кадр", 3, false),
        (2, "Избранное", "Проверить сборку на ноутбуке", 0, false),
        (3, "Новости проекта", "Опубликован план на неделю", 0, true),
        (5, "Демомедиа", "Видео и аудио", 0, false),
        (6, "Демоопросы", "Опрос · Когда провести встречу?", 0, false),
        (7, "Бот проекта", "Доступные команды проекта", 0, false),
    ]
    .into_iter()
    .map(|(id, title, preview, unread, broadcast)| Chat {
        id,
        pinned: false,
        last_activity: now() - id,
        title: title.to_owned(),
        preview: preview.to_owned(),
        preview_sender: matches!(id, 1 | 6).then(|| "Анна".to_owned()),
        preview_image: history(id).last().and_then(|message| {
            message
                .thumbnail
                .map(|media| ChatImage::attachment(id, message.id, media))
        }),
        group: matches!(id, 1 | 6),
        unread,
        unread_mark: false,
        last_message_id: history(id).last().map_or(0, |message| message.id),
        broadcast,
        saved_messages: id == 2,
        avatar: (id != 2).then_some(Avatar {
            peer: id,
            photo: id,
            dc: 0,
        }),
        archived: id == 3,
        kind: match id {
            1 | 6 => ChatKind::Group,
            3 => ChatKind::Broadcast,
            7 => ChatKind::Bot,
            _ => ChatKind::User,
        },
        muted: false,
    })
    .collect()
}

pub fn folders() -> Vec<ChatFolder> {
    vec![
        ChatFolder {
            id: 2,
            title: "Команда".into(),
            kinds: vec![ChatKind::Group],
            ..ChatFolder::default()
        },
        ChatFolder {
            id: 3,
            title: "С уведомлениями".into(),
            kinds: vec![ChatKind::Group, ChatKind::User, ChatKind::Broadcast],
            exclude_muted: true,
            ..ChatFolder::default()
        },
        ChatFolder {
            id: 4,
            title: "Непрочитанные".into(),
            kinds: vec![
                ChatKind::Group,
                ChatKind::User,
                ChatKind::Broadcast,
                ChatKind::Contact,
            ],
            exclude_read: true,
            ..ChatFolder::default()
        },
    ]
}

pub fn peer_profile(id: i64) -> Option<PeerProfile> {
    if let Some(contact) = contacts().into_iter().find(|contact| contact.id == id) {
        return Some(PeerProfile {
            id,
            title: contact.name,
            kind: contact.kind,
            is_self: false,
            blocked: Some(false),
            avatar: contact.avatar,
            about: "Демонстрационный контакт.".into(),
            usernames: contact.usernames,
            phone: contact.phone,
            members: None,
            online_members: None,
            verified: false,
            scam: false,
            fake: false,
            can_edit_chat: false,
            can_view_members: false,
            bot_commands: Vec::new(),
        });
    }
    let chat = chats().into_iter().find(|chat| chat.id == id)?;
    Some(PeerProfile {
        id,
        title: if chat.saved_messages {
            OWN_NAME.to_owned()
        } else {
            chat.title
        },
        kind: chat.kind,
        is_self: chat.saved_messages,
        blocked: matches!(chat.kind, ChatKind::Broadcast | ChatKind::Bot).then_some(false),
        avatar: if chat.saved_messages {
            Some(OWN_AVATAR)
        } else {
            chat.avatar
        },
        about: match id {
            1 => "Обсуждаем макеты, сроки и материалы проекта. Демонстрационная группа.",
            2 => "Демонстрационные данные аккаунта.",
            6 => "Демонстрационная группа с опросами и викториной.",
            7 => "Демонстрационный бот для команд проекта.",
            _ => "Объявления и новости проекта. Демонстрационный канал.",
        }
        .to_owned(),
        usernames: vec![
            match id {
                1 => "@demo_tgrust_team",
                2 => "@demo_tgrust_account",
                6 => "@demo_tgrust_polls",
                7 => "@demo_tgrust_bot",
                _ => "@demo_tgrust_news",
            }
            .to_owned(),
        ],
        phone: chat.saved_messages.then(|| "+00000000000".to_owned()),
        members: matches!(id, 1 | 3 | 6).then_some(if id == 6 { 8 } else { 45 }),
        online_members: matches!(id, 1 | 6).then_some(2),
        verified: id == 3,
        scam: false,
        fake: false,
        can_edit_chat: matches!(id, 1 | 3),
        can_view_members: matches!(id, 1 | 3 | 6),
        bot_commands: if id == 7 {
            [("/start", "Запустить бота"), ("/help", "Показать справку")]
                .into_iter()
                .map(|(name, description)| BotCommand {
                    name: name.into(),
                    description: description.into(),
                })
                .collect()
        } else {
            Vec::new()
        },
    })
}

pub fn members(chat: i64, filter: MemberFilter, offset: i32) -> MemberPage {
    let count = if chat == 6 { 8 } else { 45 };
    let members: Vec<_> = (1..=count)
        .map(|id| ChatMember {
            id,
            name: match id {
                1 => "Алексей".into(),
                2 => "Анна".into(),
                3 => "Бот проекта".into(),
                _ => format!("Участник {id}"),
            },
            avatar: (id <= 3).then_some(OWN_AVATAR),
            role: match id {
                1 => MemberRole::Owner,
                2 => MemberRole::Administrator,
                _ => MemberRole::Member,
            },
            bot: id == 3,
        })
        .filter(|member| filter.includes(member))
        .collect();
    let end = offset + MEMBER_PAGE_SIZE;
    MemberPage {
        next: (end < members.len() as i32).then_some(end),
        members: members
            .into_iter()
            .skip(offset as usize)
            .take(MEMBER_PAGE_SIZE as usize)
            .collect(),
    }
}

pub fn own_profile() -> crate::model::OwnProfile {
    crate::model::OwnProfile {
        id: 2,
        details: crate::model::ProfileDetails {
            first_name: OWN_NAME.into(),
            last_name: String::new(),
            about: "Демонстрационные данные аккаунта.".into(),
        },
        username: "demo_tgrust_account".into(),
        avatar: Some(OWN_AVATAR),
    }
}

pub fn contacts() -> Vec<Contact> {
    [
        (10, "Анна Соколова", "@demo_anna"),
        (11, "Борис Мельник", "@demo_boris"),
        (12, "Мария Орлова", "@demo_maria"),
    ]
    .into_iter()
    .map(|(id, name, username)| Contact {
        id,
        name: name.into(),
        usernames: vec![username.into()],
        phone: (id == 10).then(|| "+00000000010".into()),
        avatar: (id != 12).then_some(Avatar {
            peer: id,
            photo: id,
            dc: 0,
        }),
        kind: ChatKind::Contact,
        is_self: false,
        available: true,
    })
    .collect()
}

pub fn blocked_page(ids: &[i64], offset: i32) -> crate::model::BlockedPage {
    use crate::model::{BLOCKED_PAGE_SIZE, BlockedPage, BlockedPeer};
    let peers = ids
        .iter()
        .copied()
        .skip(offset as usize)
        .take(BLOCKED_PAGE_SIZE as usize)
        .map(|id| {
            let profile = peer_profile(id);
            BlockedPeer {
                id,
                name: profile.as_ref().map_or_else(
                    || format!("Демопрофиль {}", id - 10000),
                    |profile| profile.title.clone(),
                ),
                username: profile
                    .as_ref()
                    .and_then(|profile| profile.usernames.first().cloned()),
                avatar: profile.and_then(|profile| profile.avatar),
                available: true,
            }
        })
        .collect::<Vec<_>>();
    let next = offset + peers.len() as i32;
    BlockedPage {
        peers,
        next_offset: ((next as usize) < ids.len()).then_some(next),
    }
}

pub fn history(chat: i64) -> Vec<Message> {
    if chat == 6 {
        let mut meeting = poll_message(
            2,
            PollDraft {
                question: "Когда провести встречу?".into(),
                choices: vec!["В понедельник".into(), "В среду".into(), "В пятницу".into()],
                multiple: true,
                public: false,
            },
        );
        let mut quiz = poll_message(
            1,
            PollDraft {
                question: "Какой день идёт после вторника?".into(),
                choices: vec!["Среда".into(), "Воскресенье".into()],
                ..PollDraft::default()
            },
        );
        if let Some(poll) = &mut quiz.poll {
            poll.quiz = true;
            poll.revoting_disabled = true;
        }
        for message in [&mut meeting, &mut quiz] {
            message.sender = "Анна".into();
            message.sender_avatar = Some(Avatar {
                peer: 6,
                photo: 6,
                dc: 0,
            });
            message.self_authored = false;
            message.outgoing = false;
            message.can_delete = false;
        }
        return vec![quiz, meeting];
    }
    let today = now();
    let samples = match chat {
        1 => vec![
            (
                "Анна",
                "Привет! Как тебе обновлённый список чатов?",
                false,
                None,
            ),
            (
                OWN_NAME,
                "Стало проще найти нужный диалог. Давай ещё проверим, как окно ведёт себя при сужении.",
                true,
                None,
            ),
            (
                "Анна",
                "Отправила заметки по встрече",
                false,
                Some(FILE_NAME),
            ),
            ("Анна", "Пробный кадр", false, None),
        ],
        2 => vec![
            (OWN_NAME, "Проверить сборку на ноутбуке", true, None),
            ("Анна", "Сохранённая заметка", false, None),
        ],
        3 => vec![("Новости проекта", "Опубликован план на неделю", false, None)],
        7 => vec![("Бот проекта", "Доступные команды проекта", false, None)],
        5 => vec![
            ("Анна", "", false, Some("demo-audio.wav")),
            (OWN_NAME, "", true, Some("demo-video.webm")),
            (
                "Анна",
                "Подробности по ссылке и текст для копирования. https://example.org/?source=demo&mode=chat",
                false,
                Some("demo-voice.ogg"),
            ),
        ],
        _ => Vec::new(),
    };
    let mut messages: Vec<_> = samples
        .into_iter()
        .enumerate()
        .map(|(index, (sender, text, outgoing, attachment))| Message {
            id: index as i32 + if chat == 1 { 81 } else { 1 },
            sender: sender.to_owned(),
            sender_avatar: if outgoing {
                Some(OWN_AVATAR)
            } else {
                (chat != 2).then_some(Avatar {
                    peer: chat,
                    photo: chat,
                    dc: 0,
                })
            },
            self_authored: outgoing,
            timestamp: today - 60 * (5 - index as i64),
            text: text.to_owned(),
            styles: Vec::new(),
            links: if chat == 5 && index == 2 {
                let link = "по ссылке";
                let start = text.find(link).unwrap_or_default();
                vec![TextLink {
                    range: start..start + link.len(),
                    uri: "https://example.org/details".into(),
                }]
            } else {
                Vec::new()
            },
            outgoing,
            attachment: attachment.map(str::to_owned),
            playback: (chat == 5).then_some(match index {
                0 => PlayableMedia::Audio,
                1 => PlayableMedia::Video {
                    width: 960,
                    height: 540,
                    duration: Some(8),
                    size: include_bytes!("../data/demo-video.webm").len() as u64,
                },
                _ => PlayableMedia::Voice,
            }),
            audio: match (chat, index) {
                (5, 0) => Some(AudioInfo {
                    title: Some("Пробный тон".into()),
                    performer: Some("Демозапись".into()),
                    duration: Some(8),
                    waveform: Vec::new(),
                }),
                (5, 2) => Some(AudioInfo {
                    duration: Some(4),
                    waveform: vec![
                        31, 17, 24, 22, 11, 20, 23, 21, 21, 22, 23, 21, 21, 21, 21, 22, 2, 29, 17,
                        21, 23, 3, 0, 0, 0, 3, 8, 20, 22, 28, 3, 21, 22, 4, 8, 24, 2, 3, 26, 21,
                        29, 23, 16, 13, 0, 0, 0, 0,
                    ],
                    ..AudioInfo::default()
                }),
                _ => None,
            },
            photo_preview: (chat == 1 && index == 3) || chat == 3,
            thumbnail: if chat == 5 && index < 2 {
                Some(AttachmentThumbnail::Document(50 + index as i64))
            } else {
                ((chat == 1 && index == 3) || chat == 3).then_some(AttachmentThumbnail::Photo(chat))
            },
            poll: None,
            reply_to: (chat == 1 && index == 1).then_some(81),
            reply_preview: None,
            can_reply: true,
            can_edit: outgoing && attachment.is_none(),
            can_delete: outgoing || chat == 2,
            can_forward: true,
            forwarded: false,
            pinned: false,
            reactions: Vec::new(),
            edited: false,
            ephemeral: false,
            album: None,
        })
        .collect();
    if chat == 1 {
        let sample = messages[0].clone();
        let day = 24 * 60 * 60;
        let older = (1..=80).map(|id| {
            let mut message = Message {
                id,
                timestamp: today - 2 * day + i64::from(id) * 60,
                text: format!("Заметка о проекте № {id}"),
                ..sample.clone()
            };
            if id == 75 {
                message.sender = "Дизайн-команда".into();
                message.text = "Сообщение от имени группы".into();
                message.outgoing = true;
            } else if id == 76 {
                message.sender = "Новости проекта".into();
                message.sender_avatar = Some(Avatar {
                    peer: 3,
                    photo: 3,
                    dc: 0,
                });
                message.text = "Сообщение от имени канала".into();
            }
            message
        });
        messages.splice(..0, older);
    }
    messages
}

pub fn poll_message(id: i32, draft: PollDraft) -> Message {
    Message {
        id,
        sender: OWN_NAME.into(),
        sender_avatar: Some(OWN_AVATAR),
        self_authored: true,
        timestamp: now(),
        text: String::new(),
        styles: Vec::new(),
        links: Vec::new(),
        outgoing: true,
        attachment: None,
        photo_preview: false,
        playback: None,
        audio: None,
        thumbnail: None,
        poll: Some(draft.into_poll(i64::from(id))),
        reply_to: None,
        reply_preview: None,
        can_reply: true,
        can_edit: false,
        can_delete: true,
        can_forward: true,
        forwarded: false,
        pinned: false,
        reactions: Vec::new(),
        edited: false,
        ephemeral: false,
        album: None,
    }
}

pub fn preview_pixels() -> Vec<u8> {
    let mut pixels = Vec::with_capacity(480 * 300 * 3);
    for y in 0..300 {
        for x in 0..480 {
            let rgb = if (x - 350_i32).pow(2) + (y - 85_i32).pow(2) < 31_i32.pow(2) {
                [255, 207, 140]
            } else if y > 165 + (x - 300_i32).abs() / 4 {
                [41, 72, 89]
            } else if y > 125 + (x - 115_i32).abs() / 3 {
                [67, 112, 129]
            } else {
                [105, 160, 182]
            };
            pixels.extend_from_slice(&rgb);
        }
    }
    pixels
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub async fn playback_preview(
    media: PlayableMedia,
    progress: tokio::sync::watch::Sender<PlaybackProgress>,
    whole: bool,
) -> anyhow::Result<PlaybackSource> {
    let bytes: &[u8] = match media {
        PlayableMedia::Audio => include_bytes!("../data/demo-audio.wav"),
        PlayableMedia::Voice => include_bytes!("../data/demo-voice.ogg"),
        PlayableMedia::Video { .. } => include_bytes!("../data/demo-video.webm"),
    };
    if !whole && media != PlayableMedia::Voice {
        let stream = crate::media_stream::StreamSource::new(bytes.len() as u64, move |index| {
            Box::pin(async move {
                let offset = index as usize * crate::media_stream::BLOCK_SIZE as usize;
                let end = (offset + crate::media_stream::BLOCK_SIZE as usize).min(bytes.len());
                Ok(bytes::Bytes::from_static(&bytes[offset..end]))
            })
        })?;
        return Ok(PlaybackSource::Stream(media, stream));
    }
    let mut output = crate::storage::PlaybackDownload::new(bytes.len() as u64)?;
    progress.send_replace(PlaybackProgress {
        received: 0,
        total: bytes.len() as u64,
    });
    output.write(bytes).await?;
    progress.send_replace(PlaybackProgress {
        received: bytes.len() as u64,
        total: bytes.len() as u64,
    });
    Ok(PlaybackSource::File(output.finish(media).await?))
}

pub fn device_sessions() -> crate::model::DeviceSessions {
    use crate::model::{DeviceSession, DeviceSessions};
    let current = DeviceSession {
        id: 0,
        current: true,
        device: "Демонстрационный компьютер".into(),
        platform: "Linux".into(),
        system_version: "GNOME".into(),
        application: "tgrust".into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        created: now() - 172_800,
        active: now(),
        ip: "192.0.2.10".into(),
        country: "Тестовая страна".into(),
        region: "Демонстрационный регион".into(),
        official: false,
        password_pending: false,
        unconfirmed: false,
    };
    let mut phone = current.clone();
    phone.id = 2;
    phone.current = false;
    phone.device = "Тестовый телефон".into();
    phone.platform = "Android".into();
    phone.system_version = "Демоверсия".into();
    phone.application = "Telegram".into();
    phone.app_version = "демо".into();
    phone.ip = "198.51.100.20".into();
    phone.active -= 3600;
    phone.official = true;
    let mut laptop = phone.clone();
    laptop.id = -3;
    laptop.device = "Тестовый ноутбук".into();
    laptop.platform = "Linux".into();
    laptop.application = "Telegram Desktop".into();
    laptop.ip = "203.0.113.30".into();
    laptop.active -= 86_400;
    laptop.unconfirmed = true;
    DeviceSessions {
        sessions: vec![current, phone, laptop],
        inactive_ttl_days: 180,
    }
}

pub fn privacy_settings()
-> std::collections::HashMap<crate::model::PrivacyKey, crate::model::PrivacySetting> {
    use crate::model::{PrivacyAudience::*, PrivacyKey::*, PrivacySetting};
    [
        (PhoneNumber, Some(Nobody), 2, true),
        (AddedByPhone, Some(Everyone), 0, true),
        (LastSeen, Some(Contacts), 1, true),
        (ProfilePhoto, None, 1, false),
    ]
    .into_iter()
    .map(|(key, audience, exception_rules, editable)| {
        (
            key,
            PrivacySetting {
                audience,
                exception_rules,
                editable,
                revision: Vec::new(),
            },
        )
    })
    .collect()
}
