use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::{runtime::Handle, task::AbortHandle};

use crate::model::{
    ChatInfo, ChatKind, Contact, CreatedChat, NewChatDraft, NewChatKind, NewChatResult,
    new_chat_username,
};

type LoadContacts = Pin<Box<dyn Future<Output = Result<Arc<Vec<Contact>>>> + Send>>;
type Create = Pin<Box<dyn Future<Output = NewChatResult> + Send>>;
type Publish = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

pub struct CreateContext {
    pub runtime: Handle,
    pub contacts: Box<dyn Fn() -> LoadContacts>,
    pub create: Box<dyn Fn(NewChatDraft) -> Create>,
    pub publish: Box<dyn Fn(i64, String) -> Publish>,
    pub opened: Box<dyn Fn(CreatedChat)>,
    pub reconcile: Box<dyn Fn()>,
}

const MEMBERS_PAGE: usize = 30;

pub fn open(window: &adw::ApplicationWindow, context: CreateContext) -> adw::Dialog {
    let (dialog, toolbar, body) = crate::chat_management::dialog_with_toolbar("Создать чат");
    let form = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let details = adw::PreferencesGroup::new();
    let kind = gtk::DropDown::from_strings(&["Обычная группа", "Супергруппа", "Канал"]);
    kind.set_valign(gtk::Align::Center);
    kind.update_property(&[gtk::accessible::Property::Label("Тип чата")]);
    let kind_row = adw::ActionRow::builder().title("Тип чата").build();
    kind_row.add_suffix(&kind);
    details.add(&kind_row);
    let name = adw::EntryRow::builder().title("Название").build();
    name.set_use_markup(false);
    details.add(&name);
    let username = adw::EntryRow::builder().title("Публичное имя").build();
    details.add(&username);
    form.append(&details);
    let privacy_hint = caption();
    privacy_hint.set_text("Оставьте публичное имя пустым для закрытого чата.");
    form.append(&privacy_hint);
    form.append(&gtk::Label::builder().label("Описание").xalign(0.0).build());
    let about = gtk::TextView::builder()
        .height_request(72)
        .accepts_tab(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .css_classes(["card"])
        .build();
    about.update_property(&[gtk::accessible::Property::Label("Описание")]);
    form.append(&about);

    let members_hint = caption();
    form.append(&members_hint);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Найти участника")
        .build();
    search.update_property(&[gtk::accessible::Property::Label("Найти участника")]);
    form.append(&search);
    let people = adw::PreferencesGroup::new();
    form.append(&people);
    let list_status = caption();
    form.append(&list_status);
    let more = gtk::Button::with_label("Ещё контакты");
    more.set_halign(gtk::Align::Start);
    more.set_visible(false);
    form.append(&more);
    let reload = gtk::Button::with_label("Повторить загрузку контактов");
    reload.set_halign(gtk::Align::Start);
    reload.set_visible(false);
    form.append(&reload);
    body.append(&form);

    let actions = gtk::Box::new(gtk::Orientation::Vertical, 8);
    actions.set_margin_start(16);
    actions.set_margin_end(16);
    actions.set_margin_top(8);
    actions.set_margin_bottom(12);
    let status = caption();
    let status_scroll = gtk::ScrolledWindow::builder()
        .child(&status)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(132)
        .propagate_natural_height(true)
        .visible(false)
        .build();
    actions.append(&status_scroll);
    let retry_group = adw::PreferencesGroup::new();
    retry_group.set_visible(false);
    let retry_name = adw::EntryRow::builder()
        .title("Новое публичное имя")
        .build();
    retry_group.add(&retry_name);
    let retry_public = gtk::Button::with_label("Сохранить публичное имя");
    retry_group.add(&retry_public);
    actions.append(&retry_group);
    let submit = gtk::Button::with_label("Создать");
    submit.add_css_class("suggested-action");
    submit.set_halign(gtk::Align::End);
    actions.append(&submit);
    toolbar.add_bottom_bar(&actions);

    let state = Rc::new(Form {
        context,
        dialog: dialog.downgrade(),
        form,
        kind,
        name,
        about,
        username,
        privacy_hint,
        members_hint,
        search,
        people,
        list_status,
        more,
        reload,
        submit,
        status,
        status_scroll,
        retry_group,
        retry_name,
        contacts: RefCell::new(None),
        selected: RefCell::new(HashSet::new()),
        rendered: RefCell::new(Vec::new()),
        visible_limit: Cell::new(MEMBERS_PAGE),
        load_task: RefCell::new(None),
        busy: Cell::new(false),
        closed: Cell::new(false),
        unconfirmed: Cell::new(false),
        created: RefCell::new(None),
    });

    state.kind.connect_selected_notify(glib::clone!(
        #[weak]
        state,
        move |_| {
            state.update_type();
            state.render_contacts();
        }
    ));
    state.search.connect_search_changed(glib::clone!(
        #[weak]
        state,
        move |_| {
            state.visible_limit.set(MEMBERS_PAGE);
            state.render_contacts();
        }
    ));
    state.more.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| {
            state
                .visible_limit
                .set(state.visible_limit.get().saturating_add(MEMBERS_PAGE));
            state.render_contacts();
        }
    ));
    state.reload.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.load_contacts()
    ));
    state.submit.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.submit()
    ));
    retry_public.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.publish()
    ));
    dialog.connect_closed({
        let state = state.clone();
        move |_| {
            state.closed.set(true);
            if let Some(task) = state.load_task.take() {
                task.abort();
            }
            if let Some(mut created) = state.created.take() {
                if created.public_error.is_some() {
                    created.draft.username.clear();
                }
                (state.context.opened)(created);
            }
        }
    });

    state.update_type();
    state.load_contacts();
    dialog.set_focus(Some(&state.name));
    dialog.present(Some(window));
    dialog
}

