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

use crate::model::{ChatKind, Contact};

pub type LoadContacts =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Arc<Vec<Contact>>>> + Send>>>;
const PAGE_SIZE: usize = 30;

pub struct ContactPicker {
    pub widget: gtk::Box,
    pub search: gtk::SearchEntry,
    runtime: Handle,
    load: LoadContacts,
    heading: gtk::Label,
    heading_text: Cell<&'static str>,
    allow_bots: Cell<bool>,
    group: adw::PreferencesGroup,
    status: gtk::Label,
    more: gtk::Button,
    retry: gtk::Button,
    contacts: RefCell<Option<Arc<Vec<Contact>>>>,
    selected: RefCell<HashSet<i64>>,
    rows: RefCell<Vec<gtk::CheckButton>>,
    limit: Cell<usize>,
    pending: RefCell<Option<AbortHandle>>,
}

impl ContactPicker {
    pub fn new(runtime: Handle, load: LoadContacts) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let heading = caption();
        widget.append(&heading);
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Найти участника")
            .build();
        search.update_property(&[gtk::accessible::Property::Label("Найти участника")]);
        widget.append(&search);
        let group = adw::PreferencesGroup::new();
        widget.append(&group);
        let status = caption();
        widget.append(&status);
        let more = gtk::Button::with_label("Ещё контакты");
        more.set_halign(gtk::Align::Start);
        more.set_visible(false);
        widget.append(&more);
        let retry = gtk::Button::with_label("Повторить загрузку контактов");
        retry.set_halign(gtk::Align::Start);
        retry.set_visible(false);
        widget.append(&retry);
        let picker = Rc::new(Self {
            widget,
            search,
            runtime,
            load,
            heading,
            group,
            status,
            more,
            retry,
            heading_text: Cell::new("Выберите участников"),
            allow_bots: Cell::new(true),
            contacts: RefCell::new(None),
            selected: RefCell::new(HashSet::new()),
            rows: RefCell::new(Vec::new()),
            limit: Cell::new(PAGE_SIZE),
            pending: RefCell::new(None),
        });
        picker.search.connect_search_changed(glib::clone!(
            #[weak]
            picker,
            move |_| {
                picker.limit.set(PAGE_SIZE);
                picker.render();
            }
        ));
        picker.more.connect_clicked(glib::clone!(
            #[weak]
            picker,
            move |_| {
                picker
                    .limit
                    .set(picker.limit.get().saturating_add(PAGE_SIZE));
                picker.render();
            }
        ));
        picker.retry.connect_clicked(glib::clone!(
            #[weak]
            picker,
            move |_| picker.load()
        ));
        picker.update_count();
        picker.load();
        picker
    }

    pub fn configure(self: &Rc<Self>, heading: &'static str, allow_bots: bool) {
        self.heading_text.set(heading);
        let previous = self.allow_bots.replace(allow_bots);
        if previous != allow_bots {
            self.render();
        }
        self.update_count();
    }

    pub fn chosen(&self) -> Vec<Contact> {
        let selected = self.selected.borrow();
        self.contacts
            .borrow()
            .iter()
            .flat_map(|items| items.iter())
            .filter(|contact| self.eligible(contact) && selected.contains(&contact.id))
            .cloned()
            .collect()
    }

    pub fn cancel_load(&self) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
    }

    fn eligible(&self, contact: &Contact) -> bool {
        !contact.is_self
            && contact.available
            && (self.allow_bots.get() || contact.kind != ChatKind::Bot)
    }

    fn update_count(&self) {
        self.heading.set_text(&format!(
            "{} · {}",
            self.heading_text.get(),
            self.chosen().len()
        ));
    }

    fn load(self: &Rc<Self>) {
        self.retry.set_visible(false);
        self.status.set_text("Загрузка контактов…");
        let request = (self.load)();
        let task = self.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(15), request)
                .await
                .context("Telegram не прислал контакты вовремя")?
        });
        self.pending.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(picker) = weak
                .upgrade()
                .filter(|picker| picker.pending.take().is_some())
            else {
                return;
            };
            match result {
                Ok(contacts) => {
                    picker.contacts.replace(Some(contacts));
                    picker.render();
                }
                Err(error) => {
                    picker
                        .status
                        .set_text(&format!("Не удалось загрузить контакты. {error}"));
                    picker.retry.set_visible(true);
                }
            }
        });
    }

    fn render(self: &Rc<Self>) {
        let contacts = self.contacts.borrow();
        let Some(contacts) = contacts.as_ref() else {
            return;
        };
        for row in self.rows.take() {
            self.group.remove(&row);
        }
        let query = self.search.text();
        let mut matching = contacts
            .iter()
            .filter(|contact| self.eligible(contact) && contact.matches(&query));
        for contact in matching.by_ref().take(self.limit.get()) {
            let check = gtk::CheckButton::with_label(&contact.name);
            if let Some(label) = check.child().and_downcast::<gtk::Label>() {
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            }
            check.set_active(self.selected.borrow().contains(&contact.id));
            let id = contact.id;
            check.connect_toggled(glib::clone!(
                #[weak(rename_to = picker)]
                self,
                move |check| {
                    if check.is_active() {
                        picker.selected.borrow_mut().insert(id);
                    } else {
                        picker.selected.borrow_mut().remove(&id);
                    }
                    picker.update_count();
                }
            ));
            self.group.add(&check);
            self.rows.borrow_mut().push(check);
        }
        self.more.set_visible(matching.next().is_some());
        self.status.set_text(if self.rows.borrow().is_empty() {
            "Контактов не найдено."
        } else {
            ""
        });
        self.update_count();
    }
}

pub(crate) fn caption() -> gtk::Label {
    gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["dim-label"])
        .build()
}
