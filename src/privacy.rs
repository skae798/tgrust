use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::{PrivacyAudience, PrivacyChange, PrivacyKey, PrivacySetting};

type Request<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;

pub struct PrivacyContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn(PrivacyKey) -> Request<PrivacySetting>>,
    pub save: Box<dyn Fn(PrivacyKey, PrivacySetting, PrivacyAudience) -> Request<PrivacyChange>>,
}

struct SettingRow {
    key: PrivacyKey,
    context: Rc<PrivacyContext>,
    choice: adw::ComboRow,
    save: gtk::Button,
    message: gtk::Label,
    detail: gtk::Label,
    current: RefCell<Option<PrivacySetting>>,
    task: RefCell<Option<tokio::task::AbortHandle>>,
    saving: Cell<bool>,
    closed: Cell<bool>,
}

pub fn open(window: &adw::ApplicationWindow, context: PrivacyContext) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Конфиденциальность")
        .content_width(500)
        .content_height(610)
        .build();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Обновить настройки конфиденциальности")
        .build();
    header.pack_end(&refresh);
    toolbar.add_top_bar(&header);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 20);
    body.set_margin_top(12);
    body.set_margin_bottom(18);
    body.set_margin_start(12);
    body.set_margin_end(12);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));
    dialog.set_focus(Some(&refresh));
    let context = Rc::new(context);
    let rows: Vec<_> = PrivacyKey::ALL
        .into_iter()
        .map(|key| SettingRow::new(key, context.clone(), &body, &refresh))
        .collect();

    refresh.connect_clicked({
        let rows: Vec<_> = rows.iter().map(Rc::downgrade).collect();
        move |_| {
            for row in rows.iter().filter_map(|row| row.upgrade()) {
                row.load();
            }
        }
    });
    for row in &rows {
        row.load();
    }
    dialog.connect_closed(move |_| {
        for row in &rows {
            row.closed.set(true);
            if !row.saving.get()
                && let Some(task) = row.task.take()
            {
                task.abort();
            }
        }
    });
    dialog.present(Some(window));
    dialog
}

impl SettingRow {
    fn new(
        key: PrivacyKey,
        context: Rc<PrivacyContext>,
        body: &gtk::Box,
        refresh: &gtk::Button,
    ) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder().title(key.title()).build();
        let block = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let choice = adw::ComboRow::builder()
            .title(key.question())
            .title_lines(2)
            .model(&gtk::StringList::new(&["Загрузка…"]))
            .build();
        group.add(&choice);
        block.append(&group);
        let detail = description();
        block.append(&detail);
        let message = description();
        block.append(&message);
        let save = gtk::Button::builder()
            .child(&gtk::Label::new(Some("Сохранить")))
            .halign(gtk::Align::End)
            .build();
        save.update_property(&[gtk::accessible::Property::Label(&format!(
            "Сохранить «{}»",
            key.title()
        ))]);
        save.add_css_class("suggested-action");
        block.append(&save);
        body.append(&block);

