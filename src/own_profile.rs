use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::{OwnProfile, ProfileDetails, ProfileEdit, ProfileEditResult};

type Request<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;

pub struct ProfileContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn() -> Request<OwnProfile>>,
    pub save: Box<dyn Fn(OwnProfile, ProfileEdit) -> Request<ProfileEditResult>>,
    pub changed: Box<dyn Fn(&OwnProfile)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Details,
    Username,
}

struct Editor {
    context: Rc<ProfileContext>,
    first: adw::EntryRow,
    last: adw::EntryRow,
    about: gtk::TextView,
    username: adw::EntryRow,
    details_button: gtk::Button,
    username_button: gtk::Button,
    refresh: gtk::Button,
    status: gtk::Label,
    current: RefCell<Option<OwnProfile>>,
    busy: Cell<bool>,
    closed: Cell<bool>,
    read: RefCell<Option<tokio::task::AbortHandle>>,
}

pub fn open(window: &adw::ApplicationWindow, context: ProfileContext) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Мой профиль")
        .content_width(475)
        .content_height(560)
        .build();
    let header = adw::HeaderBar::new();
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Обновить профиль")
        .build();
    header.pack_end(&refresh);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(10);
    content.set_margin_bottom(16);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let name = adw::PreferencesGroup::new();
    let first = adw::EntryRow::builder().title("Имя").build();
    let last = adw::EntryRow::builder().title("Фамилия").build();
    name.add(&first);
    name.add(&last);
    content.append(&name);
    let about = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(8)
        .right_margin(8)
        .height_request(62)
        .build();
    about.update_property(&[gtk::accessible::Property::Label("О себе")]);
    let heading = gtk::Label::new(Some("О себе"));
    heading.add_css_class("heading");
    heading.set_xalign(0.0);
    content.append(&heading);
    let about_scroll = gtk::ScrolledWindow::builder()
        .child(&about)
        .min_content_height(62)
        .max_content_height(115)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .build();
    content.append(&gtk::Frame::builder().child(&about_scroll).build());
    let details_button = save_button("Сохранить профиль");
    content.append(&details_button);

    let public = adw::PreferencesGroup::builder()
        .description("Пустое поле удаляет текущее публичное имя пользователя.")
        .build();
    let username = adw::EntryRow::builder().title("Имя пользователя").build();
    public.add(&username);
    content.append(&public);
    let username_button = save_button("Сохранить имя пользователя");
    content.append(&username_button);
    let status = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    content.append(&status);

    let scroll = gtk::ScrolledWindow::builder()
        .child(&content)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));
    dialog.set_focus(Some(&refresh));
    let editor = Rc::new(Editor {
        context: Rc::new(context),
        first,
        last,
        about,
        username,
        details_button,
        username_button,
        refresh,
        status,
        current: RefCell::new(None),
        busy: Cell::new(false),
        closed: Cell::new(false),
        read: RefCell::new(None),
    });
    for (button, section) in [
        (&editor.details_button, Section::Details),
        (&editor.username_button, Section::Username),
    ] {
        button.connect_clicked(glib::clone!(
            #[weak]
            editor,
            move |_| editor.save(section)
        ));
    }
    editor.refresh.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |_| editor.load()
    ));
    editor.load();
    dialog.connect_closed(move |_| {
        editor.closed.set(true);
        if let Some(read) = editor.read.take() {
            read.abort();
        }
    });
    dialog.present(Some(window));
    dialog
}

fn save_button(text: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(text)
        .halign(gtk::Align::End)
        .css_classes(["suggested-action"])
        .build()
}

