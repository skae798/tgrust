use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::{
    InviteAction, InviteChanges, InviteCursor, InviteDraft, InviteEdit, InviteLink, InvitePage,
};

type PageFuture = Pin<Box<dyn Future<Output = Result<InvitePage>> + Send>>;
type WriteFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

pub struct InviteContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn(bool, Option<InviteCursor>) -> PageFuture>,
    pub write: Box<dyn Fn(InviteAction) -> WriteFuture>,
}

struct InviteFields {
    name: adw::EntryRow,
    hours: gtk::DropDown,
    approval: adw::SwitchRow,
    limit: adw::EntryRow,
    editing: bool,
}

fn invite_form(
    original: Option<&InviteLink>,
) -> (adw::PreferencesGroup, InviteFields, gtk::Button) {
    let editing = original.is_some();
    let paid = original.is_some_and(|link| link.paid);
    let name = adw::EntryRow::builder()
        .title("Название (необязательно)")
        .build();
    let duration = adw::ActionRow::builder()
        .title("Срок действия")
        .subtitle_lines(1)
        .build();
    let mut choices = vec!["Без срока", "1 час", "1 день", "1 неделя"];
    if editing {
        choices.insert(0, "Оставить как есть");
    }
    let hours = gtk::DropDown::from_strings(&choices);
    hours.update_property(&[gtk::accessible::Property::Label("Срок действия")]);
    hours.set_valign(gtk::Align::Center);
    duration.add_suffix(&hours);
    let limit = adw::EntryRow::builder()
        .title("Лимит вступлений (необязательно)")
        .input_purpose(gtk::InputPurpose::Digits)
        .build();
    let approval = adw::SwitchRow::builder()
        .title("Одобрение администратора")
        .build();
    approval.connect_active_notify(glib::clone!(
        #[weak]
        limit,
        move |approval| limit.set_visible(!paid && !approval.is_active())
    ));
    if let Some(link) = original {
        name.set_text(&link.title);
        duration.set_subtitle(&expiry_caption(link.expires).unwrap_or_else(|| "Без срока".into()));
        if let Some(value) = link.limit {
            limit.set_text(&value.to_string());
        }
        approval.set_active(link.approval);
    }
    duration.set_visible(!paid);
    approval.set_visible(!paid);
    limit.set_visible(!paid && !approval.is_active());
    let submit_row = adw::ActionRow::new();
    let submit = gtk::Button::builder()
        .label(if editing {
            "Сохранить изменения"
        } else {
            "Создать ссылку"
        })
        .valign(gtk::Align::Center)
        .css_classes(["suggested-action"])
        .build();
    submit_row.add_suffix(&submit);
    let group = adw::PreferencesGroup::new();
    for row in [
        name.upcast_ref::<gtk::Widget>(),
        duration.upcast_ref(),
        approval.upcast_ref(),
        limit.upcast_ref(),
        submit_row.upcast_ref(),
    ] {
        group.add(row);
    }
    (
        group,
        InviteFields {
            name,
            hours,
            approval,
            limit,
            editing,
        },
        submit,
    )
}

impl InviteFields {
    fn draft(&self) -> Result<InviteDraft> {
        let approval = self.approval.is_active();
        let limit = self.limit.text();
        let limit = match limit.trim() {
            "" => None,
            _ if approval => None,
            raw => Some(
                raw.parse::<i32>()
                    .context("Укажите целое число вступлений.")?,
            ),
        };
        let selected = self
            .hours
            .selected()
            .saturating_sub(u32::from(self.editing))
            .min(3);
        Ok(InviteDraft {
            title: self.name.text().into(),
            hours: [None, Some(1), Some(24), Some(168)][selected as usize],
            limit,
            approval,
        })
    }

    fn edit(&self, original: &InviteLink) -> Result<InviteEdit> {
        let draft = self.draft()?;
        let expires = if self.hours.selected() == 0 {
            original.expires
        } else {
            draft.expiration(u64::try_from(unix_now())?)?
        };
        Ok(InviteEdit {
            title: draft.title,
            expires,
            limit: if original.paid {
                original.limit
            } else {
                draft.limit
            },
            approval: draft.approval,
        })
    }

    fn clear(&self) {
        self.name.set_text("");
        self.hours.set_selected(0);
        self.limit.set_text("");
        self.approval.set_active(false);
    }
}