        let row = Rc::new(Self {
            key,
            context,
            choice,
            save,
            message,
            detail,
            current: RefCell::new(None),
            task: RefCell::new(None),
            saving: Cell::new(false),
            closed: Cell::new(false),
        });
        row.choice.connect_selected_notify(glib::clone!(
            #[weak]
            row,
            move |_| row.update_controls()
        ));
        row.save.connect_clicked(glib::clone!(
            #[weak]
            row,
            #[weak]
            refresh,
            move |_| row.save(&refresh)
        ));
        row
    }

    fn selected(&self) -> Option<PrivacyAudience> {
        self.key
            .audiences()
            .get(self.choice.selected() as usize)
            .copied()
    }

    fn update_controls(&self) {
        let current = self.current.borrow();
        let busy = self.task.borrow().is_some();
        let editable = !busy && current.as_ref().is_some_and(|value| value.editable);
        self.choice.set_sensitive(editable);
        let changed = editable
            && current.as_ref().and_then(|value| value.audience) != self.selected()
            && self.selected().is_some();
        self.save.set_sensitive(changed);
    }

    fn show(&self, setting: PrivacySetting, status: &str) {
        let index = setting
            .audience
            .and_then(|audience| {
                self.key
                    .audiences()
                    .iter()
                    .position(|option| *option == audience)
            })
            .map(|index| index as u32);
        let detail = if !setting.editable {
            "Измените эти правила в другом клиенте Telegram.".into()
        } else if setting.exception_rules > 0 {
            format!("Правил для исключений — {}", setting.exception_rules)
        } else {
            String::new()
        };
        self.detail.set_visible(!detail.is_empty());
        self.detail.set_text(&detail);
        self.current.replace(Some(setting));
        if let Some(index) = index {
            let choices: Vec<_> = self
                .key
                .audiences()
                .iter()
                .map(|option| option.label())
                .collect();
            self.choice.set_model(Some(&gtk::StringList::new(&choices)));
            self.choice.set_selected(index);
        } else {
            self.choice
                .set_model(Some(&gtk::StringList::new(&["Особые правила"])));
        }
        self.status(status);
    }

    fn status(&self, message: &str) {
        self.message.set_text(message);
        self.message.set_visible(!message.is_empty());
    }

    fn load(self: &Rc<Self>) {
        if self.closed.get() || self.task.borrow().is_some() {
            return;
        }
        self.status("Загрузка…");
        self.request((self.context.load)(self.key), |row, result| match result {
            Ok(setting) => row.show(setting, ""),
            Err(error) => {
                if row.current.borrow().is_none() {
                    row.choice
                        .set_model(Some(&gtk::StringList::new(&["Недоступно"])));
                }
                row.status(&format!("Не удалось загрузить. {error}"));
            }
        });
    }

    fn save(self: &Rc<Self>, refresh: &gtk::Button) {
        if self.closed.get() || self.task.borrow().is_some() {
            return;
        }
        let Some(audience) = self.selected() else {
            return;
        };
        let Some(baseline) = self.current.borrow().clone() else {
            return;
        };
        if !baseline.editable || baseline.audience == Some(audience) {
            return;
        }
        // Keep focus within the dialog when both editing controls become insensitive.
        refresh.grab_focus();
        self.saving.set(true);
        self.status("Сохранение…");
        self.request(
            (self.context.save)(self.key, baseline, audience),
            move |row, result| {
                row.saving.set(false);
                match result {
                    Ok(PrivacyChange::Saved {
                        setting,
                        exceptions_changed,
                    }) => {
                        let status = if setting.audience != Some(audience) {
                            "Telegram вернул другой набор правил."
                        } else if exceptions_changed {
                            "Telegram сохранил аудиторию и изменил правила исключений."
                        } else {
                            "Сохранено"
                        };
                        row.show(setting, status);
                    }
                    Ok(PrivacyChange::Conflict(setting)) => row.show(
                        setting,
                        "Настройка изменилась в другом клиенте. Проверьте новое значение.",
                    ),
                    Err(error) => {
                        row.status(&format!("Не удалось подтвердить сохранение. {error}"))
                    }
                }
            },
        );
    }

    fn request<T: Send + 'static>(
        self: &Rc<Self>,
        request: Request<T>,
        finish: impl FnOnce(&Self, Result<T>) + 'static,
    ) {
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(25), request)
                .await
                .context("Telegram не ответил вовремя")?
        });
        self.task.replace(Some(task.abort_handle()));
        self.update_controls();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|value| value);
            if let Some(row) = weak.upgrade()
                && !row.closed.get()
            {
                row.task.take();
                finish(&row, result);
                row.update_controls();
            }
        });
    }
}

fn description() -> gtk::Label {
    let label = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .visible(false)
        .build();
    label.add_css_class("dim-label");
    label
}
