use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::{Context, Result, ensure};
use gtk::{gio, glib};
use tokio::runtime::Runtime;

use crate::model::{AccountIdentity, AccountProfile, ProxySettings};
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
    app: glib::WeakRef<adw::Application>,
    windows: RefCell<HashMap<Account, WindowState>>,
    identities: RefCell<HashMap<AccountProfile, AccountIdentity>>,
    pub demo_proxy: Rc<RefCell<ProxySettings>>,
}

enum WindowState {
    Open(glib::WeakRef<adw::ApplicationWindow>),
    Closing,
}

impl Accounts {
    pub fn new(app: &adw::Application, demo: bool) -> Rc<Self> {
        Rc::new(Self {
            demo,
            app: app.downgrade(),
            windows: RefCell::new(HashMap::new()),
            identities: RefCell::new(HashMap::new()),
            demo_proxy: Rc::new(RefCell::new(ProxySettings::default())),
        })
    }

    pub fn activate(self: &Rc<Self>) -> Result<()> {
        let app = self.app.upgrade().context("Приложение закрыто")?;
        if let Some(window) = app.active_window() {
            if gtk::prelude::WidgetExt::activate_action(&window, "viewer.show-chat", None).is_err()
            {
                window.present();
            }
            return Ok(());
        }
        if !self.windows.borrow().is_empty() {
            return Ok(());
        }
        if self.demo {
            return self.open(Account::Demo(0), None);
        }
        let saved = crate::storage::saved_accounts();
        match (application_credentials(), saved) {
            (Ok(Some((id, hash))), Ok(profiles)) => {
                if profiles.is_empty() {
                    self.open(Account::Saved(AccountProfile::legacy(id)), Some(hash))
                } else if profiles.len() == 1 {
                    let profile = profiles.into_iter().next().unwrap();
                    let hash = (profile.api_id == id).then_some(hash);
                    self.open(Account::Saved(profile), hash)
                } else {
                    self.open(Account::Welcome, None)
                }
            }
            (Ok(None), Ok(profiles)) if profiles.len() == 1 => {
                self.open(Account::Saved(profiles.into_iter().next().unwrap()), None)
            }
            _ => self.open(Account::Welcome, None),
        }
    }