struct InviteEditor {
    dialog: adw::Dialog,
    status: gtk::Label,
}

pub fn open(anchor: &impl IsA<gtk::Widget>, context: InviteContext) {
    let (dialog, body) = crate::chat_management::dialog("Пригласительные ссылки");
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let filter = gtk::DropDown::from_strings(&["Мои ссылки", "Отозванные"]);
    filter.set_hexpand(true);
    filter.update_property(&[gtk::accessible::Property::Label("Фильтр ссылок")]);
    let refresh = icon_button("view-refresh-symbolic", "Обновить ссылки");
    controls.append(&filter);
    controls.append(&refresh);
    body.append(&controls);

    let form = gtk::ToggleButton::with_label("Новая ссылка");
    form.set_halign(gtk::Align::Start);
    body.append(&form);
    let (form_group, create_fields, submit) = invite_form(None);
    form_group.set_visible(false);
    body.append(&form_group);
    let feedback = status_label();
    body.append(&feedback);
    let status = status_label();
    body.append(&status);
    let rows = adw::PreferencesGroup::new();
    body.append(&rows);
    let more = gtk::Button::with_label("Показать ещё");
    more.set_halign(gtk::Align::Center);
    body.append(&more);

    let state = Rc::new(Invites {
        context,
        filter,
        refresh,
        form,
        create_fields,
        form_group,
        feedback,
        status,
        rows,
        more,
        rendered: RefCell::new(Vec::new()),
        links: RefCell::new(HashSet::new()),
        cursors: RefCell::new(HashSet::new()),
        next: RefCell::new(None),
        generation: Cell::new(0),
        read: RefCell::new(None),
        writing: Cell::new(false),
        closed: Cell::new(false),
    });
    state.filter.connect_selected_notify(glib::clone!(
        #[weak]
        state,
        move |_| {
            let creating_allowed = state.filter.selected() == 0;
            if !creating_allowed {
                state.form.set_active(false);
            }
            state.form.set_visible(creating_allowed);
            state.reset();
        }
    ));
    state.form.connect_toggled(glib::clone!(
        #[weak]
        state,
        move |form| {
            state.form_group.set_visible(form.is_active());
            if form.is_active() {
                state.create_fields.name.grab_focus();
            }
        }
    ));
    state.refresh.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| {
            set_status(&state.feedback, "");
            state.reset();
        }
    ));
    state.more.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| {
            let next = state.next.borrow().clone();
            state.load(next);
        }
    ));
    submit.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.create()
    ));
    state.load(None);
    dialog.connect_closed(move |_| {
        state.closed.set(true);
        state.cancel_read();
    });
    dialog.present(Some(anchor));
}

struct Invites {
    context: InviteContext,
    filter: gtk::DropDown,
    refresh: gtk::Button,
    form: gtk::ToggleButton,
    create_fields: InviteFields,
    form_group: adw::PreferencesGroup,
    feedback: gtk::Label,
    status: gtk::Label,
    rows: adw::PreferencesGroup,
    more: gtk::Button,
    rendered: RefCell<Vec<adw::ActionRow>>,
    links: RefCell<HashSet<String>>,
    cursors: RefCell<HashSet<InviteCursor>>,
    next: RefCell<Option<InviteCursor>>,
    generation: Cell<u64>,
    read: RefCell<Option<tokio::task::AbortHandle>>,
    writing: Cell<bool>,
    closed: Cell<bool>,
}

impl Invites {
    fn cancel_read(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(task) = self.read.take() {
            task.abort();
        }
    }

    fn reset(self: &Rc<Self>) {
        self.cancel_read();
        self.next.take();
        self.links.borrow_mut().clear();
        self.cursors.borrow_mut().clear();
        for row in self.rendered.take() {
            self.rows.remove(&row);
        }
        self.load(None);
    }

