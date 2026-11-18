use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::{gio, glib};
use tokio::runtime::Handle;

use crate::model::{OwnProfile, ProfileDetails, ProfileEdit, ProfileEditResult};
use crate::profile::AvatarLoader;

type Request<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;
type PhotoRequest = Box<dyn Fn(OwnProfile, Arc<[u8]>) -> Request<ProfileEditResult>>;

pub struct ProfileContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn() -> Request<OwnProfile>>,
    pub save: Box<dyn Fn(OwnProfile, ProfileEdit) -> Request<ProfileEditResult>>,
    pub set_photo: PhotoRequest,
    pub load_avatar: AvatarLoader,
    pub changed: Box<dyn Fn(&OwnProfile)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Details,
    Username,
    Photo,
}

struct Editor {
    context: Rc<ProfileContext>,
    avatar: adw::Bin,
    photo_button: gtk::Button,
    photo_status: gtk::Label,
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
        .content_height(620)
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

    let photo = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    let avatar = adw::Bin::new();
    avatar.set_child(Some(&adw::Avatar::new(64, None, false)));
    let photo_button = gtk::Button::builder()
        .label("Изменить фото")
        .valign(gtk::Align::Center)
        .build();
    photo.append(&avatar);
    photo.append(&photo_button);
    content.append(&photo);
    let photo_status = status_label();
    photo_status.set_visible(false);
    content.append(&photo_status);

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
    let status = status_label();
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
        avatar,
        photo_button,
        photo_status,
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
    editor.details_button.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |_| editor.save(ProfileEdit::Details(editor.details()))
    ));
    editor.username_button.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |_| editor.save(ProfileEdit::Username(editor.username.text().into()))
    ));
    editor.refresh.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |_| editor.load()
    ));
    editor.photo_button.connect_clicked(glib::clone!(
        #[weak]
        editor,
        #[weak]
        window,
        move |_| editor.choose_photo(&window)
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

fn status_label() -> gtk::Label {
    gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build()
}

impl Editor {
    fn set_save_status(&self, section: Section, message: &str) {
        if section == Section::Photo {
            self.photo_status.set_text(message);
            self.photo_status.set_visible(!message.is_empty());
        } else {
            self.status.set_text(message);
        }
    }

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
            self.photo_button.upcast_ref(),
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
        let name = snapshot.details.display_name();
        let avatar = adw::Avatar::new(64, Some(&name), true);
        if let Some(photo) = snapshot.avatar {
            (self.context.load_avatar)(&avatar, photo);
        }
        self.avatar.set_child(Some(&avatar));
        let baseline = self.current.borrow();
        if matches!(section, None | Some(Section::Details))
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
        if matches!(section, None | Some(Section::Username))
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

    fn choose_photo(self: &Rc<Self>, window: &adw::ApplicationWindow) {
        if self.closed.get() || self.busy.get() {
            return;
        }
        let Some(baseline) = self.current.borrow().clone() else {
            return;
        };
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("JPEG и PNG"));
        filter.add_mime_type("image/jpeg");
        filter.add_mime_type("image/png");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let picker = gtk::FileDialog::builder()
            .title("Выберите фото профиля")
            .filters(&filters)
            .build();
        let choice = picker.open_future(Some(window));
        let window = window.downgrade();
        let weak = Rc::downgrade(self);
        self.set_save_status(Section::Photo, "");
        self.set_busy(true);
        glib::MainContext::default().spawn_local(async move {
            let result = async {
                let file = match choice.await {
                    Ok(file) => file,
                    Err(error) if error.matches(gtk::DialogError::Dismissed) => return Ok(None),
                    Err(error) => return Err(error.into()),
                };
                let path = file.path().context("Выберите локальный файл")?;
                let jpeg: Arc<[u8]> =
                    gio::spawn_blocking(move || crate::profile_photo::prepare(&path))
                        .await
                        .map_err(|_| anyhow::anyhow!("Не удалось подготовить фотографию"))??
                        .into();
                let texture =
                    gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(jpeg.clone()))?;
                Ok::<_, anyhow::Error>(Some((jpeg, texture)))
            }
            .await;
            let Some(editor) = weak.upgrade().filter(|editor| !editor.closed.get()) else {
                return;
            };
            match result {
                Ok(Some((jpeg, texture))) if let Some(window) = window.upgrade() => {
                    let image = adw::Avatar::new(166, None, false);
                    image.set_custom_image(Some(&texture));
                    image.update_property(&[gtk::accessible::Property::Label("Предпросмотр фото")]);
                    image.set_halign(gtk::Align::Center);
                    let preview = adw::AlertDialog::builder()
                        .heading("Фото профиля")
                        .extra_child(&image)
                        .close_response("cancel")
                        .default_response("cancel")
                        .build();
                    preview.add_response("cancel", "Отмена");
                    preview.add_response("save", "Установить фото");
                    preview.set_response_appearance("save", adw::ResponseAppearance::Suggested);
                    let response = preview.choose_future(Some(&window));
                    drop(editor);
                    if response.await == "save" {
                        if let Some(editor) = weak.upgrade().filter(|editor| !editor.closed.get()) {
                            let request = (editor.context.set_photo)(baseline, jpeg);
                            editor.submit(Section::Photo, request);
                        }
                        return;
                    }
                }
                Err(error) => {
                    editor.set_save_status(Section::Photo, &format!("Фото не открыто. {error}"));
                }
                Ok(_) => {}
            }
            if let Some(editor) = weak.upgrade().filter(|editor| !editor.closed.get()) {
                editor.set_busy(false);
                editor.photo_button.grab_focus();
            }
        });
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
                    editor.set_save_status(Section::Photo, "");
                    editor.status.set_text("");
                }
                Err(error) => editor
                    .status
                    .set_text(&format!("Не удалось загрузить профиль. {error}")),
            }
            editor.set_busy(false);
        });
    }

    fn save(self: &Rc<Self>, edit: ProfileEdit) {
        if self.closed.get() || self.busy.get() {
            return;
        }
        let Some(baseline) = self.current.borrow().clone() else {
            return;
        };
        let section = match edit {
            ProfileEdit::Details(_) => Section::Details,
            ProfileEdit::Username(_) => Section::Username,
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
        let save = (self.context.save)(baseline, edit);
        self.submit(section, save);
    }

    fn submit(self: &Rc<Self>, section: Section, save: Request<ProfileEditResult>) {
        let photo = section == Section::Photo;
        self.refresh.grab_focus();
        self.set_busy(true);
        self.set_save_status(
            section,
            if photo {
                "Загрузка фото…"
            } else {
                "Сохранение…"
            },
        );
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
                        ProfileEditResult::Saved(snapshot) if photo => {
                            (snapshot, "Фото обновлено.")
                        }
                        ProfileEditResult::Saved(snapshot) => (snapshot, "Сохранено."),
                        ProfileEditResult::Conflict(snapshot) if photo => (
                            snapshot,
                            "Фото уже изменилось в другом клиенте. Проверьте новое фото.",
                        ),
                        ProfileEditResult::Conflict(snapshot) => (
                            snapshot,
                            "Это поле уже изменилось в другом клиенте. Проверьте новое значение.",
                        ),
                        ProfileEditResult::Confirmed(snapshot) if photo => (
                            snapshot,
                            "Telegram принял фото. Обновите профиль, чтобы сверить его.",
                        ),
                        ProfileEditResult::Confirmed(snapshot) => (
                            snapshot,
                            "Telegram принял изменения. Обновите профиль, чтобы сверить описание.",
                        ),
                    };
                    editor.show(snapshot, Some(section));
                    editor.set_save_status(section, message);
                }
                Err(error) => editor.set_save_status(
                    section,
                    &format!("Не удалось подтвердить сохранение. {error}"),
                ),
            }
            editor.set_busy(false);
        });
    }
}
