use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::contact_picker::{ContactPicker, LoadContacts, caption};
use crate::model::{
    ChatInfo, CreatedChat, NewChatDraft, NewChatKind, NewChatResult, chat_username,
};

type Create = Pin<Box<dyn Future<Output = NewChatResult> + Send>>;
type Publish = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

pub struct CreateContext {
    pub runtime: Handle,
    pub contacts: LoadContacts,
    pub create: Box<dyn Fn(NewChatDraft) -> Create>,
    pub publish: Box<dyn Fn(i64, String) -> Publish>,
    pub opened: Box<dyn Fn(CreatedChat)>,
    pub reconcile: Box<dyn Fn()>,
}

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
    let forum = adw::SwitchRow::builder()
        .title("Темы")
        .visible(false)
        .build();
    details.add(&forum);
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

    let CreateContext {
        runtime,
        contacts,
        create,
        publish,
        opened,
        reconcile,
    } = context;
    let picker = ContactPicker::new(runtime.clone(), contacts);
    form.append(&picker.widget);
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
        runtime,
        create,
        publish,
        opened,
        reconcile,
        picker,
        dialog: dialog.downgrade(),
        form,
        kind,
        name,
        about,
        username,
        forum,
        privacy_hint,
        submit,
        status,
        status_scroll,
        retry_group,
        retry_name,
        busy: Cell::new(false),
        closed: Cell::new(false),
        unconfirmed: Cell::new(false),
        created: RefCell::new(None),
    });

    state.kind.connect_selected_notify(glib::clone!(
        #[weak]
        state,
        move |_| state.update_type()
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
            state.picker.cancel_load();
            if let Some(mut created) = state.created.take() {
                if created.public_error.is_some() {
                    created.draft.username.clear();
                }
                (state.opened)(created);
            }
        }
    });

    state.update_type();
    dialog.set_focus(Some(&state.name));
    dialog.present(Some(window));
    dialog
}

struct Form {
    runtime: Handle,
    create: Box<dyn Fn(NewChatDraft) -> Create>,
    publish: Box<dyn Fn(i64, String) -> Publish>,
    opened: Box<dyn Fn(CreatedChat)>,
    reconcile: Box<dyn Fn()>,
    picker: Rc<ContactPicker>,
    dialog: glib::WeakRef<adw::Dialog>,
    form: gtk::Box,
    kind: gtk::DropDown,
    name: adw::EntryRow,
    about: gtk::TextView,
    username: adw::EntryRow,
    forum: adw::SwitchRow,
    privacy_hint: gtk::Label,
    submit: gtk::Button,
    status: gtk::Label,
    status_scroll: gtk::ScrolledWindow,
    retry_group: adw::PreferencesGroup,
    retry_name: adw::EntryRow,
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
        self.forum
            .set_visible(self.kind() == NewChatKind::Supergroup);
        self.privacy_hint.set_visible(public);
        let hint = if public {
            "Участники (необязательно)"
        } else {
            "Выберите хотя бы одного участника"
        };
        self.picker
            .configure(hint, self.kind() != NewChatKind::Channel);
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
            forum: self.kind() == NewChatKind::Supergroup && self.forum.is_active(),
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
            members: self.picker.chosen(),
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
        let task = self.runtime.spawn((self.create)(draft));
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
                    (state.reconcile)();
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
        let username = match chat_username(&self.retry_name.text()) {
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
        let task = self.runtime.spawn((self.publish)(chat, username.clone()));
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