    fn load(self: &Rc<Self>, after: Option<InviteCursor>) {
        if self.closed.get() || self.read.borrow().is_some() || self.writing.get() {
            return;
        }
        self.more.set_visible(false);
        set_status(&self.status, "Загрузка ссылок…");
        let request = (self.context.load)(self.filter.selected() == 1, after);
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(15), request)
                .await
                .context("Telegram не ответил вовремя")?
        });
        self.read.replace(Some(task.abort_handle()));
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(state) = weak
                .upgrade()
                .filter(|state| state.generation.get() == generation)
            else {
                return;
            };
            state.read.take();
            match result {
                Ok(page) => state.show(page),
                Err(error) => {
                    set_status(
                        &state.status,
                        &format!("Не удалось загрузить ссылки. {error}"),
                    );
                    state.more.set_label("Повторить");
                    state.more.set_visible(true);
                }
            }
        });
    }

    fn show(self: &Rc<Self>, page: InvitePage) {
        let next = page
            .next
            .filter(|cursor| self.cursors.borrow_mut().insert(cursor.clone()));
        self.next.replace(next);
        for link in page.links {
            if self.links.borrow_mut().insert(link.cursor.link.clone()) {
                let row = self.row(link);
                self.rows.add(&row);
                self.rendered.borrow_mut().push(row);
            }
        }
        let more = self.next.borrow().is_some();
        self.more.set_label("Показать ещё");
        self.more.set_visible(more);
        set_status(
            &self.status,
            if self.links.borrow().is_empty() && !more {
                "Ссылок пока нет."
            } else {
                ""
            },
        );
    }

    fn row(self: &Rc<Self>, link: InviteLink) -> adw::ActionRow {
        let title = if !link.title.is_empty() {
            &link.title
        } else if link.permanent {
            "Основная ссылка"
        } else {
            &link.cursor.link
        };
        let now = unix_now();
        let row = adw::ActionRow::builder()
            .title(title)
            .title_lines(1)
            .subtitle(link_details(&link, now))
            .use_markup(false)
            .build();
        if !link.revoked && !link.expired(now) && !link.exhausted() {
            let copy = icon_button("edit-copy-symbolic", "Скопировать ссылку");
            let copy_link = link.clone();
            copy.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |button| {
                    if copy_link.expired(unix_now()) {
                        set_status(&state.feedback, "Срок действия ссылки истёк.");
                        state.reset();
                    } else {
                        button.clipboard().set_text(&copy_link.cursor.link);
                        set_status(&state.feedback, "Ссылка скопирована.");
                    }
                }
            ));
            row.add_suffix(&copy);
        }
        if link.editable() {
            let edit = icon_button("document-edit-symbolic", "Редактировать ссылку");
            let edit_link = link.clone();
            edit.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |button| state.open_editor(button, edit_link.clone())
            ));
            row.add_suffix(&edit);
        }
        if !link.revoked && !link.paid {
            let revoke = icon_button("user-trash-symbolic", "Отозвать ссылку");
            revoke.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |button| state.confirm_revoke(button, &link)
            ));
            row.add_suffix(&revoke);
        }
        row
    }

    fn create(self: &Rc<Self>) {
        match self.create_fields.draft().and_then(InviteDraft::normalize) {
            Ok(draft) => self.write(InviteAction::Create(draft), None),
            Err(error) => set_status(&self.feedback, &error.to_string()),
        }
    }

    fn open_editor(self: &Rc<Self>, anchor: &gtk::Button, original: InviteLink) {
        let (dialog, body) = crate::chat_management::dialog("Редактирование ссылки");
        let (group, fields, submit) = invite_form(Some(&original));
        let status = status_label();
        body.append(&group);
        body.append(&status);
        dialog.set_focus(Some(&fields.name));
        submit.connect_clicked(glib::clone!(
            #[weak(rename_to = state)]
            self,
            #[weak]
            dialog,
            #[weak]
            status,
            move |_| match fields.edit(&original).and_then(|edit| {
                let changes = edit.changes(&original, &original)?;
                Ok((edit, changes))
            }) {
                Ok((_, changes)) if changes == InviteChanges::default() => {
                    set_status(&status, "Нет изменений.");
                }
                Ok((edit, _)) => state.write(
                    InviteAction::Edit(original.clone(), edit),
                    Some(InviteEditor { dialog, status }),
                ),
                Err(error) => set_status(&status, &error.to_string()),
            }
        ));
        dialog.present(Some(anchor));
    }

    fn confirm_revoke(self: &Rc<Self>, anchor: &gtk::Button, link: &InviteLink) {
        let body = if link.permanent {
            "По этой ссылке больше нельзя будет вступить. Telegram создаст новую основную ссылку."
        } else {
            "По этой ссылке больше нельзя будет вступить в чат."
        };
        let prompt = adw::AlertDialog::new(Some("Отозвать ссылку?"), Some(body));
        prompt.add_responses(&[("cancel", "Отмена"), ("revoke", "Отозвать")]);
        prompt.set_response_appearance("revoke", adw::ResponseAppearance::Destructive);
        prompt.set_default_response(Some("cancel"));
        prompt.set_close_response("cancel");
        if let Some(cancel) = prompt.default_widget() {
            prompt.set_focus(Some(&cancel));
        }
        let url = link.cursor.link.clone();
        prompt.connect_response(
            Some("revoke"),
            glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |_, _| state.write(InviteAction::Revoke(url.clone()), None)
            ),
        );
        prompt.present(Some(anchor));
    }

    fn write(self: &Rc<Self>, action: InviteAction, editor: Option<InviteEditor>) {
        if self.closed.get() || self.writing.replace(true) {
            return;
        }
        self.cancel_read();
        self.set_sensitive(false);
        let (create, progress, success) = match &action {
            InviteAction::Create(_) => (true, "Создание ссылки…", "Ссылка создана."),
            InviteAction::Edit(..) => (false, "Сохранение изменений…", "Ссылка обновлена."),
            InviteAction::Revoke(_) => (false, "Отзыв ссылки…", "Ссылка отозвана."),
        };
        let feedback = if let Some(editor) = editor.as_ref() {
            editor.dialog.set_can_close(false);
            editor.dialog.set_sensitive(false);
            &editor.status
        } else {
            &self.feedback
        };
        set_status(feedback, progress);
        let request = (self.context.write)(action);
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(25), request)
                .await
                .context("Telegram не подтвердил действие вовремя")?
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(state) = weak.upgrade().filter(|state| !state.closed.get()) else {
                return;
            };
            state.writing.set(false);
            state.set_sensitive(true);
            if let Some(editor) = editor.as_ref() {
                editor.dialog.set_sensitive(true);
                editor.dialog.set_can_close(true);
                if let Err(error) = &result {
                    set_status(
                        &editor.status,
                        &format!("Не удалось сохранить ссылку. {error}"),
                    );
                    return;
                }
                editor.dialog.close();
            }
            state.filter.grab_focus();
            match result {
                Ok(()) => {
                    if create {
                        state.create_fields.clear();
                    }
                    set_status(&state.feedback, success);
                }
                Err(error) => set_status(
                    &state.feedback,
                    &format!("Telegram не подтвердил действие. {error}"),
                ),
            }
            // A lost response can still mean the server applied the change.
            if editor.is_none() {
                state.form.set_active(false);
            }
            state.reset();
        });
    }

    fn set_sensitive(&self, sensitive: bool) {
        self.filter.set_sensitive(sensitive);
        self.refresh.set_sensitive(sensitive);
        self.form.set_sensitive(sensitive);
        self.form_group.set_sensitive(sensitive);
        self.more.set_sensitive(sensitive);
        self.rows.set_sensitive(sensitive);
    }
}

