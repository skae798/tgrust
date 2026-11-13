use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::{Chat, Message};

pub const FILE_NAME: &str = "заметки.txt";
pub const FILE_CONTENT: &str = "Подготовить макеты, согласовать сроки и проверить сборку.\n";

pub fn has_sample_attachment(chat: i64, message: i32) -> bool {
    chat == 1 && message == 3
}

pub fn chats() -> Vec<Chat> {
    [
        (
            1,
            "Дизайн-команда",
            "Отправила заметки по встрече",
            2,
            false,
        ),
        (2, "Избранное", "Проверить сборку на ноутбуке", 0, false),
        (3, "Новости проекта", "Опубликован план на неделю", 0, true),
    ]
    .into_iter()
    .map(|(id, title, preview, unread, broadcast)| Chat {
        id,
        title: title.to_owned(),
        preview: preview.to_owned(),
        unread,
        broadcast,
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
                "Вы",
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
        ],
        2 => vec![("Вы", "Проверить сборку на ноутбуке", true, None)],
        _ => vec![("Новости проекта", "Опубликован план на неделю", false, None)],
    };
    samples
        .into_iter()
        .enumerate()
        .map(|(index, (sender, text, outgoing, attachment))| Message {
            id: index as i32 + 1,
            sender: sender.to_owned(),
            timestamp: today - 60 * (5 - index as i64),
            text: text.to_owned(),
            outgoing,
            attachment: attachment.map(str::to_owned),
            reply_to: None,
            can_reply: true,
            can_edit: outgoing && attachment.is_none(),
            can_delete: outgoing,
            can_forward: true,
            forwarded: false,
            pinned: false,
            reactions: Vec::new(),
            edited: false,
        })
        .collect()
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}
