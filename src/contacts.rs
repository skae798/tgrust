use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::{Avatar, ChatKind, Contact};

type ContactFuture = Pin<Box<dyn Future<Output = Result<Arc<Vec<Contact>>>> + Send>>;
type AvatarLoader = Box<dyn Fn(&adw::Avatar, Avatar)>;
type ContactItem = (Arc<Vec<Contact>>, usize);

pub struct ContactContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn(bool) -> ContactFuture>,
    pub avatar: AvatarLoader,
    pub selected: Box<dyn Fn(Contact)>,
    pub ready: Box<dyn Fn()>,
}

struct ContactsDialog {
    context: ContactContext,
    dialog: glib::WeakRef<adw::Dialog>,
    search: gtk::SearchEntry,
    status: gtk::Label,
    refresh: gtk::Button,
    retry: gtk::Button,
    store: gtk::gio::ListStore,
    filter: gtk::CustomFilter,
    filtered: gtk::FilterListModel,
    task: RefCell<Option<tokio::task::AbortHandle>>,
    error: RefCell<Option<String>>,
    loaded: Cell<bool>,
    closed: Cell<bool>,
    chosen: RefCell<Option<Contact>>,
    activate_first: Cell<bool>,
}

pub fn open(window: &adw::ApplicationWindow, context: ContactContext) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Контакты")
        .content_width(440)
        .content_height(520)
        .build();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Обновить контакты")
        .build();
    header.pack_end(&refresh);
    toolbar.add_top_bar(&header);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_top(8);
    body.set_margin_bottom(12);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Имя, username или телефон")
        .build();
    search.update_property(&[gtk::accessible::Property::Label("Найти контакт")]);
    body.append(&search);
    let status = gtk::Label::new(Some("Загрузка контактов…"));
    status.set_wrap(true);
    status.add_css_class("dim-label");
    body.append(&status);
    let retry = gtk::Button::with_label("Повторить");
    retry.set_halign(gtk::Align::Center);
    retry.set_visible(false);
    body.append(&retry);
    let store = gtk::gio::ListStore::new::<glib::BoxedAnyObject>();
    let filter = gtk::CustomFilter::new(glib::clone!(
        #[weak]
        search,
        #[upgrade_or]
        false,
        move |object| {
            let item = object
                .downcast_ref::<glib::BoxedAnyObject>()
                .unwrap()
                .borrow::<ContactItem>();
            item.0[item.1].matches(&search.text())
        }
    ));
    let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
    filtered.set_incremental(true);
    let selection = gtk::SingleSelection::new(Some(filtered.clone()));
    selection.set_autoselect(false);
    let factory = gtk::SignalListItemFactory::new();
    let list = gtk::ListView::new(Some(selection), Some(factory.clone()));
    list.set_single_click_activate(true);
    list.add_css_class("navigation-sidebar");
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    body.append(&scroll);
    toolbar.set_content(Some(&body));
    dialog.set_child(Some(&toolbar));
    dialog.set_focus(Some(&search));
    let state = Rc::new(ContactsDialog {
        context,
        dialog: dialog.downgrade(),
        search,
        status,
        refresh,
        retry,
        store,
        filter,
        filtered,
        task: RefCell::new(None),
        error: RefCell::new(None),
        loaded: Cell::new(false),
        closed: Cell::new(false),
        chosen: RefCell::new(None),
        activate_first: Cell::new(false),
    });
    factory.connect_bind(glib::clone!(
        #[weak]
        state,
        move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let object = item
                .item()
                .unwrap()
                .downcast::<glib::BoxedAnyObject>()
                .unwrap();
            let contact = object.borrow::<ContactItem>();
            let contact = &contact.0[contact.1];
            // Each binding owns a fresh avatar so a late image never updates a recycled row.
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.set_margin_top(8);
            row.set_margin_bottom(8);
            row.set_margin_start(8);
            row.set_margin_end(8);
            row.set_sensitive(contact.available);
            let avatar = adw::Avatar::new(40, Some(&contact.name), true);
            if let Some(photo) = contact.avatar {
                (state.context.avatar)(&avatar, photo);
            }
            row.append(&avatar);
            let labels = gtk::Box::new(gtk::Orientation::Vertical, 3);
            labels.set_hexpand(true);
            let name = gtk::Label::new(Some(&contact.name));
            name.set_xalign(0.0);
            name.add_css_class("heading");
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let subtitle = if !contact.available {
                "Недоступен"
            } else if contact.is_self {
                "Ваш аккаунт"
            } else {
                contact.usernames.first().map(String::as_str).unwrap_or(
                    if contact.kind == ChatKind::Bot {
                        "Бот"
                    } else {
                        "Контакт"
                    },
                )
            };
            let subtitle = gtk::Label::new(Some(subtitle));
            subtitle.set_xalign(0.0);
            subtitle.add_css_class("dim-label");
            subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
            labels.append(&name);
            labels.append(&subtitle);
            row.append(&labels);
            item.set_activatable(contact.available);
            item.set_selectable(contact.available);
            item.set_child(Some(&row));
        }
    ));
    factory.connect_unbind(|_, item| {
        item.downcast_ref::<gtk::ListItem>()
            .unwrap()
            .set_child(None::<&gtk::Widget>)
    });
    list.connect_activate(glib::clone!(
        #[weak]
        state,
        move |_, position| state.choose(position)
    ));
    state.search.connect_changed(glib::clone!(
        #[weak]
        state,
        move |_| {
            state.activate_first.set(false);
            state.filter.changed(gtk::FilterChange::Different);
            state.update_status();
        }
    ));
    state.search.connect_activate(glib::clone!(
        #[weak]
        state,
        move |_| {
            if state.task.borrow().is_some() || state.filtered.pending() > 0 {
                state.activate_first.set(true);
            } else {
                state.choose(0);
            }
        }
    ));
    state.search.connect_stop_search(glib::clone!(
        #[weak]
        state,
        move |_| {
            if let Some(dialog) = state.dialog.upgrade() {
                dialog.close();
            }
        }
    ));
    state.refresh.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.load(true)
    ));
    state.retry.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.load(true)
    ));
    state.filtered.connect_items_changed(glib::clone!(
        #[weak]
        state,
        move |_, _, _, _| state.update_status()
    ));
    state.filtered.connect_pending_notify(glib::clone!(
        #[weak]
        state,
        move |_| state.update_status()
    ));
    state.load(false);
    dialog.connect_closed(move |_| {
        state.closed.set(true);
        if let Some(task) = state.task.take() {
            task.abort();
        }
        if state.chosen.take().is_some() {
            (state.context.ready)();
        }
    });
    dialog.present(Some(window));
    dialog
}

