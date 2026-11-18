use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::model::ProxySettings;
use crate::proxy;

pub fn open(
    window: &adw::ApplicationWindow,
    demo: Option<Rc<RefCell<ProxySettings>>>,
) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Прокси")
        .content_width(460)
        .content_height(470)
        .build();
    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_top(12);
    body.set_margin_bottom(16);

    let mode = adw::SwitchRow::builder()
        .title("Подключаться через SOCKS5")
        .subtitle("При выключенном прокси используется прямое подключение")
        .build();
    let modes = adw::PreferencesGroup::new();
    modes.add(&mode);
    body.append(&modes);

    let host = adw::EntryRow::builder().title("Адрес сервера").build();
    let port = adw::EntryRow::builder()
        .title("Порт")
        .input_purpose(gtk::InputPurpose::Digits)
        .build();
    let username = adw::EntryRow::builder().title("Имя пользователя").build();
    let password = adw::PasswordEntryRow::builder().title("Пароль").build();
    let fields = adw::PreferencesGroup::builder()
        .description("Имя пользователя и пароль нужны, если их требует сервер.")
        .build();
    fields.add(&host);
    fields.add(&port);
    fields.add(&username);
    fields.add(&password);
    fields.set_sensitive(false);
    body.append(&fields);

    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["dim-label"])
        .build();
    body.append(&status);
    let save = gtk::Button::builder()
        .label("Сохранить")
        .halign(gtk::Align::End)
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    body.append(&save);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .focusable(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));
    dialog.set_focus(Some(&scroll));

    mode.connect_active_notify(glib::clone!(
        #[weak]
        fields,
        move |mode| fields.set_sensitive(mode.is_active())
    ));

    let form = Rc::new(Form {
        mode,
        host,
        port,
        username,
        password,
        fields,
        status,
        save,
        scroll,
        dialog: dialog.downgrade(),
        demo,
    });
    form.save.connect_clicked(glib::clone!(
        #[weak]
        form,
        move |_| form.save()
    ));
    form.load();
    dialog.connect_closed(move |_| {
        form.dialog.set(None);
    });
    dialog.present(Some(window));
    dialog
}

struct Form {
    mode: adw::SwitchRow,
    host: adw::EntryRow,
    port: adw::EntryRow,
    username: adw::EntryRow,
    password: adw::PasswordEntryRow,
    fields: adw::PreferencesGroup,
    status: gtk::Label,
    save: gtk::Button,
    scroll: gtk::ScrolledWindow,
    dialog: glib::WeakRef<adw::Dialog>,
    demo: Option<Rc<RefCell<ProxySettings>>>,
}

impl Form {
    fn set_busy(&self, busy: bool) {
        self.mode.set_sensitive(!busy);
        self.fields.set_sensitive(!busy && self.mode.is_active());
        self.save.set_sensitive(!busy);
    }

    fn show(&self, settings: &ProxySettings) {
        self.mode.set_active(settings.enabled);
        self.host.set_text(&settings.host);
        self.port.set_text(&settings.port);
        self.username.set_text(&settings.username);
        self.password.set_text(&settings.password);
        self.status
            .set_text("Настройки применяются при открытии окна аккаунта или повторе подключения на экране входа.");
    }

    fn load(self: &Rc<Self>) {
        if let Some(demo) = &self.demo {
            self.show(&demo.borrow());
            self.set_busy(false);
            return;
        }
        self.set_busy(true);
        self.status.set_text("Загрузка настроек…");
        let task = gio::spawn_blocking(proxy::load);
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(|_| anyhow::anyhow!("Чтение настроек прервано"))
                .and_then(|result| result);
            let Some(form) = weak
                .upgrade()
                .filter(|form| form.dialog.upgrade().is_some())
            else {
                return;
            };
            form.set_busy(false);
            match result {
                Ok(settings) => form.show(&settings),
                Err(error) => form
                    .status
                    .set_text(&format!("{error:#}. Можно сохранить новые параметры.")),
            }
        });
    }

    fn save(self: &Rc<Self>) {
        let settings = ProxySettings {
            enabled: self.mode.is_active(),
            host: self.host.text().to_string(),
            port: self.port.text().to_string(),
            username: self.username.text().to_string(),
            password: self.password.text().to_string(),
        };
        if let Err(error) = proxy::validate(&settings) {
            self.status.set_text(&error.to_string());
            return;
        }
        if let Some(demo) = &self.demo {
            demo.replace(settings);
            self.status.set_text("Сохранено в деморежиме.");
            return;
        }
        self.scroll.grab_focus();
        self.set_busy(true);
        self.status.set_text("Сохранение…");
        let task = gio::spawn_blocking(move || proxy::save(&settings));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(|_| anyhow::anyhow!("Сохранение настроек прервано"))
                .and_then(|result| result);
            let Some(form) = weak.upgrade().filter(|form| form.dialog.upgrade().is_some()) else {
                return;
            };
            form.set_busy(false);
            match result {
                Ok(()) => form.status.set_text("Сохранено. Для открытого аккаунта переоткройте его окно, чтобы применить настройки."),
                Err(error) => form.status.set_text(&format!("{error:#}")),
            }
        });
    }
}
