use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::model::ClientUpdateStatus;
use crate::updater::{self, Installation, Outcome};

pub fn register(app: &adw::Application, demo: bool) {
    let (installation, status) = match Installation::detect(demo) {
        Ok(installation) => match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
        {
            Ok(runtime) => (Some((installation, runtime)), ClientUpdateStatus::Idle),
            Err(_) => (
                None,
                ClientUpdateStatus::Disabled("Не удалось запустить проверку обновлений."),
            ),
        },
        Err(reason) => (None, ClientUpdateStatus::Disabled(reason)),
    };
    let (status, _) = watch::channel(status);
    let updater = Rc::new(UpdateController {
        app: app.downgrade(),
        installation,
        status,
        dialog: glib::WeakRef::new(),
    });

    let updates = gio::SimpleAction::new("updates", None);
    updates.connect_activate({
        let updater = updater.clone();
        move |_, _| updater.present()
    });
    app.add_action(&updates);
    let check = gio::SimpleAction::new("check-update", None);
    check.connect_activate({
        let updater = updater.clone();
        move |_, _| updater.check()
    });
    app.add_action(&check);

    if updater.installation.is_some() {
        app.connect_startup(move |app| {
            app.withdraw_notification("client-update");
            glib::timeout_add_local_once(
                Duration::from_secs(3),
                glib::clone!(
                    #[weak]
                    updater,
                    move || updater.check()
                ),
            );
            glib::timeout_add_local(
                Duration::from_secs(6 * 60 * 60),
                glib::clone!(
                    #[weak]
                    updater,
                    #[upgrade_or]
                    glib::ControlFlow::Break,
                    move || {
                        updater.check();
                        glib::ControlFlow::Continue
                    }
                ),
            );
        });
    }
}

struct UpdateController {
    app: glib::WeakRef<adw::Application>,
    installation: Option<(Installation, Runtime)>,
    status: watch::Sender<ClientUpdateStatus>,
    dialog: glib::WeakRef<adw::Dialog>,
}

impl UpdateController {
    fn check(self: &Rc<Self>) {
        let Some((installation, runtime)) = &self.installation else {
            return;
        };
        if !self.status.borrow().can_check() {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            return;
        };

        self.status.send_replace(ClientUpdateStatus::Checking);
        let installation = installation.clone();
        let status = self.status.clone();
        let hold = app.hold();
        let task = runtime.spawn_blocking(move || {
            installation.check(|percent| {
                status.send_replace(ClientUpdateStatus::Downloading(percent));
            })
        });
        let status = self.status.clone();
        glib::spawn_future_local(async move {
            match task.await {
                Ok(Ok(Outcome::Current)) => {
                    status.send_replace(ClientUpdateStatus::Current);
                }
                Ok(Ok(Outcome::Installed(build))) => {
                    status.send_replace(ClientUpdateStatus::Installed(build));
                    let notification = gio::Notification::new("tgrust обновлён");
                    notification.set_body(Some("Новая сборка откроется при следующем запуске."));
                    notification.set_default_action("app.updates");
                    app.send_notification(Some("client-update"), &notification);
                }
                Ok(Err(error)) => {
                    status.send_replace(ClientUpdateStatus::Failed(format!("{error:#}")));
                }
                Err(_) => {
                    status.send_replace(ClientUpdateStatus::Failed("Проверка прервана.".into()));
                }
            }
            drop(hold);
        });
    }

    fn present(self: &Rc<Self>) {
        let Some(window) = self.app.upgrade().and_then(|app| app.active_window()) else {
            return;
        };
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.present(Some(&window));
            return;
        }
        let dialog = adw::Dialog::builder()
            .title("Обновления клиента")
            .content_width(400)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        body.set_margin_start(18);
        body.set_margin_end(18);
        body.set_margin_top(12);
        body.set_margin_bottom(18);
        let build = gtk::Label::builder()
            .label(format!("Текущая версия {}", updater::version()))
            .wrap(true)
            .xalign(0.0)
            .css_classes(["heading"])
            .build();
        let message = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        let check = gtk::Button::builder()
            .label("Проверить сейчас")
            .action_name("app.check-update")
            .halign(gtk::Align::End)
            .css_classes(["suggested-action"])
            .build();
        let mut status = self.status.subscribe();
        render(&status.borrow_and_update(), &message, &check);

        body.append(&build);
        body.append(&message);
        body.append(&check);
        toolbar.set_content(Some(&body));
        dialog.set_child(Some(&toolbar));
        self.dialog.set(Some(&dialog));
        let watch = glib::spawn_future_local(async move {
            while status.changed().await.is_ok() {
                render(&status.borrow_and_update(), &message, &check);
            }
        });
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = updater)]
            self,
            move |_| {
                watch.abort();
                updater.dialog.set(None);
            }
        ));
        dialog.present(Some(&window));
    }
}

fn render(status: &ClientUpdateStatus, message: &gtk::Label, check: &gtk::Button) {
    let description = match status {
        ClientUpdateStatus::Disabled(reason) => (*reason).into(),
        ClientUpdateStatus::Idle => "Обновления проверяются и загружаются автоматически.".into(),
        ClientUpdateStatus::Checking => "Проверка обновлений…".into(),
        ClientUpdateStatus::Downloading(percent) => format!("Загрузка обновления… {percent}%"),
        ClientUpdateStatus::Current => "Установлена актуальная сборка.".into(),
        ClientUpdateStatus::Installed(build) => {
            format!("Сборка {build} установлена и откроется при следующем запуске tgrust.")
        }
        ClientUpdateStatus::Failed(error) => format!("Не удалось обновить клиент. {error}"),
    };
    message.set_text(&description);
    check.set_sensitive(status.can_check());
    check.set_visible(!matches!(
        status,
        ClientUpdateStatus::Disabled(_) | ClientUpdateStatus::Installed(_)
    ));
}
