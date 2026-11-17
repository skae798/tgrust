use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::{
    AttachmentThumbnail, Avatar, Chat, ChatFolder, ChatImage, ChatKind, Contact, Message,
    PeerProfile,
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
    ]
    .into_iter()
    .map(|(id, title, preview, unread, broadcast)| Chat {
        id,
        pinned: false,
        last_activity: now() - id,
        title: title.to_owned(),
        preview: preview.to_owned(),
        preview_sender: (id == 1).then(|| "Анна".to_owned()),
        preview_image: history(id).last().and_then(|message| {
            message
                .thumbnail
                .map(|media| ChatImage::attachment(id, message.id, media))
        }),
        group: id == 1,
        unread,
        broadcast,
        saved_messages: id == 2,
        avatar: (id != 2).then_some(Avatar {
            peer: id,
            photo: id,
            dc: 0,
        }),
        archived: id == 3,
        kind: match id {
            1 => ChatKind::Group,
            3 => ChatKind::Broadcast,
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
    ]
}

pub fn peer_profile(id: i64) -> Option<PeerProfile> {
    if let Some(contact) = contacts().into_iter().find(|contact| contact.id == id) {
        return Some(PeerProfile {
            id,
            title: contact.name,
            kind: contact.kind,
            is_self: false,
            avatar: contact.avatar,
            about: "Демонстрационный контакт.".into(),
            usernames: contact.usernames,
            phone: contact.phone,
            members: None,
            online_members: None,
            verified: false,
            scam: false,
            fake: false,
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
        avatar: if chat.saved_messages {
            Some(OWN_AVATAR)
        } else {
            chat.avatar
        },
        about: match id {
            1 => "Обсуждаем макеты, сроки и материалы проекта. Демонстрационная группа.",
            2 => "Демонстрационные данные аккаунта.",
            _ => "Объявления и новости проекта. Демонстрационный канал.",
        }
        .to_owned(),
        usernames: vec![
            match id {
                1 => "@demo_tgrust_team",
                2 => "@demo_tgrust_account",
                _ => "@demo_tgrust_news",
            }
            .to_owned(),
        ],
        phone: chat.saved_messages.then(|| "+00000000000".to_owned()),
        members: (id != 2).then_some(if id == 1 { 8 } else { 120 }),
        online_members: (id == 1).then_some(2),
        verified: id == 3,
        scam: false,
        fake: false,
    })
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

pub fn history(chat: i64) -> Vec<Message> {
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
            timestamp: today - 60 * (5 - index as i64),
            text: text.to_owned(),
            styles: Vec::new(),
            outgoing,
            attachment: attachment.map(str::to_owned),
            photo_preview: (chat == 1 && index == 3) || chat == 3,
            thumbnail: ((chat == 1 && index == 3) || chat == 3)
                .then_some(AttachmentThumbnail::Photo(chat)),
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
        let older = (1..=80).map(|id| Message {
            id,
            timestamp: today - 2 * day + i64::from(id) * 60,
            text: format!("Заметка о проекте № {id}"),
            ..sample.clone()
        });
        messages.splice(..0, older);
    }
    messages
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