struct Form {
    context: CreateContext,
    dialog: glib::WeakRef<adw::Dialog>,
    form: gtk::Box,
    kind: gtk::DropDown,
    name: adw::EntryRow,
    about: gtk::TextView,
    username: adw::EntryRow,
    privacy_hint: gtk::Label,
    members_hint: gtk::Label,
    search: gtk::SearchEntry,
    people: adw::PreferencesGroup,
    list_status: gtk::Label,
    more: gtk::Button,
    reload: gtk::Button,
    submit: gtk::Button,
    status: gtk::Label,
    status_scroll: gtk::ScrolledWindow,
    retry_group: adw::PreferencesGroup,
    retry_name: adw::EntryRow,
    contacts: RefCell<Option<Arc<Vec<Contact>>>>,
    selected: RefCell<HashSet<i64>>,
    rendered: RefCell<Vec<gtk::CheckButton>>,
    visible_limit: Cell<usize>,
    load_task: RefCell<Option<AbortHandle>>,
    busy: Cell<bool>,
    closed: Cell<bool>,
    unconfirmed: Cell<bool>,
    created: RefCell<Option<CreatedChat>>,
}

impl Form {
    fn kind(&self) -> NewChatKind {
        match self.kind.selected() {
            1 => NewChatKind::Supergroup,
            2 => NewChatKind::Channel,
            _ => NewChatKind::Group,
        }
    }

    fn update_type(&self) {
        let public = self.kind() != NewChatKind::Group;
        self.username.set_visible(public);
        self.privacy_hint.set_visible(public);
        self.update_members_hint();
    }

    fn eligible(&self, contact: &Contact) -> bool {
        !contact.is_self
            && contact.available
            && (self.kind() != NewChatKind::Channel || contact.kind != ChatKind::Bot)
    }

    fn chosen(&self) -> Vec<Contact> {
        self.contacts
            .borrow()
            .iter()
            .flat_map(|contacts| contacts.iter())
            .filter(|contact| {
                self.eligible(contact) && self.selected.borrow().contains(&contact.id)
            })
            .cloned()
            .collect()
    }

    fn update_members_hint(&self) {
        let count = self.chosen().len();
        let hint = if self.kind() == NewChatKind::Group {
            "Выберите хотя бы одного участника"
        } else {
            "Участники (необязательно)"
        };
        self.members_hint.set_text(&format!("{hint} · {count}"));
    }

