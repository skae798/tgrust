use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::{
    ADMIN_LOG_PAGE_SIZE, AdminLogActor, AdminLogEntry, AdminLogFilter, AdminLogPage, AdminLogQuery,
    AdminPolicy, AdminRight, AdminRights, AdminState, AttachmentThumbnail, AudioInfo, Avatar,
    BotCommand, Chat, ChatFolder, ChatImage, ChatKind, ChatMember, Contact, CreatedChat,
    FORUM_PAGE_SIZE, ForumCursor, ForumMode, ForumPage, ForumPermissions, ForumTopic,
    ForumTopicChange, ForumWrite, ForumWriteResult, GENERAL_TOPIC, GroupPermission,
    GroupPermissionSettings, GroupPermissions, INVITE_PAGE_SIZE, InviteAction, InviteCursor,
    InviteDraft, InviteLink, InvitePage, JOIN_REQUEST_PAGE_SIZE, JoinDecisionOutcome, JoinRequest,
    JoinRequestCursor, JoinRequestPage, MEMBER_PAGE_SIZE, MemberChange, MemberFilter,
    MemberModeration, MemberPage, MemberRestriction, MemberRestrictionEdit,
    MemberRestrictionSettings, MemberRole, Message, NewChatDraft, NewChatKind, NewChatResult,
    PeerProfile, PlayableMedia, PlaybackProgress, PlaybackSource, PollDraft, TextLink,
    forum_topic_title, restriction_now,
};

pub const OWN_NAME: &str = "Алексей";
pub const OWN_AVATAR: Avatar = Avatar {
    peer: 4,
    photo: 4,
    dc: 0,
};
pub const FILE_NAME: &str = "заметки.txt";
pub const FILE_CONTENT: &str = "Подготовить макеты, согласовать сроки и проверить сборку.\n";
pub const SAMPLE_QUIZ_ID: i32 = 1;
pub const SLOW_MODE_GROUP: i64 = 8;
pub const MODERATION_GROUP: i64 = 9;
pub const FORUM_CHAT: i64 = 14;
const PUBLIC_POLL_ID: i32 = 73;
pub const SLOW_MODE_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

pub fn has_sample_attachment(chat: i64, message: i32) -> bool {
    chat == 1 && message == 83
}

