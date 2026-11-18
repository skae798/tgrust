use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::{runtime::Handle, sync::watch};

use crate::model::{Avatar, BlockedPage, BlockedPeer};

type Request<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;
pub type BlockAction = Rc<dyn Fn(i64, bool) -> Request<()>>;
type AvatarLoader = Box<dyn Fn(&adw::Avatar, Avatar)>;

pub struct BlockedContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn(i32) -> Request<BlockedPage>>,
    pub change: BlockAction,
    pub avatar: AvatarLoader,
    pub changes: Option<watch::Receiver<u64>>,
}

struct BlockedDialog {
    context: BlockedContext,
    list: gtk::ListBox,
    refresh: gtk::Button,
    status: gtk::Label,
    more: gtk::Button,
    rows: RefCell<HashMap<i64, gtk::ListBoxRow>>,
    next_offset: Cell<Option<i32>>,
    read: RefCell<Option<tokio::task::AbortHandle>>,
    writing: Cell<bool>,
    refresh_pending: Cell<bool>,
    closed: Cell<bool>,
}

pub fn open(window: &adw::ApplicationWindow, mut context: BlockedContext) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Заблокированные")
        .content_width(460)
        .content_height(540)
        .build();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Обновить список заблокированных")
        .build();
    header.pack_end(&refresh);
    toolbar.add_top_bar(&header);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let status = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["dim-label"])
        .build();
    let more = gtk::Button::builder()
        .label("Загрузить ещё")
        .halign(gtk::Align::Center)
        .visible(false)
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(12);
    body.set_margin_bottom(16);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.append(&status);
    body.append(&list);
    body.append(&more);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));
    dialog.set_focus(Some(&refresh));
    let changes = context.changes.take();
    let state = Rc::new(BlockedDialog {
        context,
        list,
        refresh,
        status,
        more,
        rows: RefCell::new(HashMap::new()),
        next_offset: Cell::new(None),
        read: RefCell::new(None),
        writing: Cell::new(false),
        refresh_pending: Cell::new(false),
        closed: Cell::new(false),
    });
    state.refresh.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.reload()
    ));
    state.more.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| {
            if let Some(offset) = state.next_offset.get() {
                state.load(offset);
            }
        }
    ));
    let listener = changes.map(|mut changes| {
        let weak = Rc::downgrade(&state);
        glib::MainContext::default().spawn_local(async move {
            while changes.changed().await.is_ok() {
                let Some(state) = weak.upgrade() else { break };
                state.reload();
            }
        })
    });
    state.load(0);
    dialog.connect_closed(move |_| {
        state.closed.set(true);
        if let Some(read) = state.read.take() {
            read.abort();
        }
        if let Some(listener) = &listener {
            listener.abort();
        }
    });
    dialog.present(Some(window));
    dialog
}

impl BlockedDialog {
    fn busy(&self) -> bool {
        self.read.borrow().is_some() || self.writing.get()
    }

    fn reload(self: &Rc<Self>) {
        if self.more.has_focus() {
            self.refresh.grab_focus();
        }
        self.next_offset.set(None);
        self.more.set_visible(false);
        if self.busy() {
            self.refresh_pending.set(true);
        } else {
            self.load(0);
        }
    }

