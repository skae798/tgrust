use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::{DeviceSession, DeviceSessions};

type LoadFuture = Pin<Box<dyn Future<Output = Result<DeviceSessions>> + Send>>;
type TerminateFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

pub struct DevicesContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn() -> LoadFuture>,
    pub terminate: Box<dyn Fn(i64) -> TerminateFuture>,
}

struct DevicesDialog {
    context: DevicesContext,
    dialog: glib::WeakRef<adw::Dialog>,
    status: gtk::Label,
    spinner: gtk::Spinner,
    refresh: gtk::Button,
    retry: gtk::Button,
    groups: gtk::Box,
    snapshot: RefCell<Option<DeviceSessions>>,
    task: RefCell<Option<tokio::task::AbortHandle>>,
    mutating: Cell<bool>,
    confirming: Cell<bool>,
    closed: Cell<bool>,
}

pub fn open(window: &adw::ApplicationWindow, context: DevicesContext) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Устройства")
        .content_width(480)
        .content_height(560)
        .build();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Обновить устройства")
        .build();
    header.pack_end(&refresh);
    toolbar.add_top_bar(&header);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_top(12);
    body.set_margin_bottom(18);
    let spinner = gtk::Spinner::new();
    spinner.set_size_request(24, 24);
    body.append(&spinner);
    let status = text("Загрузка устройств…");
    status.add_css_class("dim-label");
    body.append(&status);
    let retry = gtk::Button::with_label("Обновить список");
    retry.set_halign(gtk::Align::Center);
    retry.set_visible(false);
    body.append(&retry);
    let groups = gtk::Box::new(gtk::Orientation::Vertical, 18);
    body.append(&groups);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    scroll.set_focusable(true);
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));
    dialog.set_focus(Some(&scroll));
    let state = Rc::new(DevicesDialog {
        context,
        dialog: dialog.downgrade(),
        status,
        spinner,
        refresh,
        retry,
        groups,
        snapshot: RefCell::new(None),
        task: RefCell::new(None),
        mutating: Cell::new(false),
        confirming: Cell::new(false),
        closed: Cell::new(false),
    });
    for button in [&state.refresh, &state.retry] {
        button.connect_clicked(glib::clone!(
            #[weak]
            state,
            move |_| state.load(None)
        ));
    }
    state.load(None);
    dialog.connect_closed(move |_| {
        state.closed.set(true);
        // Closing a read-only request cancels it. An explicitly confirmed reset
        // continues: cancelling a local future cannot retract a server mutation.
        if !state.mutating.get()
            && let Some(task) = state.task.take()
        {
            task.abort();
        }
    });
    dialog.present(Some(window));
    dialog
}

impl DevicesDialog {
    fn busy(&self) -> bool {
        self.task.borrow().is_some() || self.confirming.get()
    }

    fn update_controls(&self) {
        let busy = self.busy();
        self.refresh.set_sensitive(!busy);
        self.retry.set_sensitive(!busy);
        self.groups.set_sensitive(!busy);
        self.spinner.set_visible(self.task.borrow().is_some());
        self.spinner.set_spinning(self.task.borrow().is_some());
    }