fn unix_now() -> i64 {
    glib::DateTime::now_utc().map_or(0, |now| now.to_unix())
}

fn link_details(link: &InviteLink, now: i64) -> String {
    let mut details = Vec::new();
    if link.revoked {
        details.push("Отозвана".to_owned());
    } else if link.expired(now) {
        details.push("Срок истёк".to_owned());
    } else if link.exhausted() {
        details.push("Лимит исчерпан".to_owned());
    }
    if link.paid {
        details.push("Подписка Telegram Stars".to_owned());
    }
    if link.approval {
        details.push(format!("Заявок на вступление {}", link.pending));
    }
    details.push(match link.limit {
        Some(limit) => format!("Вступили {} из {limit}", link.uses),
        None => format!("Вступили {}", link.uses),
    });
    if let Some(caption) = expiry_caption(link.expires) {
        details.push(caption);
    }
    details.join(" · ")
}

fn expiry_caption(expires: Option<i32>) -> Option<String> {
    let time = glib::DateTime::from_unix_local(i64::from(expires?)).ok()?;
    Some(format!("До {}", time.format("%d.%m.%Y %H:%M").ok()?))
}

fn status_label() -> gtk::Label {
    gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .visible(false)
        .build()
}

fn set_status(label: &gtk::Label, text: &str) {
    label.set_text(text);
    label.set_visible(!text.is_empty());
}

fn icon_button(icon: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.set_tooltip_text(Some(label));
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button.set_valign(gtk::Align::Center);
    button
}