    fn load(self: &Rc<Self>, offset: i32) {
        if self.closed.get() || self.busy() {
            return;
        }
        let list_focused = self
            .list
            .root()
            .and_then(|root| root.focus())
            .is_some_and(|focus| focus.is_ancestor(&self.list));
        if self.more.has_focus() || list_focused {
            self.refresh.grab_focus();
        }
        self.set_status("Загрузка…");
        self.list.set_sensitive(false);
        self.more.set_sensitive(false);
        let load = (self.context.load)(offset);
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(25), load)
                .await
                .context("Telegram не ответил вовремя")?
        });
        self.read.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(state) = weak.upgrade().filter(|state| !state.closed.get()) else {
                return;
            };
            state.read.take();
            if state.refresh_pending.replace(false) {
                state.load(0);
                return;
            }
            state.list.set_sensitive(true);
            state.more.set_sensitive(true);
            match result {
                Ok(page) => state.show(offset, page),
                Err(error) => state.set_status(&format!("Не удалось загрузить список. {error}")),
            }
        });
    }

    fn set_status(&self, value: &str) {
        self.status.set_text(value);
        self.status.set_visible(!value.is_empty());
    }

    fn show(self: &Rc<Self>, offset: i32, page: BlockedPage) {
        if offset == 0 {
            for (_, row) in self.rows.borrow_mut().drain() {
                self.list.remove(&row);
            }
        }
        for peer in page.peers {
            if !self.rows.borrow().contains_key(&peer.id) {
                self.append(peer);
            }
        }
        self.next_offset.set(page.next_offset);
        self.more.set_visible(page.next_offset.is_some());
        if self.rows.borrow().is_empty() && page.next_offset.is_none() {
            self.set_status("Список заблокированных пуст");
        } else {
            self.set_status("");
        }
    }

    fn append(self: &Rc<Self>, peer: BlockedPeer) {
        let row = adw::ActionRow::builder().title_lines(2).build();
        row.set_use_markup(false);
        row.set_title(&peer.name);
        if let Some(username) = &peer.username {
            row.set_subtitle(username);
        }
        let avatar = adw::Avatar::new(36, Some(&peer.name), true);
        avatar.set_valign(gtk::Align::Center);
        if let Some(photo) = peer.avatar {
            (self.context.avatar)(&avatar, photo);
        }
        row.add_prefix(&avatar);
        let unblock = gtk::Button::builder()
            .icon_name("changes-allow-symbolic")
            .tooltip_text(if peer.available {
                format!("Разблокировать: {}", peer.name)
            } else {
                "Профиль недоступен для разблокировки".into()
            })
            .valign(gtk::Align::Center)
            .sensitive(peer.available)
            .css_classes(["flat"])
            .build();
        unblock.connect_clicked(glib::clone!(
            #[weak(rename_to = state)]
            self,
            move |button| {
                let name = peer.name.clone();
                let weak = Rc::downgrade(&state);
                confirm(button, &name, false, move || {
                    if let Some(state) = weak.upgrade() {
                        state.unblock(peer.id);
                    }
                });
            }
        ));
        row.add_suffix(&unblock);
        self.list.append(&row);
        self.rows.borrow_mut().insert(peer.id, row.upcast());
    }

    fn unblock(self: &Rc<Self>, id: i64) {
        if self.closed.get() || self.busy() || !self.rows.borrow().contains_key(&id) {
            return;
        }
        self.refresh.grab_focus();
        self.writing.set(true);
        self.list.set_sensitive(false);
        self.more.set_sensitive(false);
        self.set_status("Разблокирование…");
        let task = request_change(&self.context.runtime, &self.context.change, id, false);
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(state) = weak.upgrade().filter(|state| !state.closed.get()) else {
                return;
            };
            state.writing.set(false);
            let refresh = state.refresh_pending.replace(false);
            match result {
                Ok(()) => {
                    if let Some(row) = state.rows.borrow_mut().remove(&id) {
                        state.list.remove(&row);
                    }
                    state.next_offset.set(None);
                    state.more.set_visible(false);
                    state.load(0);
                }
                Err(error) => {
                    if refresh {
                        state.next_offset.set(None);
                        state.more.set_visible(false);
                    }
                    state.list.set_sensitive(true);
                    state.more.set_sensitive(true);
                    state.set_status(&format!("Не удалось подтвердить разблокировку. {error}"));
                }
            }
        });
    }
}

pub fn request_change(
    runtime: &Handle,
    action: &BlockAction,
    id: i64,
    blocked: bool,
) -> tokio::task::JoinHandle<Result<()>> {
    let future = action(id, blocked);
    runtime.spawn(async move {
        tokio::time::timeout(Duration::from_secs(25), future)
            .await
            .context("Telegram не ответил вовремя")?
    })
}

pub fn confirm(
    parent: &impl IsA<gtk::Widget>,
    name: &str,
    blocked: bool,
    action: impl FnOnce() + 'static,
) {
    let (heading, accept) = if blocked {
        ("Заблокировать профиль?", "Заблокировать")
    } else {
        ("Снять блокировку?", "Разблокировать")
    };
    let prompt = adw::AlertDialog::new(Some(heading), Some(name));
    prompt.set_body_use_markup(false);
    prompt.add_responses(&[("cancel", "Отмена"), ("confirm", accept)]);
    if blocked {
        prompt.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    }
    prompt.set_default_response(Some("cancel"));
    prompt.set_close_response("cancel");
    let action = RefCell::new(Some(action));
    prompt.connect_response(Some("confirm"), move |_, _| {
        if let Some(action) = action.take() {
            action();
        }
    });
    prompt.present(Some(parent));
}