    fn load(self: &Rc<Self>, success: Option<&str>) {
        if self.closed.get() || self.busy() {
            return;
        }
        let prefix = success.map(str::to_owned);
        self.status
            .set_text(success.unwrap_or("Загрузка устройств…"));
        self.retry.set_visible(false);
        let request = (self.context.load)();
        let task = self
            .context
            .runtime
            .spawn(async move { tokio::time::timeout(Duration::from_secs(20), request).await });
        self.task.replace(Some(task.abort_handle()));
        self.update_controls();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result.context("Telegram не ответил вовремя"))
                .and_then(|result| result);
            if let Some(state) = weak.upgrade()
                && !state.closed.get()
            {
                state.task.take();
                match result {
                    Ok(snapshot) => {
                        state.snapshot.replace(Some(snapshot));
                        state.render();
                        state.status.set_text(
                            prefix.as_deref().unwrap_or(
                                "Выберите устройство, чтобы посмотреть сведения о сеансе.",
                            ),
                        );
                    }
                    Err(error) => {
                        state.status.set_text(&format!(
                            "{}Не удалось обновить устройства. {error}",
                            prefix.map_or(String::new(), |text| format!("{text} "))
                        ));
                        state.retry.set_visible(true);
                    }
                }
                state.update_controls();
            }
        });
    }

    fn render(self: &Rc<Self>) {
        while let Some(child) = self.groups.first_child() {
            self.groups.remove(&child);
        }
        let snapshot = self.snapshot.borrow();
        let Some(snapshot) = snapshot.as_ref() else {
            return;
        };
        for (current, title) in [(true, "Это устройство"), (false, "Другие сеансы")]
        {
            let group = adw::PreferencesGroup::builder().title(title).build();
            let mut count = 0;
            for session in snapshot
                .sessions
                .iter()
                .filter(|session| session.current == current)
            {
                count += 1;
                let row = adw::ExpanderRow::new();
                row.set_use_markup(false);
                row.set_title(if session.device.trim().is_empty() {
                    "Неизвестное устройство"
                } else {
                    &session.device
                });
                row.set_subtitle(&joined(&[
                    &session.application,
                    &session.app_version,
                    &session.platform,
                    &session.system_version,
                ]));
                row.set_title_lines(2);
                row.set_subtitle_lines(2);
                for (title, value) in [
                    (
                        "Приложение",
                        joined(&[&session.application, &session.app_version]),
                    ),
                    (
                        "Система",
                        joined(&[&session.platform, &session.system_version]),
                    ),
                    ("IP-адрес", session.ip.clone()),
                    (
                        "Местоположение",
                        joined(&[&session.country, &session.region]),
                    ),
                    ("Вход выполнен", timestamp(session.created)),
                    ("Последняя активность", timestamp(session.active)),
                ] {
                    if !value.trim().is_empty() {
                        let detail = adw::ActionRow::new();
                        detail.set_use_markup(false);
                        detail.set_title(title);
                        detail.set_subtitle(&value);
                        detail.set_subtitle_selectable(true);
                        detail.set_subtitle_lines(0);
                        row.add_row(&detail);
                    }
                }
                for (enabled, label) in [
                    (session.official, "Официальное приложение Telegram"),
                    (session.password_pending, "Ожидает облачного пароля"),
                    (session.unconfirmed, "Неподтверждённый сеанс"),
                ] {
                    if enabled {
                        let flag = adw::ActionRow::builder().title(label).build();
                        row.add_row(&flag);
                    }
                }
                if session.can_terminate() {
                    let terminate = gtk::Button::builder()
                        .icon_name("process-stop-symbolic")
                        .tooltip_text(format!("Завершить сеанс: {}", session.device))
                        .valign(gtk::Align::Center)
                        .build();
                    terminate.add_css_class("flat");
                    terminate.add_css_class("destructive-action");
                    let session = session.clone();
                    terminate.connect_clicked(glib::clone!(
                        #[weak(rename_to = state)]
                        self,
                        move |_| state.confirm(&session)
                    ));
                    row.add_suffix(&terminate);
                }
                group.add(&row);
            }
            if count > 0 {
                self.groups.append(&group);
            } else if !current {
                self.groups.append(&text("Других активных сеансов нет"));
            }
        }
        if snapshot.inactive_ttl_days > 0 {
            self.groups.append(&text(&format!(
                "Автозавершение текущего сеанса: {} дн.",
                snapshot.inactive_ttl_days
            )));
        }
    }

    fn confirm(self: &Rc<Self>, session: &DeviceSession) {
        if self.closed.get() || self.busy() || !session.can_terminate() {
            return;
        }
        let Some(parent) = self.dialog.upgrade() else {
            return;
        };
        self.confirming.set(true);
        self.update_controls();
        let confirmation = adw::AlertDialog::builder()
            .heading("Завершить сеанс?")
            .body_use_markup(false)
            .body(format!(
                "{}\n{}\n{}\n\nНа этом устройстве потребуется снова войти в Telegram.",
                session.device,
                joined(&[&session.application, &session.app_version]),
                joined(&[&session.ip, &session.country, &session.region])
            ))
            .close_response("cancel")
            .default_response("cancel")
            .build();
        confirmation.add_response("cancel", "Отмена");
        confirmation.add_response("terminate", "Завершить сеанс");
        confirmation.set_response_appearance("terminate", adw::ResponseAppearance::Destructive);
        let weak = Rc::downgrade(self);
        let id = session.id;
        glib::MainContext::default().spawn_local(async move {
            let response = confirmation.choose_future(Some(&parent)).await;
            if let Some(state) = weak.upgrade()
                && !state.closed.get()
            {
                state.confirming.set(false);
                state.update_controls();
                if response == "terminate" {
                    state.terminate(id);
                }
            }
        });
    }

    fn terminate(self: &Rc<Self>, id: i64) {
        if self.closed.get() || self.busy() {
            return;
        }
        self.mutating.set(true);
        self.status.set_text("Завершение сеанса…");
        self.retry.set_visible(false);
        let request = (self.context.terminate)(id);
        let task = self
            .context
            .runtime
            .spawn(async move { tokio::time::timeout(Duration::from_secs(30), request).await });
        self.task.replace(Some(task.abort_handle()));
        self.update_controls();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result.context("Telegram не ответил вовремя"))
                .and_then(|result| result);
            if let Some(state) = weak.upgrade()
                && !state.closed.get()
            {
                state.task.take();
                state.mutating.set(false);
                match result {
                    Ok(()) => {
                        if let Some(snapshot) = state.snapshot.borrow_mut().as_mut() {
                            snapshot
                                .sessions
                                .retain(|session| session.id != id || session.current);
                        }
                        state.render();
                        state.update_controls();
                        state.load(Some("Сеанс завершён."));
                    }
                    Err(error) => {
                        state.status.set_text(&format!(
                            "Не удалось подтвердить завершение сеанса. Обновите список. {error}"
                        ));
                        state.retry.set_visible(true);
                        state.update_controls();
                    }
                }
            }
        });
    }
}

fn text(value: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(value));
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label
}

fn joined(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn timestamp(timestamp: i64) -> String {
    (timestamp > 0)
        .then(|| glib::DateTime::from_unix_local(timestamp).ok())
        .flatten()
        .and_then(|time| time.format("%x %H:%M").ok())
        .map_or_else(|| "Неизвестно".into(), |time| time.to_string())
}
