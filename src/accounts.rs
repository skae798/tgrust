use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::{Context, Result, ensure};
use gtk::{gio, glib};
use tokio::runtime::Runtime;

use crate::model::AccountProfile;
use crate::storage::SessionRemoval;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Account {
    Welcome,
    Demo(u8),
    Saved(AccountProfile),
}

impl Account {
    pub fn key(&self) -> String {
        match self {
            Self::Welcome => "welcome".into(),
            Self::Demo(n) => format!("demo-{n}"),
            Self::Saved(profile) => profile.key(),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Welcome => "tgrust".into(),
            Self::Demo(n) => format!("Демо {}", n + 1),
            Self::Saved(profile) => profile.label(),
        }
    }
}

pub struct Accounts {
    demo: bool,
    credentials: Result<Option<(i32, String)>>,
    app: glib::WeakRef<adw::Application>,
    windows: RefCell<HashMap<Account, WindowState>>,
}

enum WindowState {
    Open(glib::WeakRef<adw::ApplicationWindow>),
    Closing,
}

impl Accounts {
    pub fn new(app: &adw::Application, demo: bool) -> Rc<Self> {
        Rc::new(Self {
            demo,
            credentials: environment_credentials(),
            app: app.downgrade(),
            windows: RefCell::new(HashMap::new()),
        })
    }

    pub fn activate(self: &Rc<Self>) -> Result<()> {
        let app = self.app.upgrade().context("Приложение закрыто")?;
        if let Some(window) = app.active_window() {
            window.present();
            return Ok(());
        }
        if !self.windows.borrow().is_empty() {
            return Ok(());
        }
        if self.demo {
            return self.open(Account::Demo(0), None);
        }
        let saved = crate::storage::saved_accounts();
        match (&self.credentials, saved) {
            (Ok(Some((id, hash))), Ok(profiles)) => {
                let legacy = AccountProfile::legacy(*id);
                let mut matching = profiles.into_iter().filter(|profile| profile.api_id == *id);
                match (matching.next(), matching.next()) {
                    (None, _) => self.open(Account::Saved(legacy), Some(hash.clone())),
                    (Some(profile), None) => self.open(Account::Saved(profile), Some(hash.clone())),
                    _ => self.open(Account::Welcome, None),
                }
            }
            (Ok(None), Ok(profiles)) if profiles.len() == 1 => {
                self.open(Account::Saved(profiles.into_iter().next().unwrap()), None)
            }
            _ => self.open(Account::Welcome, None),
        }
    }

    pub fn startup_hint(&self) -> String {
        if let Err(error) = &self.credentials {
            return error.to_string();
        }
        match crate::storage::saved_accounts() {
            Ok(_) => "Выберите сохранённый аккаунт или добавьте новый.".into(),
            Err(error) => error.to_string(),
        }
    }

    pub fn open_demo(self: &Rc<Self>) -> Result<()> {
        self.open(Account::Demo(0), None)
    }

    fn open(self: &Rc<Self>, account: Account, hash: Option<String>) -> Result<()> {
        if let Some(state) = self.windows.borrow().get(&account) {
            match state {
                WindowState::Open(window) => {
                    if let Some(window) = window.upgrade() {
                        window.present();
                        return Ok(());
                    }
                }
                WindowState::Closing => anyhow::bail!("Аккаунт ещё закрывается. Попробуйте снова."),
            }
        }
        let app = self.app.upgrade().context("Приложение закрыто")?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .context("Не удалось запустить сетевые задачи аккаунта")?;
        let window = crate::ui::open(&app, self.clone(), account.clone(), hash, runtime);
        self.windows
            .borrow_mut()
            .insert(account, WindowState::Open(window.downgrade()));
        Ok(())
    }

    pub fn open_chat(&self, key: &str, chat: i64) {
        let window = self
            .windows
            .borrow()
            .iter()
            .find_map(|(account, state)| match state {
                WindowState::Open(window) if account.key() == key => window.upgrade(),
                _ => None,
            });
        if let Some(window) = window {
            let _ = gtk::prelude::WidgetExt::activate_action(
                &window,
                "win.open-chat",
                Some(&chat.to_variant()),
            );
        }
    }