    pub fn startup_hint(&self) -> String {
        if let Err(error) = application_credentials() {
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

    pub fn identity(&self, account: &Account) -> Option<AccountIdentity> {
        let Account::Saved(profile) = account else {
            return None;
        };
        self.identities
            .borrow()
            .get(profile)
            .cloned()
            .or_else(|| crate::storage::account_identity(profile))
    }

    pub fn display_name(&self, account: &Account) -> String {
        self.identity(account)
            .filter(|identity| !identity.name.is_empty())
            .map_or_else(|| account.label(), |identity| identity.name)
    }

    pub fn signed_in_ids(&self, current: &Account) -> Vec<i64> {
        let mut ids: Vec<_> = self
            .entries(current)
            .unwrap_or_default()
            .into_iter()
            .filter(|account| account != current)
            .filter_map(|account| self.identity(&account).map(|identity| identity.user_id))
            .filter(|id| *id > 0)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub fn remember_identity(&self, account: &Account, identity: AccountIdentity) {
        if let Account::Saved(profile) = account
            && self.identity(account).as_ref() != Some(&identity)
        {
            let _ = crate::storage::remember_account_identity(profile, &identity);
            self.identities
                .borrow_mut()
                .insert(profile.clone(), identity);
        }
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
            if let Account::Saved(profile) = &account {
                accounts.identities.borrow_mut().remove(profile);
            }
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
                    let display = self.display_name(&account);
                    let row = adw::ActionRow::builder()
                        .activatable(account != *current)
                        .build();
                    row.set_use_markup(false);
                    row.set_title(&display);
                    let badge = adw::Avatar::new(34, Some(&display), true);
                    badge.set_valign(gtk::Align::Center);
                    row.add_prefix(&badge);
                    if account == *current {
                        row.set_subtitle("Текущий аккаунт");
                    } else {
                        if let Some(identity) = self.identity(&account)
                            && !identity.username.is_empty()
                        {
                            row.set_subtitle(&format!("@{}", identity.username));
                        }
                        let open = gtk::Button::builder()
                            .icon_name("go-next-symbolic")
                            .tooltip_text(format!("Открыть аккаунт {display}"))
                            .valign(gtk::Align::Center)
                            .build();
                        open.add_css_class("flat");
                        open.update_property(&[gtk::accessible::Property::Label(&format!(
                            "Открыть аккаунт {}",
                            display
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
                                let hash = match (&account, application_credentials()) {
                                    (Account::Saved(profile), Ok(Some((id, hash))))
                                        if id == profile.api_id =>
                                    {
                                        Some(hash)
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

    fn start_account(self: &Rc<Self>, api_id: i32, hash: String) -> Result<()> {
        let mut profiles = crate::storage::saved_accounts()?;
        profiles.extend(self.windows.borrow().keys().filter_map(|account| {
            if let Account::Saved(profile) = account {
                Some(profile.clone())
            } else {
                None
            }
        }));
        self.open(Account::Saved(next_profile(api_id, &profiles)?), Some(hash))
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
        let credentials = application_credentials().ok().flatten();
        if credentials.is_some() {
            let open = gtk::Button::with_label("Добавить аккаунт");
            open.set_halign(gtk::Align::Start);
            open.add_css_class("suggested-action");
            group.add(&open);
            let accounts = self.clone();
            open.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                #[weak]
                status,
                #[weak]
                parent,
                move |_| {
                    let result = application_credentials().and_then(|keys| {
                        let (id, hash) = keys.context("Сначала настройте ключи приложения")?;
                        accounts.start_account(id, hash)
                    });
                    finish_open(result, &dialog, &status, close_welcome.then_some(&parent));
                }
            ));
        }
        let setup = adw::ExpanderRow::builder()
            .title("Ключи приложения Telegram")
            .expanded(credentials.is_none())
            .build();
        let id = adw::EntryRow::builder()
            .title("API ID")
            .input_purpose(gtk::InputPurpose::Digits)
            .build();
        let hash = adw::PasswordEntryRow::builder().title("API hash").build();
        if let Some((configured, _)) = credentials {
            id.set_text(&configured.to_string());
        }
        let hint = gtk::Label::new(Some(
            "Настраивается один раз. Используйте ключи своего приложения с my.telegram.org.",
        ));
        hint.set_wrap(true);
        hint.set_xalign(0.0);
        hint.set_margin_start(10);
        hint.set_margin_end(10);
        hint.set_margin_top(6);
        hint.set_margin_bottom(6);
        setup.add_row(&hint);
        setup.add_row(&id);
        setup.add_row(&hash);
        let submit = gtk::Button::with_label("Сохранить ключи и войти");
        submit.add_css_class("suggested-action");
        submit.set_halign(gtk::Align::Start);
        submit.set_margin_start(10);
        submit.set_margin_top(6);
        submit.set_margin_bottom(6);
        setup.add_row(&submit);
        group.add(&setup);
        body.append(&group);

        let accounts = self.clone();
        submit.connect_clicked(glib::clone!(
            #[weak]
            dialog,
            #[weak]
            status,
            #[weak]
            id,
            #[weak]
            hash,
            #[weak]
            parent,
            move |_| {
                let result = (|| {
                    let id = parse_api_id(id.text().trim())?;
                    let api_hash = hash.text().to_string();
                    validate_hash(&api_hash)?;
                    crate::storage::save_application_credentials(id, &api_hash)
                        .context("Не удалось сохранить ключи приложения")?;
                    hash.set_text("");
                    accounts.start_account(id, api_hash)
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

fn next_profile(api_id: i32, profiles: &[AccountProfile]) -> Result<AccountProfile> {
    if !profiles.iter().any(|profile| profile.api_id == api_id) {
        return Ok(AccountProfile::legacy(api_id));
    }
    for number in 2..=profiles.len().saturating_add(2) {
        let candidate = AccountProfile::named(api_id, &format!("account-{number}"))?;
        if !profiles.contains(&candidate) {
            return Ok(candidate);
        }
    }
    anyhow::bail!("Не удалось выбрать локальный номер аккаунта")
}

fn application_credentials() -> Result<Option<(i32, String)>> {
    let environment = (
        std::env::var("TGRUST_API_ID").ok(),
        std::env::var("TGRUST_API_HASH").ok(),
    );
    match environment {
        (Some(id), Some(hash)) => return checked_credentials(&id, hash).map(Some),
        (None, None) => {}
        _ => anyhow::bail!("Укажите TGRUST_API_ID и TGRUST_API_HASH вместе"),
    }
    if let Some((id, hash)) = crate::storage::application_credentials()? {
        return checked_credentials(&id.to_string(), hash).map(Some);
    }
    match (
        option_env!("TGRUST_BUILD_API_ID"),
        option_env!("TGRUST_BUILD_API_HASH"),
    ) {
        (Some(id), Some(hash)) => checked_credentials(id, hash.to_owned()).map(Some),
        (None, None) => Ok(None),
        _ => anyhow::bail!("В сборке нужна пара ключей приложения Telegram"),
    }
}

fn checked_credentials(id: &str, hash: String) -> Result<(i32, String)> {
    let id = parse_api_id(id)?;
    validate_hash(&hash)?;
    Ok((id, hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_account_slots_avoid_legacy_and_named_session_collisions() {
        let legacy = AccountProfile::legacy(123);
        let second = AccountProfile::named(123, "account-2").unwrap();
        assert_eq!(next_profile(123, &[]).unwrap(), legacy);
        assert_eq!(
            next_profile(123, std::slice::from_ref(&legacy)).unwrap(),
            second
        );
        assert_eq!(
            next_profile(123, &[legacy, second]).unwrap(),
            AccountProfile::named(123, "account-3").unwrap()
        );
        assert_eq!(next_profile(321, &[]).unwrap(), AccountProfile::legacy(321));
    }
}
