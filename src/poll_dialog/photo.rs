use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tokio::runtime::Handle;

use crate::poll_photo::Photo;

#[derive(Clone)]
pub(super) struct Controls(Rc<State>);

struct State {
    runtime: Handle,
    window: glib::WeakRef<adw::ApplicationWindow>,
    row: adw::ActionRow,
    picture: gtk::Picture,
    choose: gtk::Button,
    remove: gtk::Button,
    spoiler: adw::SwitchRow,
    selected: RefCell<Option<Photo>>,
    chooser: RefCell<Option<gtk::gio::Cancellable>>,
    task: RefCell<Option<tokio::task::AbortHandle>>,
    generation: Cell<u64>,
    busy: Cell<bool>,
    closed: Cell<bool>,
}

impl Controls {
    pub(super) fn append(
        window: &adw::ApplicationWindow,
        dialog: &adw::Dialog,
        content: &gtk::Box,
        runtime: Handle,
    ) -> Self {
        let group = adw::PreferencesGroup::new();
        let row = adw::ActionRow::builder()
            .title("Фото к вопросу")
            .subtitle("Без фото")
            .use_markup(false)
            .build();
        let choose = gtk::Button::with_label("Выбрать");
        choose.set_valign(gtk::Align::Center);
        choose.update_property(&[gtk::accessible::Property::Label("Выбрать фото к вопросу")]);
        let remove = gtk::Button::from_icon_name("user-trash-symbolic");
        remove.set_valign(gtk::Align::Center);
        remove.set_visible(false);
        remove.set_tooltip_text(Some("Убрать фото"));
        remove.update_property(&[gtk::accessible::Property::Label("Убрать фото")]);
        row.add_suffix(&choose);
        row.add_suffix(&remove);
        row.set_activatable_widget(Some(&choose));
        group.add(&row);
        let picture = gtk::Picture::builder()
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Contain)
            .height_request(140)
            .visible(false)
            .build();
        picture.update_property(&[gtk::accessible::Property::Label("Фото к вопросу")]);
        group.add(&picture);
        let spoiler = adw::SwitchRow::builder()
            .title("Скрыть фото спойлером")
            .visible(false)
            .build();
        group.add(&spoiler);
        content.append(&group);
        let state = Rc::new(State {
            runtime,
            window: window.downgrade(),
            row,
            picture,
            choose,
            remove,
            spoiler,
            selected: RefCell::default(),
            chooser: RefCell::default(),
            task: RefCell::default(),
            generation: Cell::new(0),
            busy: Cell::new(false),
            closed: Cell::new(false),
        });
        state.choose.connect_clicked(glib::clone!(
            #[weak]
            state,
            move |_| state.choose()
        ));
        state.remove.connect_clicked(glib::clone!(
            #[weak]
            state,
            move |_| state.clear()
        ));
        dialog.connect_closed({
            let state = state.clone();
            move |_| {
                state.closed.set(true);
                state.clear();
            }
        });
        Self(state)
    }

    pub(super) fn selection(&self) -> anyhow::Result<Option<Photo>> {
        anyhow::ensure!(!self.0.closed.get(), "Редактор закрыт");
        anyhow::ensure!(
            !self.0.busy.get() && self.0.chooser.borrow().is_none(),
            "Дождитесь подготовки фото"
        );
        Ok(self
            .0
            .selected
            .borrow()
            .clone()
            .map(|photo| photo.with_spoiler(self.0.spoiler.is_active())))
    }

    #[cfg(test)]
    pub(super) fn load(&self, path: PathBuf) {
        self.0.load(path);
    }
}

impl State {
    fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(request) = self.chooser.take() {
            request.cancel();
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.busy.set(false);
    }

    fn clear(&self) {
        self.cancel();
        self.selected.take();
        self.picture.set_paintable(None::<&gtk::gdk::Texture>);
        self.picture.set_visible(false);
        self.spoiler.set_active(false);
        self.spoiler.set_visible(false);
        self.row.set_subtitle("Без фото");
        self.choose.set_label("Выбрать");
        self.choose.set_sensitive(true);
        self.remove.set_visible(false);
    }

    fn choose(self: &Rc<Self>) {
        if self.closed.get() || self.busy.get() || self.chooser.borrow().is_some() {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        self.cancel();
        let generation = self.generation.get();
        let picker = gtk::FileDialog::builder()
            .title("Фото к вопросу опроса")
            .build();
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Фото JPEG, PNG или WebP"));
        for mime in ["image/jpeg", "image/png", "image/webp"] {
            filter.add_mime_type(mime);
        }
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        picker.set_filters(Some(&filters));
        let cancel = gtk::gio::Cancellable::new();
        self.chooser.replace(Some(cancel.clone()));
        self.choose.set_sensitive(false);
        picker.open(
            Some(&window),
            Some(&cancel),
            glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |result| {
                    if state.closed.get() || state.generation.get() != generation {
                        return;
                    }
                    state.chooser.take();
                    state.choose.set_sensitive(true);
                    if let Ok(file) = result {
                        if let Some(path) = file.path() {
                            state.load(path);
                        } else {
                            state.failed("Выберите локальный файл с фото");
                        }
                    }
                }
            ),
        );
    }

    fn load(self: &Rc<Self>, path: PathBuf) {
        if self.closed.get() {
            return;
        }
        self.cancel();
        let generation = self.generation.get();
        self.busy.set(true);
        self.row.set_subtitle("Подготовка фото…");
        self.choose.set_sensitive(false);
        self.remove.set_visible(true);
        let task = self.runtime.spawn(crate::poll_photo::load(path));
        self.task.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(state) = weak.upgrade() else {
                return;
            };
            if state.closed.get() || state.generation.get() != generation {
                return;
            }
            state.task.take();
            state.busy.set(false);
            state.choose.set_sensitive(true);
            match result {
                Ok(photo) => {
                    match gtk::gdk::Texture::from_bytes(&glib::Bytes::from(photo.preview())) {
                        Ok(texture) => {
                            state.picture.set_paintable(Some(&texture));
                            state.picture.set_visible(true);
                            state.spoiler.set_visible(true);
                            state.row.set_subtitle(photo.name());
                            state.choose.set_label("Заменить");
                            state.selected.replace(Some(photo));
                        }
                        Err(error) => state.failed(&error.to_string()),
                    }
                }
                Err(error) => state.failed(&error.to_string()),
            }
            state.remove.set_visible(state.selected.borrow().is_some());
        });
    }

    fn failed(&self, error: &str) {
        self.row
            .set_subtitle(&self.selected.borrow().as_ref().map_or_else(
                || format!("Не удалось подготовить фото: {error}"),
                |photo| format!("Осталось прежнее фото «{}». {error}", photo.name()),
            ));
    }
}