impl ContactsDialog {
    fn load(self: &Rc<Self>, refresh: bool) {
        if self.closed.get() || self.task.borrow().is_some() {
            return;
        }
        self.error.take();
        self.activate_first.set(false);
        self.refresh.set_sensitive(false);
        self.retry.set_visible(false);
        let task = self.context.runtime.spawn((self.context.load)(refresh));
        self.task.replace(Some(task.abort_handle()));
        self.update_status();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            if let Some(state) = weak.upgrade()
                && !state.closed.get()
            {
                match result {
                    Ok(contacts) => {
                        let items: Vec<_> = (0..contacts.len())
                            .map(|index| glib::BoxedAnyObject::new((contacts.clone(), index)))
                            .collect();
                        state.store.splice(0, state.store.n_items(), &items);
                        state.loaded.set(true);
                    }
                    Err(error) => {
                        state.activate_first.set(false);
                        state
                            .error
                            .replace(Some(format!("Не удалось загрузить контакты. {error}")));
                    }
                }
                state.task.take();
                state.refresh.set_sensitive(true);
                state.update_status();
            }
        });
    }

    fn update_status(&self) {
        if self.closed.get() {
            return;
        }
        let error = self.error.borrow();
        self.retry.set_visible(error.is_some());
        let value = if self.task.borrow().is_some() {
            "Загрузка контактов…".to_owned()
        } else if let Some(error) = error.as_ref() {
            error.clone()
        } else if self.filtered.pending() > 0 {
            "Поиск…".to_owned()
        } else if !self.loaded.get() {
            String::new()
        } else if self.store.n_items() == 0 {
            "Контактов пока нет".to_owned()
        } else if self.filtered.n_items() == 0 {
            "Контакты не найдены".to_owned()
        } else {
            format!("Контакты: {}", self.filtered.n_items())
        };
        self.status.set_text(&value);
        let activate = error.is_none()
            && self.task.borrow().is_none()
            && self.filtered.pending() == 0
            && self.activate_first.replace(false);
        drop(error);
        if activate {
            self.choose(0);
        }
    }

    fn choose(&self, position: u32) {
        if self.closed.get() || self.chosen.borrow().is_some() {
            return;
        }
        let Some(item) = self.filtered.item(position) else {
            return;
        };
        let item = item.downcast::<glib::BoxedAnyObject>().unwrap();
        let contact = item.borrow::<ContactItem>();
        let contact = &contact.0[contact.1];
        if !contact.available || !contact.matches(&self.search.text()) {
            return;
        }
        self.chosen.replace(Some(contact.clone()));
        if let Some(dialog) = self.dialog.upgrade() {
            (self.context.selected)(contact.clone());
            dialog.close();
        }
    }
}