impl Editor {
    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        let ready = !busy && self.current.borrow().is_some();
        for widget in [
            self.first.upcast_ref::<gtk::Widget>(),
            self.last.upcast_ref(),
            self.about.upcast_ref(),
            self.username.upcast_ref(),
            self.details_button.upcast_ref(),
            self.username_button.upcast_ref(),
        ] {
            widget.set_sensitive(ready);
        }
    }

    fn details(&self) -> ProfileDetails {
        let buffer = self.about.buffer();
        let (start, end) = buffer.bounds();
        ProfileDetails {
            first_name: self.first.text().into(),
            last_name: self.last.text().into(),
            about: buffer.text(&start, &end, false).into(),
        }
    }

    fn show(&self, mut snapshot: OwnProfile, section: Option<Section>) {
        let baseline = self.current.borrow();
        if section != Some(Section::Username)
            || baseline
                .as_ref()
                .is_none_or(|old| self.details() == old.details)
        {
            self.first.set_text(&snapshot.details.first_name);
            self.last.set_text(&snapshot.details.last_name);
            self.about.buffer().set_text(&snapshot.details.about);
        } else if let Some(old) = baseline.as_ref() {
            snapshot.details = old.details.clone();
        }
        if section != Some(Section::Details)
            || baseline
                .as_ref()
                .is_none_or(|old| self.username.text().as_str() == old.username)
        {
            self.username.set_text(&snapshot.username);
        } else if let Some(old) = baseline.as_ref() {
            snapshot.username = old.username.clone();
        }
        drop(baseline);
        self.current.replace(Some(snapshot));
    }

    fn load(self: &Rc<Self>) {
        if self.closed.get() || self.busy.get() {
            return;
        }
        self.set_busy(true);
        self.status.set_text("Загрузка профиля…");
        let load = (self.context.load)();
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(20), load)
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
            let Some(editor) = weak.upgrade().filter(|editor| !editor.closed.get()) else {
                return;
            };
            editor.read.take();
            match result {
                Ok(snapshot) => {
                    (editor.context.changed)(&snapshot);
                    editor.show(snapshot, None);
                    editor.status.set_text("");
                }
                Err(error) => editor
                    .status
                    .set_text(&format!("Не удалось загрузить профиль. {error}")),
            }
            editor.set_busy(false);
        });
    }

    fn save(self: &Rc<Self>, section: Section) {
        if self.closed.get() || self.busy.get() {
            return;
        }
        let Some(baseline) = self.current.borrow().clone() else {
            return;
        };
        let edit = match section {
            Section::Details => ProfileEdit::Details(self.details()),
            Section::Username => ProfileEdit::Username(self.username.text().into()),
        };
        match crate::profile_edit::prepare(&baseline, &baseline, &edit) {
            Err(error) => {
                self.status.set_text(&error.to_string());
                return;
            }
            Ok(crate::profile_edit::Plan::Current) => {
                self.status.set_text("Изменений нет.");
                return;
            }
            _ => {}
        }
        self.refresh.grab_focus();
        self.set_busy(true);
        self.status.set_text("Сохранение…");
        let save = (self.context.save)(baseline, edit);
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(35), save)
                .await
                .context("Telegram не подтвердил сохранение вовремя")?
        });
        let context = self.context.clone();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            match &result {
                Ok(
                    ProfileEditResult::Saved(snapshot)
                    | ProfileEditResult::Conflict(snapshot)
                    | ProfileEditResult::Confirmed(snapshot),
                ) => (context.changed)(snapshot),
                Err(_) => {}
            }
            let Some(editor) = weak.upgrade().filter(|editor| !editor.closed.get()) else {
                return;
            };
            match result {
                Ok(result) => {
                    let (snapshot, message) = match result {
                        ProfileEditResult::Saved(snapshot) => (snapshot, "Сохранено."),
                        ProfileEditResult::Conflict(snapshot) => (
                            snapshot,
                            "Это поле уже изменилось в другом клиенте. Проверьте новое значение.",
                        ),
                        ProfileEditResult::Confirmed(snapshot) => (
                            snapshot,
                            "Telegram принял изменения. Обновите профиль, чтобы сверить описание.",
                        ),
                    };
                    editor.show(snapshot, Some(section));
                    editor.status.set_text(message);
                }
                Err(error) => editor
                    .status
                    .set_text(&format!("Не удалось подтвердить сохранение. {error}")),
            }
            editor.set_busy(false);
        });
    }
}