    pub fn close(
        self: &Rc<Self>,
        account: Account,
        runtime: Runtime,
        removal: Option<SessionRemoval>,
        listener_finished: Option<tokio::sync::oneshot::Receiver<()>>,
    ) {
        let Some(app) = self.app.upgrade() else {
            drop(runtime);
            return;
        };
        self.windows
            .borrow_mut()
            .insert(account.clone(), WindowState::Closing);
        let hold = app.hold();
        let accounts = self.clone();
        let stopped = gio::spawn_blocking(move || {
            if let Some(finished) = listener_finished {
                runtime.block_on(async {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), finished).await;
                });
            }
            drop(runtime);
            if let Some(removal) = removal {
                removal.remove()?;
            }
            Ok::<(), anyhow::Error>(())
        });
        glib::MainContext::default().spawn_local(async move {
            let result = stopped
                .await
                .map_err(|_| anyhow::anyhow!("Не удалось остановить сетевые задачи"))
                .and_then(|result| result);
            accounts.windows.borrow_mut().remove(&account);
            if let Err(error) = result {
                let message = format!("Не удалось очистить сеанс «{}». {error:#}", account.label());
                eprintln!("{message}");
                let notice = adw::AlertDialog::builder()
                    .heading("Не удалось удалить локальную сессию")
                    .body(&message)
                    .build();
                notice.add_response("close", "Закрыть");
                notice.set_close_response("close");
                if let Some(window) = app.active_window() {
                    notice.present(Some(&window));
                } else {
                    // Keep the failure visible when logout closes the last window.
                    let window = adw::ApplicationWindow::builder()
                        .application(&app)
                        .title("tgrust")
                        .build();
                    window.present();
                    notice.choose_future(Some(&window)).await;
                    window.close();
                }
            }
            drop(hold);
        });
    }

    fn entries(&self, current: &Account) -> Result<Vec<Account>> {
        if self.demo || matches!(current, Account::Demo(_)) {
            return Ok(vec![Account::Demo(0), Account::Demo(1)]);
        }
        let mut accounts: Vec<_> = crate::storage::saved_accounts()?
            .into_iter()
            .map(Account::Saved)
            .collect();
        for account in self.windows.borrow().keys() {
            if matches!(account, Account::Saved(_)) && !accounts.contains(account) {
                accounts.push(account.clone());
            }
        }
        Ok(accounts)
    }

    pub fn chooser(
        self: &Rc<Self>,
        parent: &adw::ApplicationWindow,
        current: &Account,
    ) -> adw::Dialog {
        let dialog = adw::Dialog::builder()
            .title("Аккаунты")
            .content_width(460)
            .content_height(520)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let body = gtk::Box::new(gtk::Orientation::Vertical, 16);
        body.set_margin_top(12);
        body.set_margin_bottom(12);
        body.set_margin_start(12);
        body.set_margin_end(12);
        let status = gtk::Label::new(None);
        status.set_wrap(true);
        status.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        status.set_visible(false);
        body.append(&status);
        let saved = adw::PreferencesGroup::builder()
            .title("Сохранённые аккаунты")
            .build();
        body.append(&saved);
        match self.entries(current) {
            Ok(accounts) if accounts.is_empty() => {
                saved.set_description(Some("Сохранённых аккаунтов пока нет"));
            }
            Ok(accounts) => {
                for account in accounts {
                    let row = adw::ActionRow::builder()
                        .title(account.label())
                        .activatable(account != *current)
                        .build();
                    if account == *current {
                        row.set_subtitle("Текущий аккаунт");
                    } else {
                        if let Account::Saved(profile) = &account {
                            row.set_subtitle(&format!("API {}", profile.api_id));
                        }
                        let open = gtk::Button::builder()
                            .icon_name("go-next-symbolic")
                            .tooltip_text(format!("Открыть аккаунт {}", account.label()))
                            .valign(gtk::Align::Center)
                            .build();
                        open.add_css_class("flat");
                        open.update_property(&[gtk::accessible::Property::Label(&format!(
                            "Открыть аккаунт {}",
                            account.label()
                        ))]);
                        row.add_suffix(&open);
                        row.set_activatable_widget(Some(&open));
                        let controller = self.clone();
                        let close_welcome = matches!(current, Account::Welcome);
                        open.connect_clicked(glib::clone!(
                            #[weak]
                            dialog,
                            #[weak]
                            status,
                            #[weak]
                            parent,
                            move |_| {
                                let hash = match (&account, &controller.credentials) {
                                    (Account::Saved(profile), Ok(Some((id, hash))))
                                        if *id == profile.api_id =>
                                    {
                                        Some(hash.clone())
                                    }
                                    _ => None,
                                };
                                finish_open(
                                    controller.open(account.clone(), hash),
                                    &dialog,
                                    &status,
                                    close_welcome.then_some(&parent),
                                );
                            }
                        ));
                    }
                    saved.add(&row);
                }
            }
            Err(error) => show_error(&status, &error),
        }
        if !matches!(current, Account::Demo(_)) && !self.demo {
            self.append_add_form(
                &body,
                &dialog,
                &status,
                parent,
                matches!(current, Account::Welcome),
            );
        }
        let scroll = gtk::ScrolledWindow::builder()
            .child(&body)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        toolbar.set_content(Some(&scroll));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(parent));
        dialog
    }

    fn append_add_form(
        self: &Rc<Self>,
        body: &gtk::Box,
        dialog: &adw::Dialog,
        status: &gtk::Label,
        parent: &adw::ApplicationWindow,
        close_welcome: bool,
    ) {
        let group = adw::PreferencesGroup::builder()
            .title("Добавить аккаунт")
            .build();
        let name = adw::EntryRow::builder()
            .title("Метка, например work")
            .build();
        let id = adw::EntryRow::builder()
            .title("API ID")
            .input_purpose(gtk::InputPurpose::Digits)
            .build();
        let hash = adw::PasswordEntryRow::builder().title("API hash").build();
        if let Ok(Some((api_id, _))) = &self.credentials {
            id.set_text(&api_id.to_string());
        }
        group.add(&name);
        group.add(&id);
        group.add(&hash);
        body.append(&group);
        let submit = gtk::Button::with_label("Продолжить вход");
        submit.add_css_class("suggested-action");
        submit.set_halign(gtk::Align::End);
        body.append(&submit);
        let accounts = self.clone();
        submit.connect_clicked(glib::clone!(
            #[weak] dialog,
            #[weak] status,
            #[weak] name,
            #[weak] id,
            #[weak] hash,
            #[weak] parent,
            move |_| {
                let result = (|| {
                    let api_id = parse_api_id(id.text().trim())?;
                    let profile = AccountProfile::named(api_id, name.text().trim())?;
                    let password = hash.text();
                    let api_hash = match &accounts.credentials {
                        Ok(Some((known, value))) if *known == api_id && password.is_empty() => value.clone(),
                        _ => {
                            validate_hash(&password)?;
                            password.to_string()
                        }
                    };
                    let target = Account::Saved(profile.clone());
                    ensure!(!accounts.windows.borrow().contains_key(&target) && !crate::storage::saved_accounts()?.contains(&profile),
                        "Эта метка уже используется с данным API ID. Выберите аккаунт из списка или используйте другую метку.");
                    accounts.open(target, Some(api_hash))?;
                    hash.set_text("");
                    Ok(())
                })();
                finish_open(result, &dialog, &status, close_welcome.then_some(&parent));
            }
        ));
    }
}

