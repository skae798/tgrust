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

use crate::model::{InviteAction, InviteCursor, InviteDraft, InviteLink, InvitePage};

type PageFuture = Pin<Box<dyn Future<Output = Result<InvitePage>> + Send>>;
type WriteFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

pub struct InviteContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn(bool, Option<InviteCursor>) -> PageFuture>,
    pub write: Box<dyn Fn(InviteAction) -> WriteFuture>,
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
    let name = adw::EntryRow::builder()
        .title("Название (необязательно)")
        .build();
    let duration = adw::ActionRow::builder().title("Срок действия").build();
    let hours = gtk::DropDown::from_strings(&["Без срока", "1 час", "1 день", "1 неделя"]);
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
        move |approval| limit.set_visible(!approval.is_active())
    ));
    let submit_row = adw::ActionRow::new();
    let submit = gtk::Button::builder()
        .label("Создать ссылку")
        .valign(gtk::Align::Center)
        .css_classes(["suggested-action"])
        .build();
    submit_row.add_suffix(&submit);
    let form_group = adw::PreferencesGroup::new();
    form_group.set_visible(false);
    for row in [
        name.upcast_ref::<gtk::Widget>(),
        duration.upcast_ref(),
        approval.upcast_ref(),
        limit.upcast_ref(),
        submit_row.upcast_ref(),
    ] {
        form_group.add(row);
    }
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
        name,
        hours,
        approval,
        limit,
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
                state.name.grab_focus();
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
    name: adw::EntryRow,
    hours: gtk::DropDown,
    approval: adw::SwitchRow,
    limit: adw::EntryRow,
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
        let approval = self.approval.is_active();
        let limit = if approval {
            glib::GString::from("")
        } else {
            self.limit.text()
        };
        let limit = match limit.trim() {
            "" => None,
            raw => match raw.parse::<i32>() {
                Ok(limit) => Some(limit),
                Err(_) => {
                    set_status(&self.feedback, "Укажите целое число вступлений.");
                    return;
                }
            },
        };
        let draft = InviteDraft {
            title: self.name.text().into(),
            hours: [None, Some(1), Some(24), Some(168)][self.hours.selected().min(3) as usize],
            limit,
            approval,
        };
        match draft.normalize() {
            Ok(draft) => self.write(InviteAction::Create(draft)),
            Err(error) => set_status(&self.feedback, &error.to_string()),
        }
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
                move |_, _| state.write(InviteAction::Revoke(url.clone()))
            ),
        );
        prompt.present(Some(anchor));
    }

    fn write(self: &Rc<Self>, action: InviteAction) {
        if self.closed.get() || self.writing.replace(true) {
            return;
        }
        self.cancel_read();
        self.set_sensitive(false);
        let create = matches!(action, InviteAction::Create(_));
        set_status(
            &self.feedback,
            if create {
                "Создание ссылки…"
            } else {
                "Отзыв ссылки…"
            },
        );
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
            state.filter.grab_focus();
            match result {
                Ok(()) => {
                    if create {
                        state.name.set_text("");
                        state.hours.set_selected(0);
                        state.limit.set_text("");
                        state.approval.set_active(false);
                    }
                    set_status(
                        &state.feedback,
                        if create {
                            "Ссылка создана."
                        } else {
                            "Ссылка отозвана."
                        },
                    );
                }
                Err(error) => set_status(
                    &state.feedback,
                    &format!("Telegram не подтвердил действие. {error}"),
                ),
            }
            // A lost response can still mean the server applied the change.
            state.form.set_active(false);
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
    if let Some(expires) = link.expires
        && let Ok(time) = glib::DateTime::from_unix_local(i64::from(expires))
        && let Ok(date) = time.format("%d.%m.%Y %H:%M")
    {
        details.push(format!("До {date}"));
    }
    details.join(" · ")
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