pub fn chats() -> Vec<Chat> {
    [
        (1, "Дизайн-команда", "Пробный кадр", 3, false),
        (2, "Избранное", "Проверить сборку на ноутбуке", 0, false),
        (3, "Новости проекта", "Опубликован план на неделю", 0, true),
        (5, "Демомедиа", "Видео и аудио", 0, false),
        (
            6,
            "Демоопросы",
            "Опрос · Когда открыть обсуждение проекта?",
            0,
            false,
        ),
        (7, "Бот проекта", "Доступные команды проекта", 0, false),
        (
            SLOW_MODE_GROUP,
            "Демо медленного режима",
            "Интервал между сообщениями — 10 секунд",
            0,
            false,
        ),
        (
            MODERATION_GROUP,
            "Демо модерации",
            "Разрешения и ограничения участников",
            0,
            false,
        ),
        (FORUM_CHAT, "Демофорум", "Обсуждение по темам", 3, false),
    ]
    .into_iter()
    .map(|(id, title, preview, unread, broadcast)| Chat {
        id,
        pinned: false,
        last_activity: now() - id,
        title: title.to_owned(),
        preview: preview.to_owned(),
        preview_sender: matches!(id, 1 | 6 | SLOW_MODE_GROUP | MODERATION_GROUP | FORUM_CHAT)
            .then(|| "Анна".to_owned()),
        preview_image: history(id).last().and_then(|message| {
            message
                .thumbnail
                .map(|media| ChatImage::attachment(id, message.id, media))
        }),
        group: matches!(id, 1 | 6 | SLOW_MODE_GROUP | MODERATION_GROUP | FORUM_CHAT),
        supergroup: matches!(id, 1 | 6 | SLOW_MODE_GROUP | MODERATION_GROUP | FORUM_CHAT),
        forum: id == FORUM_CHAT,
        unread,
        unread_mark: false,
        last_message_id: history(id).last().map_or(0, |message| message.id),
        broadcast,
        can_write: !broadcast,
        slow_mode_enabled: false,
        saved_messages: id == 2,
        avatar: (id != 2).then_some(Avatar {
            peer: id,
            photo: id,
            dc: 0,
        }),
        archived: id == 3,
        kind: match id {
            1 | 6 | SLOW_MODE_GROUP | MODERATION_GROUP | FORUM_CHAT => ChatKind::Group,
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
            moderation: None,
            admin_policy: None,
            can_manage_invites: false,
            can_add_members: false,
            can_read_admin_log: false,
            addresses: None,
            group_permissions: None,
            forum_mode: None,
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
            SLOW_MODE_GROUP => {
                "В этой демонстрационной группе участники отправляют сообщения раз в 10 секунд."
            }
            MODERATION_GROUP => {
                "В этой демонстрационной супергруппе можно менять разрешения и ограничения."
            }
            FORUM_CHAT => "Демонстрационная супергруппа с темами.",
            _ => "Объявления и новости проекта. Демонстрационный канал.",
        }
        .to_owned(),
        usernames: vec![
            match id {
                1 => "@demo_tgrust_team",
                2 => "@demo_tgrust_account",
                6 => "@demo_tgrust_polls",
                7 => "@demo_tgrust_bot",
                SLOW_MODE_GROUP => "@demo_tgrust_slow_mode",
                MODERATION_GROUP => "@demo_tgrust_moderation",
                FORUM_CHAT => "@demo_tgrust_forum",
                _ => "@demo_tgrust_news",
            }
            .to_owned(),
        ],
        phone: chat.saved_messages.then(|| "+00000000000".to_owned()),
        members: matches!(id, 1 | 3 | 6 | MODERATION_GROUP).then_some(if id == 6 { 8 } else { 45 }),
        online_members: matches!(id, 1 | 6).then_some(2),
        verified: id == 3,
        scam: false,
        fake: false,
        can_edit_chat: matches!(id, 1 | 3),
        can_view_members: matches!(id, 1 | 3 | 6 | MODERATION_GROUP),
        moderation: demo_moderation(id),
        admin_policy: demo_admin_policy(id),
        can_manage_invites: matches!(id, 1 | 3),
        can_add_members: matches!(id, 1 | 3),
        can_read_admin_log: id == 3,
        addresses: (id == 3).then(|| crate::model::ChatAddresses {
            primary: "demo_tgrust_news".into(),
            aliases: vec![
                crate::model::AttachedAddress {
                    name: "demo_news_archive".into(),
                    active: true,
                    can_toggle: true,
                },
                crate::model::AttachedAddress {
                    name: "demo_extra_news".into(),
                    active: false,
                    can_toggle: true,
                },
            ],
        }),
        group_permissions: match id {
            1 => Some(group_permissions(GroupPermissions::all(), false)),
            MODERATION_GROUP => Some(GroupPermissionSettings {
                slow_mode: Some(0),
                ..group_permissions(GroupPermissions::all(), false)
            }),
            _ => None,
        },
        forum_mode: (id == FORUM_CHAT).then_some(ForumMode {
            enabled: true,
            linked_discussion: false,
            hidden_history: false,
        }),
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

pub fn create_chat(id: i64, mut draft: NewChatDraft) -> NewChatResult {
    if draft.username == "demo_taken" {
        return NewChatResult::Rejected("Это публичное имя занято.".into());
    }
    if draft.username == "demo_unconfirmed" {
        return NewChatResult::Unconfirmed("Telegram не прислал ответ вовремя.".into());
    }
    let public_error = (draft.username == "demo_race").then(|| "Это публичное имя занято.".into());
    let mut warnings = Vec::new();
    if let Some(contact) = draft.members.iter().find(|contact| contact.id == 12) {
        warnings.push(format!(
            "Telegram не добавил следующих участников.\n{}",
            contact.name
        ));
        draft.members.retain(|contact| contact.id != 12);
    }
    NewChatResult::Created(Box::new(CreatedChat {
        chat: draft.chat(id, now()),
        draft,
        warnings,
        public_error,
    }))
}

pub fn created_profile(id: i64, draft: &NewChatDraft) -> PeerProfile {
    let broadcast = draft.kind == NewChatKind::Channel;
    PeerProfile {
        id,
        kind: if broadcast {
            ChatKind::Broadcast
        } else {
            ChatKind::Group
        },
        title: draft.info.title.clone(),
        about: draft.info.about.clone(),
        usernames: (!draft.username.is_empty())
            .then(|| format!("@{}", draft.username))
            .into_iter()
            .collect(),
        members: u32::try_from(draft.members.len() + 1).ok(),
        is_self: false,
        blocked: None,
        avatar: None,
        phone: None,
        online_members: None,
        verified: false,
        scam: false,
        fake: false,
        can_edit_chat: true,
        can_view_members: true,
        moderation: None,
        admin_policy: None,
        can_manage_invites: true,
        can_add_members: true,
        can_read_admin_log: draft.kind != NewChatKind::Group,
        addresses: (draft.kind != NewChatKind::Group).then(|| crate::model::ChatAddresses {
            primary: draft.username.clone(),
            aliases: Vec::new(),
        }),
        group_permissions: (!broadcast).then(|| GroupPermissionSettings {
            slow_mode: (draft.kind == NewChatKind::Supergroup).then_some(0),
            forum: draft.forum,
            ..group_permissions(GroupPermissions::all(), !draft.username.is_empty())
        }),
        forum_mode: (draft.kind == NewChatKind::Supergroup).then_some(ForumMode {
            enabled: draft.forum,
            linked_discussion: false,
            hidden_history: false,
        }),
        bot_commands: Vec::new(),
    }
}

pub fn group_permissions(allowed: GroupPermissions, public: bool) -> GroupPermissionSettings {
    use GroupPermission::{ChangeInfo, Pin, Topics};
    GroupPermissionSettings {
        allowed: allowed
            .with(Pin, !public && allowed.has(Pin))
            .with(ChangeInfo, !public && allowed.has(ChangeInfo)),
        editable: GroupPermissions::all()
            .with(Topics, false)
            .with(Pin, !public)
            .with(ChangeInfo, !public),
        forum: false,
        slow_mode: None,
    }
}

pub fn created_members(
    chat: i64,
    draft: &NewChatDraft,
    changes: &MemberChanges,
    filter: MemberFilter,
    offset: i32,
) -> MemberPage {
    let owner = ChatMember {
        id: OWN_AVATAR.peer,
        name: OWN_NAME.into(),
        avatar: Some(OWN_AVATAR),
        role: MemberRole::Owner,
        bot: false,
        can_update: false,
        admin: None,
    };
    let members = std::iter::once(owner)
        .chain(
            draft
                .members
                .iter()
                .chain(
                    changes
                        .invited
                        .iter()
                        .filter_map(|(&(target, _), contact)| (target == chat).then_some(contact)),
                )
                .map(|member| ChatMember {
                    id: member.id,
                    name: member.name.clone(),
                    avatar: member.avatar,
                    role: MemberRole::Member,
                    bot: member.kind == ChatKind::Bot,
                    can_update: false,
                    admin: None,
                }),
        )
        .filter(|member| filter.includes(member))
        .collect::<Vec<_>>();
    member_page(members, offset)
}

#[derive(Default)]
pub struct InviteLinks {
    links: HashMap<i64, Vec<InviteLink>>,
    created: u32,
}

impl InviteLinks {
    fn links(&mut self, chat: i64) -> &mut Vec<InviteLink> {
        self.links.entry(chat).or_insert_with(|| {
            (0..28)
                .map(|index| InviteLink {
                    cursor: InviteCursor {
                        date: 1_700_000_000 - index,
                        link: format!("https://example.invalid/+demo-{chat}-{index}"),
                    },
                    title: match index {
                        0 => "Основная ссылка".into(),
                        1 => "Превышен лимит".into(),
                        2 => "Подписка проекта".into(),
                        27 => "Отозванная ссылка".into(),
                        _ => format!("Приглашение {index}"),
                    },
                    permanent: index == 0,
                    revoked: index == 27,
                    expires: None,
                    limit: (index == 1).then_some(2),
                    uses: if index == 1 { 2 } else { 0 },
                    approval: false,
                    pending: 0,
                    paid: index == 2,
                })
                .collect()
        })
    }

    pub fn page(&mut self, chat: i64, revoked: bool, after: Option<InviteCursor>) -> InvitePage {
        let links = self.links(chat);
        let offset = after
            .as_ref()
            .and_then(|after| links.iter().position(|link| &link.cursor == after))
            .map_or(0, |index| index + 1);
        let mut links: Vec<_> = links[offset..]
            .iter()
            .filter(|link| link.revoked == revoked)
            .take(INVITE_PAGE_SIZE as usize + 1)
            .cloned()
            .collect();
        let next = if links.len() > INVITE_PAGE_SIZE as usize {
            links.pop();
            links.last().map(|link| link.cursor.clone())
        } else {
            None
        };
        InvitePage { links, next }
    }

    pub fn change(&mut self, chat: i64, action: InviteAction) -> anyhow::Result<()> {
        match action {
            InviteAction::Create(draft) => {
                let draft = draft.normalize()?;
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                let expires = draft.expiration(now)?;
                self.created += 1;
                let link = format!("https://example.invalid/+demo-{chat}-new-{}", self.created);
                self.links(chat).insert(
                    0,
                    InviteLink {
                        cursor: InviteCursor {
                            date: i32::try_from(now)?,
                            link,
                        },
                        title: draft.title,
                        permanent: false,
                        revoked: false,
                        expires,
                        limit: draft.limit,
                        uses: 0,
                        approval: draft.approval,
                        pending: 0,
                        paid: false,
                    },
                );
            }
            InviteAction::Edit(original, edit) => {
                let link = self
                    .links(chat)
                    .iter_mut()
                    .find(|link| link.cursor.link == original.cursor.link)
                    .ok_or_else(|| anyhow::anyhow!("Ссылка не найдена"))?;
                let changes = edit.changes(&original, link)?;
                if let Some(title) = changes.title {
                    link.title = title;
                }
                if let Some(expires) = changes.expire_date {
                    link.expires = (expires > 0).then_some(expires);
                }
                if let Some(limit) = changes.usage_limit {
                    link.limit = (limit > 0).then_some(limit);
                }
                if let Some(approval) = changes.request_needed {
                    link.approval = approval;
                }
            }
            InviteAction::Revoke(url) => {
                let invite = self
                    .links(chat)
                    .iter_mut()
                    .find(|link| link.cursor.link == url)
                    .ok_or_else(|| anyhow::anyhow!("Ссылка не найдена"))?;
                if invite.revoked {
                    return Ok(());
                }
                invite.revoked = true;
                if invite.permanent {
                    self.change(
                        chat,
                        InviteAction::Create(InviteDraft {
                            title: "Новая основная ссылка".into(),
                            hours: None,
                            limit: None,
                            approval: false,
                        }),
                    )?;
                    self.links(chat)[0].permanent = true;
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct JoinRequests {
    handled: HashSet<(i64, i64)>,
}

impl JoinRequests {
    pub fn page(&self, chat: i64, after: Option<JoinRequestCursor>) -> JoinRequestPage {
        let remaining: Vec<_> = (101..=130)
            .filter(|user| !self.handled.contains(&(chat, *user)))
            .collect();
        let mut requests: Vec<_> = remaining
            .iter()
            .copied()
            .filter(|user| after.is_none_or(|after| *user > after.user))
            .take(JOIN_REQUEST_PAGE_SIZE as usize + 1)
            .map(|user| JoinRequest {
                cursor: JoinRequestCursor {
                    date: 1_700_000_000 - (user as i32 - 101) / 2,
                    user,
                },
                name: match user {
                    101 => "Алёна Миронова".into(),
                    102 => "Максим Крылов".into(),
                    _ => format!("Заявитель {user}"),
                },
                about: if user == 101 {
                    "Хочу присоединиться к обсуждению.".into()
                } else {
                    String::new()
                },
                avatar: None,
                actionable: true,
            })
            .collect();
        let next = if requests.len() > JOIN_REQUEST_PAGE_SIZE as usize {
            requests.pop();
            requests.last().map(|request| request.cursor)
        } else {
            None
        };
        JoinRequestPage {
            requests,
            next,
            total: remaining.len(),
        }
    }

    pub fn decide(&mut self, chat: i64, user: i64) -> JoinDecisionOutcome {
        if self.handled.insert((chat, user)) {
            JoinDecisionOutcome::Applied
        } else {
            JoinDecisionOutcome::AlreadyHandled
        }
    }
}

#[derive(Default)]
pub struct MemberChanges {
    moderation: HashMap<(i64, i64), MemberChange>,
    restrictions: HashMap<(i64, i64), MemberRestriction>,
    invited: HashMap<(i64, i64), Contact>,
}

impl MemberChanges {
    pub fn invited_count(&self, chat: i64) -> u32 {
        self.invited
            .keys()
            .filter(|(target, _)| chat == *target)
            .count() as u32
    }
}

pub fn invitable_contacts() -> Vec<Contact> {
    let mut contacts = contacts();
    contacts.extend(
        [
            (50, "Елена Белова", "demo_elena"),
            (51, "Денис Левин", "demo_denis"),
            (52, "София Коваль", "demo_sofia"),
        ]
        .map(|(id, name, username)| Contact {
            id,
            name: name.into(),
            usernames: vec![format!("@{username}")],
            phone: None,
            avatar: None,
            kind: ChatKind::Contact,
            is_self: false,
            available: true,
        }),
    );
    contacts
}

pub fn invite_members(
    chat: i64,
    created: Option<&NewChatDraft>,
    contacts: Vec<Contact>,
    changes: &mut MemberChanges,
) -> anyhow::Result<crate::model::MemberInvitations> {
    use crate::model::MemberInvitation;
    anyhow::ensure!(
        created.is_some() || matches!(chat, 1 | 3),
        "Приглашение недоступно"
    );
    Ok(contacts
        .into_iter()
        .map(|contact| {
            let role = demo_member_role(chat, contact.id, changes);
            let existing = created.map_or(
                role.is_some_and(|role| role != MemberRole::Banned),
                |draft| {
                    draft.members.iter().any(|member| member.id == contact.id)
                        || changes.invited.contains_key(&(chat, contact.id))
                },
            );
            let status = if existing {
                MemberInvitation::InChat
            } else if created.is_none() && role == Some(MemberRole::Banned) {
                MemberInvitation::Rejected("Участник заблокирован".into())
            } else if contact.id == 52 {
                MemberInvitation::Rejected("Ограничения приватности".into())
            } else {
                changes.moderation.remove(&(chat, contact.id));
                changes.invited.insert((chat, contact.id), contact.clone());
                MemberInvitation::InChat
            };
            (contact.name, status)
        })
        .collect())
}

pub fn admin_log(chat: i64, query: AdminLogQuery, before: Option<i64>) -> AdminLogPage {
    use AdminLogFilter as F;
    let query_text = query.text.to_lowercase();
    let now = now();
    let entries = (1..=43)
        .rev()
        .filter(|&id| chat == 3 && before.is_none_or(|before| id < before))
        .filter_map(|id| {
            let (filter, actor, action, details) = match id {
                43 => (
                    F::Settings,
                    1,
                    "Изменено название",
                    Some("Было\nВести проекта\n\nСтало\nНовости проекта".to_owned()),
                ),
                42 => (
                    F::Permissions,
                    2,
                    "Назначен администратор",
                    Some("Бот проекта\n\nСтало\nПубликовать сообщения".to_owned()),
                ),
                41 => (
                    F::Messages,
                    1,
                    "Сообщение удалено",
                    Some("Черновик афиши для демонстрации".to_owned()),
                ),
                40 => (
                    F::Invites,
                    2,
                    "Пригласительная ссылка отозвана",
                    Some("Приглашение на встречу".to_owned()),
                ),
                39 => (
                    F::Topics,
                    1,
                    "Тема создана",
                    Some("Подготовка мероприятия".to_owned()),
                ),
                38 => (F::Calls, 2, "Начат видеочат", None),
                2 => (F::Members, 4, "Участник вступил в чат", None),
                1 => (F::Members, 2, "Участник вышел из чата", None),
                _ => (F::Members, 1 + id % 2, "Участник вступил в чат", None),
            };
            let name = match (id, actor) {
                (2, _) => "Гость второй страницы",
                (_, 1) => "Алексей",
                _ => "Анна",
            };
            let matches_text = query_text.is_empty()
                || name.to_lowercase().contains(&query_text)
                || action.to_lowercase().contains(&query_text)
                || details
                    .as_ref()
                    .is_some_and(|text| text.to_lowercase().contains(&query_text));
            (matches_text
                && (query.filter == F::All || filter == query.filter)
                && query.actor.is_none_or(|value| value == actor))
            .then(|| AdminLogEntry {
                id,
                date: i32::try_from(now - (44 - id) * 60).unwrap_or_default(),
                actor: AdminLogActor {
                    filter_id: Some(actor),
                    name: name.to_owned(),
                    avatar: (actor == 1 && id != 2).then_some(OWN_AVATAR),
                },
                action: action.to_owned(),
                details,
            })
        })
        .take(ADMIN_LOG_PAGE_SIZE)
        .collect();
    AdminLogPage::new(entries, before)
}

fn demo_moderation(chat: i64) -> Option<MemberModeration> {
    match chat {
        1 => Some(MemberModeration::Remove),
        3 | MODERATION_GROUP => Some(MemberModeration::Ban),
        _ => None,
    }
}

fn demo_admin_policy(chat: i64) -> Option<AdminPolicy> {
    match chat {
        1 => Some(AdminPolicy::Basic),
        3 => Some(AdminPolicy::Channel(AdminRights::channel(
            true, false, true,
        ))),
        _ => None,
    }
}

fn demo_member_role(chat: i64, member: i64, changes: &MemberChanges) -> Option<MemberRole> {
    match changes.moderation.get(&(chat, member)) {
        Some(MemberChange::Remove | MemberChange::Unban) => return None,
        Some(MemberChange::Ban) => return Some(MemberRole::Banned),
        Some(MemberChange::SetAdmin { requested, .. }) => {
            return Some(match requested {
                AdminState::Member => MemberRole::Member,
                AdminState::Administrator(_) => MemberRole::Administrator,
            });
        }
        None => {}
    }
    let role = match member {
        1 => MemberRole::Owner,
        2 => MemberRole::Administrator,
        3..=8 => MemberRole::Member,
        9..=45 if chat != 6 => MemberRole::Member,
        46..=47 if matches!(chat, 3 | MODERATION_GROUP) => MemberRole::Banned,
        _ if changes.invited.contains_key(&(chat, member)) => MemberRole::Member,
        _ => return None,
    };
    Some(
        if role == MemberRole::Member && personal_restriction(changes, chat, member).active() {
            MemberRole::Restricted
        } else {
            role
        },
    )
}

fn personal_restriction(changes: &MemberChanges, chat: i64, member: i64) -> MemberRestriction {
    let personal = changes
        .restrictions
        .get(&(chat, member))
        .copied()
        .unwrap_or_else(|| {
            if chat == MODERATION_GROUP && member == 5 {
                MemberRestriction {
                    allowed: GroupPermissions::all().with(GroupPermission::Voice, false),
                    ..MemberRestriction::default()
                }
            } else {
                MemberRestriction::default()
            }
        });
    if personal.until == 0 || personal.until > restriction_now() {
        personal
    } else {
        MemberRestriction::default()
    }
}

pub fn member_restriction(
    changes: &MemberChanges,
    chat: i64,
    member: i64,
    allowed: GroupPermissions,
) -> anyhow::Result<MemberRestrictionSettings> {
    anyhow::ensure!(
        chat == MODERATION_GROUP
            && matches!(
                demo_member_role(chat, member, changes),
                Some(MemberRole::Member | MemberRole::Restricted)
            ),
        "Ограничения участника недоступны"
    );
    Ok(MemberRestrictionSettings {
        group: group_permissions(allowed, false),
        personal: personal_restriction(changes, chat, member),
    })
}

pub fn edit_member_restriction(
    changes: &mut MemberChanges,
    chat: i64,
    member: i64,
    allowed: GroupPermissions,
    original: MemberRestrictionSettings,
    edit: MemberRestrictionEdit,
) -> anyhow::Result<()> {
    let current = member_restriction(changes, chat, member, allowed)?;
    if let Some(next) = current.prepare(original, edit, restriction_now())? {
        changes.restrictions.insert((chat, member), next);
    }
    Ok(())
}

fn demo_admin_state(chat: i64, member: i64, changes: &MemberChanges) -> Option<AdminState> {
    match demo_member_role(chat, member, changes)? {
        MemberRole::Member => Some(AdminState::Member),
        MemberRole::Administrator => {
            if let Some(MemberChange::SetAdmin { requested, .. }) =
                changes.moderation.get(&(chat, member))
            {
                return Some(*requested);
            }
            let rights = if chat == 3 {
                AdminRights::from_rights([AdminRight::PostMessages, AdminRight::Other])
            } else {
                AdminRights::default()
            };
            Some(AdminState::Administrator(rights))
        }
        _ => None,
    }
}

pub fn change_member(
    changes: &mut MemberChanges,
    chat: i64,
    member: i64,
    change: MemberChange,
) -> anyhow::Result<()> {
    if let MemberChange::SetAdmin {
        original,
        requested,
    } = change
    {
        use anyhow::Context;
        let policy = demo_admin_policy(chat).context("Назначение администратора недоступно")?;
        let current =
            demo_admin_state(chat, member, changes).context("Эту роль нельзя изменить")?;
        if policy.needs_change(original, current, requested)? {
            changes.moderation.insert((chat, member), change);
        }
        return Ok(());
    }
    let allowed = demo_moderation(chat)
        .zip(demo_member_role(chat, member, changes))
        .and_then(|(mode, role)| mode.action(role));
    anyhow::ensure!(
        allowed == Some(change),
        "Это действие с участником недоступно"
    );
    changes.moderation.insert((chat, member), change);
    if change == MemberChange::Ban {
        changes
            .restrictions
            .insert((chat, member), MemberRestriction::default());
    }
    Ok(())
}

pub fn member_count(chat: i64, changes: &MemberChanges) -> u32 {
    member_ids(chat, changes)
        .filter_map(|id| demo_member_role(chat, id, changes))
        .filter(|role| role.in_chat())
        .count() as u32
}

pub fn members(
    chat: i64,
    changes: &MemberChanges,
    filter: MemberFilter,
    offset: i32,
) -> MemberPage {
    let contact_names = contacts();
    let members: Vec<_> = member_ids(chat, changes)
        .filter_map(|id| Some((id, demo_member_role(chat, id, changes)?)))
        .map(|(id, role)| ChatMember {
            id,
            name: match id {
                1 => "Алексей".into(),
                2 => "Анна".into(),
                3 => "Бот проекта".into(),
                _ => changes
                    .invited
                    .get(&(chat, id))
                    .or_else(|| contact_names.iter().find(|contact| contact.id == id))
                    .map_or_else(|| format!("Участник {id}"), |contact| contact.name.clone()),
            },
            avatar: (id <= 3).then_some(OWN_AVATAR),
            role,
            bot: id == 3,
            can_update: true,
            admin: demo_admin_state(chat, id, changes),
        })
        .filter(|member| filter.includes(member))
        .collect();
    member_page(members, offset)
}

fn member_ids(chat: i64, changes: &MemberChanges) -> impl Iterator<Item = i64> {
    let invited: Vec<_> = changes
        .invited
        .keys()
        .filter_map(|(target, id)| (*target == chat && *id > 47).then_some(*id))
        .collect();
    (1..=47).chain(invited)
}

fn member_page(mut members: Vec<ChatMember>, offset: i32) -> MemberPage {
    members.sort_by_key(|member| member.id);
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

fn forum_topic(id: i32) -> Option<ForumTopic> {
    let title = match id {
        GENERAL_TOPIC => "Общая".into(),
        100 => "Инфраструктура".into(),
        200 => "Обсуждение макетов".into(),
        300 => "Завершённое обсуждение".into(),
        301..=342 => format!("Архивная тема {id}"),
        _ => return None,
    };
    Some(ForumTopic {
        id,
        title,
        unread: match id {
            100 => 2,
            200 => 1,
            _ => 0,
        },
        closed: id == 300,
        hidden: false,
        pinned: id == GENERAL_TOPIC,
        mine: id == 200,
    })
}

const FORUM_PERMISSIONS: ForumPermissions = ForumPermissions {
    create: true,
    manage: true,
    delete_messages: true,
};

#[derive(Default)]
pub struct ForumTopics {
    seeded: bool,
    edits: HashMap<i32, Option<ForumTopic>>,
    created: HashMap<i64, i32>,
}

impl ForumTopics {
    pub fn for_chat(chat: i64) -> Self {
        Self {
            seeded: chat == FORUM_CHAT,
            ..Self::default()
        }
    }

    pub fn topic(&self, id: i32) -> Option<ForumTopic> {
        self.edits.get(&id).cloned().unwrap_or_else(|| {
            if self.seeded || id == GENERAL_TOPIC {
                let mut topic = forum_topic(id)?;
                topic.pinned &= self.seeded;
                Some(topic)
            } else {
                None
            }
        })
    }

    pub fn page(&self, cursor: ForumCursor) -> ForumPage {
        let mut all: Vec<_> = [GENERAL_TOPIC, 100, 200, 300]
            .into_iter()
            .chain(301..=342)
            .chain(self.created.values().copied())
            .filter_map(|id| self.topic(id))
            .collect();
        all.sort_by_key(|topic| {
            (
                !topic.pinned,
                if topic.id >= 1000 {
                    -topic.id
                } else {
                    topic.id
                },
            )
        });
        let start = all
            .iter()
            .position(|topic| topic.id == cursor.topic)
            .map_or(0, |index| index + 1);
        let end = (start + FORUM_PAGE_SIZE).min(all.len());
        let next = (end < all.len()).then(|| ForumCursor {
            topic: all[end - 1].id,
            ..ForumCursor::default()
        });
        ForumPage {
            topics: all[start..end].to_vec(),
            next,
            permissions: Some(FORUM_PERMISSIONS),
        }
    }

    pub fn write(&mut self, write: ForumWrite) -> anyhow::Result<ForumWriteResult> {
        match write {
            ForumWrite::Create { title, random_id } => {
                if let Some(&id) = self.created.get(&random_id) {
                    return Ok(self.topic(id).map_or(
                        ForumWriteResult::CreationUnconfirmed,
                        ForumWriteResult::Created,
                    ));
                }
                let title = forum_topic_title(&title)?;
                let id = 1000 + i32::try_from(self.created.len())?;
                let topic = ForumTopic {
                    id,
                    title,
                    unread: 0,
                    closed: false,
                    hidden: false,
                    pinned: false,
                    mine: true,
                };
                self.created.insert(random_id, id);
                self.edits.insert(id, Some(topic.clone()));
                Ok(ForumWriteResult::Created(topic))
            }
            ForumWrite::Change { original, change } => {
                let id = original.id;
                let mut topic = match self.topic(id) {
                    Some(topic) => topic,
                    None if id > GENERAL_TOPIC && matches!(change, ForumTopicChange::Delete) => {
                        return Ok(ForumWriteResult::Deleted(id));
                    }
                    None => anyhow::bail!("Тема больше недоступна"),
                };
                anyhow::ensure!(
                    FORUM_PERMISSIONS.allows(&topic, &change),
                    "Нет права менять тему"
                );
                if !change.needs_write(&original, &topic)? {
                    return Ok(ForumWriteResult::Updated(topic));
                }
                match change {
                    ForumTopicChange::Rename(title) => topic.title = forum_topic_title(&title)?,
                    ForumTopicChange::Closed(closed) => {
                        topic.closed = closed;
                        if !closed {
                            topic.hidden = false;
                        }
                    }
                    ForumTopicChange::Hidden(hidden) => {
                        topic.hidden = hidden;
                        topic.closed |= hidden;
                    }
                    ForumTopicChange::Pinned(pinned) => topic.pinned = pinned,
                    ForumTopicChange::Delete => {
                        self.edits.insert(id, None);
                        return Ok(ForumWriteResult::Deleted(id));
                    }
                }
                self.edits.insert(id, Some(topic.clone()));
                Ok(ForumWriteResult::Updated(topic))
            }
        }
    }
}

pub fn history(chat: i64) -> Vec<Message> {
    if chat == FORUM_CHAT {
        let template = poll_message(0, PollDraft::default());
        let text_message = |id, topic, text| Message {
            id,
            topic: Some(topic),
            text,
            poll: None,
            sticker: None,
            self_authored: false,
            outgoing: false,
            can_edit: false,
            can_delete: false,
            can_close_poll: false,
            sender: "Анна".into(),
            sender_avatar: None,
            ..template.clone()
        };
        let mut messages = vec![text_message(
            20,
            GENERAL_TOPIC,
            "Общий разговор форума".into(),
        )];
        for (topic, title, count) in [
            (100, "Инфраструктура", 65),
            (200, "Макеты", 5),
            (300, "Завершённая тема", 1),
        ] {
            messages.extend((1..=count).map(|index| {
                text_message(topic + index, topic, format!("{title} · запись {index}"))
            }));
        }
        return messages;
    }
    if chat == 6 {
        let mut meeting = poll_message(
            2,
            PollDraft {
                deadline: None,
                hide_results_until_close: false,
                question: "Когда провести встречу?".into(),
                open_answers: false,
                choices: vec!["В понедельник".into(), "В среду".into(), "В пятницу".into()],
                multiple: true,
                shuffle_answers: true,
                subscribers_only: true,
                ..PollDraft::default()
            },
        );
        let mut quiz = poll_message(SAMPLE_QUIZ_ID, sample_quiz());
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
            message.can_close_poll = false;
        }
        let mut poll = PollDraft {
            question: "Когда открыть обсуждение проекта?".into(),
            choices: vec![
                "В четверг".into(),
                "В пятницу".into(),
                "После выходных".into(),
            ],
            multiple: true,
            ..PollDraft::default()
        }
        .into_poll(i64::from(PUBLIC_POLL_ID));
        poll.public = true;
        poll.creator = false;
        poll.total = Some(60);
        for (choice, count) in poll.choices.iter_mut().zip([40, 30, 0]) {
            choice.votes = Some(count);
        }
        let public = Message {
            id: PUBLIC_POLL_ID,
            poll: Some(poll),
            ..meeting.clone()
        };
        return vec![quiz, meeting, public];
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
        MODERATION_GROUP => vec![("Анна", "Разрешения и ограничения участников", false, None)],
        SLOW_MODE_GROUP => vec![(
            "Анна",
            "Интервал между сообщениями — 10 секунд",
            false,
            None,
        )],
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
            topic: None,
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
            sticker: None,
            reply_to: (chat == 1 && index == 1).then_some(81),
            reply_preview: None,
            can_reply: true,
            can_edit: outgoing && attachment.is_none(),
            can_delete: outgoing || chat == 2,
            can_close_poll: false,
            can_forward: true,
            forwarded: false,
            pinned: false,
            reactions: Vec::new(),
            edited: false,
            ephemeral: false,
            album: None,
        })
        .collect();
    if chat == MODERATION_GROUP
        && let Some(sample) = messages.first()
    {
        let mut poll = PollDraft {
            question: "Как назвать раздел для правил?".into(),
            choices: vec!["Правила группы".into(), "Памятка участника".into()],
            ..PollDraft::default()
        }
        .into_poll(46);
        poll.creator = false;
        poll.hide_results_until_close = true;
        poll.closes_at = Some(now() + 90);
        poll.total = Some(2);
        for choice in &mut poll.choices {
            choice.votes = Some(1);
        }
        messages.insert(
            0,
            Message {
                id: 46,
                poll: Some(poll),
                text: String::new(),
                ..sample.clone()
            },
        );
    }
    if chat == 1 {
        let sample = messages[0].clone();
        let day = 24 * 60 * 60;
        let older = (1..=80).map(|id| {
            let mut message = Message {
                id,
                topic: None,
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
        let mut sticker = sticker_message(
            85,
            crate::model::Sticker {
                favorite: false,
                id: 1,
                emoji: "🐱".into(),
                kind: crate::model::StickerKind::Static,
            },
        );
        sticker.timestamp = today - 10;
        messages.push(sticker);
        let mut video = sticker_message(
            86,
            crate::model::Sticker {
                favorite: false,
                id: 3,
                emoji: "✨".into(),
                kind: crate::model::StickerKind::Video,
            },
        );
        video.timestamp = today - 5;
        messages.push(video);
        let mut animated = sticker_message(
            87,
            crate::model::Sticker {
                favorite: false,
                id: 2,
                emoji: "🚀".into(),
                kind: crate::model::StickerKind::Animated,
            },
        );
        animated.timestamp = today - 2;
        messages.push(animated);
    }
    if chat == 3 {
        let mut post = poll_message(
            42,
            PollDraft {
                question: "Какой выпуск подготовить первым?".into(),
                choices: vec!["Итоги недели".into(), "Ответы на вопросы".into()],
                ..PollDraft::default()
            },
        );
        post.sender = "Новости проекта".into();
        post.sender_avatar = Some(Avatar {
            peer: 3,
            photo: 3,
            dc: 0,
        });
        post.self_authored = false;
        post.outgoing = false;
        post.can_delete = false;
        messages.insert(0, post);
    }
    messages
}

pub fn sample_quiz() -> PollDraft {
    PollDraft {
        question: "Какой день идёт после вторника?".into(),
        choices: vec!["Среда".into(), "Воскресенье".into()],
        quiz: Some(crate::model::PollQuizDraft {
            correct: vec![0],
            solution: "Первый вариант правильный.".into(),
        }),
        ..PollDraft::default()
    }
}

pub fn poll_message(id: i32, draft: PollDraft) -> Message {
    let timestamp = now();
    let (deadline, hidden) = (draft.deadline, draft.hide_results_until_close);
    let mut poll = draft.into_poll(i64::from(id));
    poll.closes_at = deadline.map(|deadline| deadline.closes_at(timestamp));
    poll.hide_results_until_close = hidden;
    Message {
        id,
        topic: None,
        sender: OWN_NAME.into(),
        sender_avatar: Some(OWN_AVATAR),
        self_authored: true,
        timestamp,
        text: String::new(),
        styles: Vec::new(),
        links: Vec::new(),
        outgoing: true,
        attachment: None,
        photo_preview: false,
        playback: None,
        audio: None,
        thumbnail: None,
        poll: Some(poll),
        sticker: None,
        reply_to: None,
        reply_preview: None,
        can_reply: true,
        can_edit: false,
        can_delete: true,
        can_close_poll: true,
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
    crate::model::unix_now()
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
    playback_bytes(media, progress, whole, bytes).await
}

pub fn sticker_video_media() -> PlayableMedia {
    PlayableMedia::Video {
        width: 512,
        height: 512,
        duration: Some(2),
        size: include_bytes!("../data/demo-sticker-video.webm").len() as u64,
    }
}

pub async fn sticker_video_preview(
    progress: tokio::sync::watch::Sender<PlaybackProgress>,
    whole: bool,
) -> anyhow::Result<PlaybackSource> {
    playback_bytes(
        sticker_video_media(),
        progress,
        whole,
        include_bytes!("../data/demo-sticker-video.webm"),
    )
    .await
}

async fn playback_bytes(
    media: PlayableMedia,
    progress: tokio::sync::watch::Sender<PlaybackProgress>,
    whole: bool,
    bytes: &'static [u8],
) -> anyhow::Result<PlaybackSource> {
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

pub fn sticker_message(id: i32, sticker: crate::model::Sticker) -> Message {
    let mut message = poll_message(id, PollDraft::default());
    message.poll = None;
    message.attachment = match sticker.kind {
        crate::model::StickerKind::Static => Some(format!("sticker-{id}.webp")),
        crate::model::StickerKind::Video => Some(format!("sticker-{id}.webm")),
        crate::model::StickerKind::Animated => Some(format!("sticker-{id}.tgs")),
    };
    message.playback = (sticker.kind == crate::model::StickerKind::Video).then(sticker_video_media);
    message.sticker = Some(sticker);
    message
}

pub struct StickerPacks {
    names: Vec<String>,
    installed: HashSet<i64>,
}

impl Default for StickerPacks {
    fn default() -> Self {
        Self {
            names: vec!["SpaceCat".into()],
            installed: HashSet::from([1]),
        }
    }
}

impl StickerPacks {
    fn collection(&self, id: i64) -> anyhow::Result<crate::model::StickerCollection> {
        use crate::model::{StickerCollection, StickerSource};
        let index = usize::try_from(
            id.checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("Набор недоступен"))?,
        )?;
        let name = self
            .names
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("Набор недоступен"))?;
        Ok(StickerCollection {
            source: StickerSource::Set(id),
            title: if id == 1 {
                "Демо — космический кот".into()
            } else {
                format!("Демо · {name}")
            },
            installed: Some(self.installed.contains(&id)),
        })
    }

    pub fn collections(&self) -> Vec<crate::model::StickerCollection> {
        use crate::model::{StickerCollection, StickerSource};
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
        collections.extend(
            (1..=self.names.len() as i64)
                .filter(|id| self.installed.contains(id))
                .filter_map(|id| self.collection(id).ok()),
        );
        collections
    }

    pub fn open(&mut self, input: &str) -> anyhow::Result<crate::model::StickerCollection> {
        let name = crate::model::sticker_pack_name(input)?;
        let id = self
            .names
            .iter()
            .position(|known| known.eq_ignore_ascii_case(&name))
            .map_or_else(
                || {
                    self.names.push(name);
                    self.names.len() as i64
                },
                |index| index as i64 + 1,
            );
        self.collection(id)
    }

    pub fn change(
        &mut self,
        id: i64,
        installed: bool,
    ) -> anyhow::Result<crate::model::StickerPackChange> {
        self.collection(id)?;
        if installed {
            self.installed.insert(id);
        } else {
            self.installed.remove(&id);
        }
        Ok(crate::model::StickerPackChange {
            collection: self.collection(id)?,
            archived: Some(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticker_packs_keep_preview_identity_and_installation_separate_per_account() {
        use crate::model::StickerSource;
        let mut first = StickerPacks::default();
        let second = StickerPacks::default();
        let preview = first.open("https://t.me/addstickers/SpaceDogs").unwrap();
        let StickerSource::Set(id) = preview.source else {
            panic!()
        };
        assert_eq!(preview.installed, Some(false));
        assert_eq!(first.collections().len(), 3);
        first.change(id, true).unwrap();
        assert_eq!(first.collections().len(), 4);
        assert_eq!(second.collections().len(), 3);
        assert_eq!(first.open("spacedogs").unwrap().source, preview.source);
        first.change(id, false).unwrap();
        assert_eq!(first.collections().len(), 3);
        assert_eq!(first.open("SpaceDogs").unwrap().source, preview.source);
        assert!(first.change(999, true).is_err());
        assert!(first.change(i64::MIN, true).is_err());
    }

    #[test]
    fn join_request_pages_and_decision_retries_stay_in_their_chat() {
        let mut queue = JoinRequests::default();
        let first = queue.page(1, None);
        assert_eq!(first.requests.len(), 25);
        let second = queue.page(1, first.next);
        assert_eq!(second.requests.len(), 5);
        assert!(second.next.is_none());
        let user = first.requests[0].cursor.user;
        assert_eq!(queue.decide(1, user), JoinDecisionOutcome::Applied);
        assert_eq!(queue.decide(1, user), JoinDecisionOutcome::AlreadyHandled);
        assert_eq!(queue.page(1, None).total, 29);
        assert_eq!(queue.page(3, None).total, 30);
        assert_eq!(queue.page(3, None).requests[0].cursor.user, user);
    }
}

pub fn poll_voter_page(
    poll: &crate::model::Poll,
    option: Option<&[u8]>,
    offset: Option<&str>,
) -> crate::model::PollVoterPage {
    use crate::model::{POLL_VOTERS_PAGE_SIZE, PollVoter, PollVoterPage};

    let mut votes = Vec::new();
    if poll.has_vote() {
        votes.push((1, OWN_NAME.to_owned(), Some(OWN_AVATAR), {
            poll.choices
                .iter()
                .filter(|choice| choice.chosen)
                .map(|choice| choice.option.as_slice())
                .collect::<Vec<_>>()
        }));
    }
    if poll.id == i64::from(PUBLIC_POLL_ID) {
        for index in 1..=60 {
            let mut answers = Vec::new();
            if index <= 40 {
                answers.push(&[0][..]);
            }
            if index > 40 || index <= 10 {
                answers.push(&[1][..]);
            }
            votes.push((
                10_000 + index,
                format!("Участник {index:02}"),
                None,
                answers,
            ));
        }
    }
    votes.retain(|(_, _, _, options)| option.is_none_or(|option| options.contains(&option)));
    let total = votes.len();
    let start = offset
        .and_then(|offset| offset.parse::<usize>().ok())
        .unwrap_or_default();
    let voters: Vec<_> = votes
        .into_iter()
        .skip(start)
        .take(POLL_VOTERS_PAGE_SIZE as usize)
        .map(|(peer, name, avatar, options)| PollVoter {
            peer,
            name,
            avatar,
            answers: poll
                .choices
                .iter()
                .filter(|choice| options.contains(&choice.option.as_slice()))
                .map(|choice| choice.label.clone())
                .collect(),
        })
        .collect();
    let next = start.saturating_add(voters.len());
    PollVoterPage {
        voters,
        total: total as i32,
        next: (next < total).then(|| next.to_string()),
    }
}

#[cfg(test)]
mod poll_voter_tests {
    use super::*;

    #[test]
    fn demo_voters_paginate_each_option_independently() {
        let message = history(6)
            .into_iter()
            .find(|message| message.id == PUBLIC_POLL_ID)
            .expect("public demo poll");
        let mut poll = message.poll.expect("public demo poll");
        assert!(!poll.can_list_voters());
        poll.choices[0].chosen = true;
        assert!(poll.can_list_voters());

        let first = poll_voter_page(&poll, None, None);
        let second = poll_voter_page(&poll, None, first.next.as_deref());
        let last = poll_voter_page(&poll, None, second.next.as_deref());
        assert_eq!(
            (first.voters.len(), second.voters.len(), last.voters.len()),
            (24, 24, 13)
        );
        assert_eq!(first.total, 61);
        assert_eq!(first.next.as_deref(), Some("24"));
        assert!(last.next.is_none());
        assert!(
            first
                .voters
                .iter()
                .all(|voter| second.voters.iter().all(|other| voter.peer != other.peer))
        );

        let friday = poll_voter_page(&poll, Some(&[1]), None);
        assert_eq!(friday.total, 30);
        assert!(
            friday
                .voters
                .iter()
                .all(|voter| voter.answers.iter().any(|answer| answer == "В пятницу"))
        );
        let empty = poll_voter_page(&poll, Some(&[2]), None);
        assert!(empty.voters.is_empty() && empty.next.is_none());
    }
}