fn finish_open(
    result: Result<()>,
    dialog: &adw::Dialog,
    status: &gtk::Label,
    welcome: Option<&adw::ApplicationWindow>,
) {
    match result {
        Ok(()) => {
            dialog.close();
            if let Some(welcome) = welcome {
                welcome.close();
            }
        }
        Err(error) => show_error(status, &error),
    }
}

fn show_error(status: &gtk::Label, error: &anyhow::Error) {
    status.set_text(&error.to_string());
    status.set_visible(true);
}

fn parse_api_id(value: &str) -> Result<i32> {
    let id: i32 = value.parse().context("API ID должен быть целым числом")?;
    ensure!(id > 0, "API ID должен быть положительным");
    Ok(id)
}

pub fn validate_hash(hash: &str) -> Result<()> {
    ensure!(
        hash.len() == 32 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "API hash должен содержать 32 шестнадцатеричных символа"
    );
    Ok(())
}

fn environment_credentials() -> Result<Option<(i32, String)>> {
    match (
        std::env::var("TGRUST_API_ID").ok(),
        std::env::var("TGRUST_API_HASH").ok(),
    ) {
        (None, None) => Ok(None),
        (Some(id), Some(hash)) => {
            let id = parse_api_id(&id)?;
            validate_hash(&hash)?;
            Ok(Some((id, hash)))
        }
        _ => anyhow::bail!("Укажите TGRUST_API_ID и TGRUST_API_HASH вместе"),
    }
}