    fn load_contacts(self: &Rc<Self>) {
        self.reload.set_visible(false);
        self.list_status.set_text("Загрузка контактов…");
        let request = (self.context.contacts)();
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(15), request)
                .await
                .context("Telegram не прислал контакты вовремя")?
        });
        self.load_task.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(state) = weak.upgrade().filter(|state| !state.closed.get()) else {
                return;
            };
            state.load_task.take();
            match result {
                Ok(contacts) => {
                    state.contacts.replace(Some(contacts));
                    state.render_contacts();
                }
                Err(error) => {
                    state
                        .list_status
                        .set_text(&format!("Не удалось загрузить контакты. {error}"));
                    state.reload.set_visible(true);
                }
            }
        });
    }

    fn render_contacts(self: &Rc<Self>) {
        let contacts = self.contacts.borrow();
        let Some(contacts) = contacts.as_ref() else {
            return;
        };
        for row in self.rendered.take() {
            self.people.remove(&row);
        }
        let query = self.search.text();
        let limit = self.visible_limit.get();
        let mut matching = contacts
            .iter()
            .filter(|contact| self.eligible(contact) && contact.matches(&query));
        for contact in matching.by_ref().take(limit) {
            let check = gtk::CheckButton::with_label(&contact.name);
            if let Some(label) = check.child().and_downcast::<gtk::Label>() {
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            }
            check.set_active(self.selected.borrow().contains(&contact.id));
            let id = contact.id;
            check.connect_toggled(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |check| {
                    if check.is_active() {
                        state.selected.borrow_mut().insert(id);
                    } else {
                        state.selected.borrow_mut().remove(&id);
                    }
                    state.update_members_hint();
                }
            ));
            self.people.add(&check);
            self.rendered.borrow_mut().push(check);
        }
        self.more.set_visible(matching.next().is_some());
        self.list_status
            .set_text(if self.rendered.borrow().is_empty() {
                "Контактов не найдено."
            } else {
                ""
            });
        self.update_members_hint();
    }

    fn set_status(&self, text: &str) {
        self.status.set_text(text);
        self.status_scroll.set_visible(!text.is_empty());
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.set_can_close(!busy);
        }
        self.form
            .set_sensitive(!busy && !self.unconfirmed.get() && self.created.borrow().is_none());
        self.retry_group.set_sensitive(!busy);
        self.submit.set_sensitive(!busy);
    }

    fn submit(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        if self.unconfirmed.get() || self.created.borrow().is_some() {
            if let Some(dialog) = self.dialog.upgrade() {
                dialog.close();
            }
            return;
        }
        let about = self.about.buffer();
        let draft = NewChatDraft {
            kind: self.kind(),
            info: ChatInfo {
                title: self.name.text().into(),
                about: about
                    .text(&about.start_iter(), &about.end_iter(), false)
                    .into(),
            },
            username: if self.kind() == NewChatKind::Group {
                String::new()
            } else {
                self.username.text().into()
            },
            members: self.chosen(),
        };
        let draft = match draft.validate() {
            Ok(draft) => draft,
            Err(error) => {
                self.set_status(&error.to_string());
                return;
            }
        };
        self.set_status("Создание чата…");
        self.set_busy(true);
        let task = self.context.runtime.spawn((self.context.create)(draft));
        let state = self.clone();
        glib::spawn_future_local(async move {
            let result = task
                .await
                .unwrap_or_else(|error| NewChatResult::Unconfirmed(error.to_string()));
            if state.closed.get() {
                return;
            }
            state.set_busy(false);
            match result {
                NewChatResult::Created(created) => {
                    let complete = created.warnings.is_empty() && created.public_error.is_none();
                    state.retry_name.set_text(&created.draft.username);
                    state.created.replace(Some(*created));
                    if complete && let Some(dialog) = state.dialog.upgrade() {
                        dialog.close();
                        return;
                    }
                    state.form.set_visible(false);
                    state.submit.set_label("Открыть чат");
                    state.submit.grab_focus();
                    state.show_created();
                }
                NewChatResult::Rejected(error) => state.set_status(&error),
                NewChatResult::Unconfirmed(error) => {
                    (state.context.reconcile)();
                    state.unconfirmed.set(true);
                    state.form.set_sensitive(false);
                    state.submit.set_label("К списку чатов");
                    state.submit.grab_focus();
                    state.set_status(&format!("Не удалось подтвердить создание. {error}\nПроверьте список чатов перед повторной попыткой."));
                }
            }
        });
    }

    fn show_created(&self) {
        let created = self.created.borrow();
        let Some(created) = created.as_ref() else {
            return;
        };
        let mut status = vec!["Чат создан.".to_owned()];
        status.extend(created.warnings.iter().cloned());
        status.extend(created.public_error.iter().cloned());
        self.set_status(&status.join("\n"));
        self.retry_group.set_visible(created.public_error.is_some());
    }

    fn publish(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let Some(chat) = self
            .created
            .borrow()
            .as_ref()
            .map(|created| created.chat.id)
        else {
            return;
        };
        let username = match new_chat_username(&self.retry_name.text()) {
            Ok(username) if !username.is_empty() => username,
            Ok(_) => {
                self.set_status("Введите публичное имя.");
                return;
            }
            Err(error) => {
                self.set_status(&error.to_string());
                return;
            }
        };
        self.set_status("Сохранение публичного имени…");
        self.set_busy(true);
        let task = self
            .context
            .runtime
            .spawn((self.context.publish)(chat, username.clone()));
        let state = self.clone();
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            if state.closed.get() {
                return;
            }
            if let Some(created) = state.created.borrow_mut().as_mut() {
                match result {
                    Ok(()) => {
                        created.draft.username = username;
                        created.public_error = None;
                    }
                    Err(error) => created.public_error = Some(error.to_string()),
                }
            }
            state.set_busy(false);
            state.show_created();
        });
    }
}

fn caption() -> gtk::Label {
    gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["dim-label"])
        .build()
}
