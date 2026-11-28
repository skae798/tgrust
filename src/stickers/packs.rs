use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;

use super::Picker;
use crate::model::{StickerCollection, StickerSource};

impl Picker {
    pub(super) fn ask_pack(self: &Rc<Self>) {
        if self.closed.get() || self.sending.get() || self.editing.get() {
            return;
        }
        let Some(parent) = self.dialog.upgrade() else {
            return;
        };
        let dialog = adw::AlertDialog::builder()
            .heading("Открыть набор стикеров")
            .close_response("cancel")
            .default_response("open")
            .build();
        dialog.add_response("cancel", "Отмена");
        dialog.add_response("open", "Открыть");
        let input = gtk::Entry::builder()
            .max_length(512)
            .placeholder_text("Имя или ссылка t.me/addstickers/…")
            .build();
        input.update_property(&[gtk::accessible::Property::Label(
            "Имя или ссылка на набор стикеров",
        )]);
        let hint = gtk::Label::builder()
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(30)
            .xalign(0.0)
            .build();
        hint.add_css_class("dim-label");
        let form = gtk::Box::new(gtk::Orientation::Vertical, 6);
        form.append(&input);
        form.append(&hint);
        dialog.set_extra_child(Some(&form));
        dialog.set_focus(Some(&input));
        dialog.set_response_enabled("open", false);
        input.connect_activate(glib::clone!(
            #[weak]
            dialog,
            move |_| {
                if dialog.is_response_enabled("open")
                    && let Some(button) = dialog.default_widget().and_downcast::<gtk::Button>()
                {
                    button.emit_clicked();
                }
            }
        ));
        input.connect_changed(glib::clone!(
            #[weak]
            dialog,
            #[weak]
            hint,
            move |input| {
                let result = crate::model::sticker_pack_name(input.text().as_str());
                dialog.set_response_enabled("open", result.is_ok());
                hint.set_text(
                    &result
                        .err()
                        .map(|error| error.to_string())
                        .unwrap_or_default(),
                );
            }
        ));
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak(rename_to = state)]
            self,
            async move {
                if dialog.choose_future(Some(&parent)).await == "open" {
                    state.open_named_pack(input.text().as_str());
                }
            }
        ));
    }

    pub(super) fn open_named_pack(self: &Rc<Self>, input: &str) {
        if self.closed.get() || self.sending.get() || self.editing.get() {
            return;
        }
        let name = match crate::model::sticker_pack_name(input) {
            Ok(name) => name,
            Err(error) => {
                self.status.set_text(&error.to_string());
                return;
            }
        };
        self.pending_pack.replace(Some(name.clone()));
        self.retry.set_visible(false);
        self.cancel_loading();
        if let Some(client) = self.context.client.clone() {
            self.controls_sensitive(false);
            self.status.set_text("Загрузка набора…");
            self.run(
                async move { client.open_sticker_pack(&name).await },
                |state, result| {
                    state.controls_sensitive(true);
                    match result {
                        Ok(collection) => state.select_collection(collection),
                        Err(error) => state.fail(error),
                    }
                },
            );
        } else {
            let result = self.context.demo_packs.borrow_mut().open(&name);
            match result {
                Ok(collection) => self.select_collection(collection),
                Err(error) => self.fail(error),
            }
        }
    }

    fn select_collection(self: &Rc<Self>, collection: StickerCollection) {
        self.pending_pack.take();
        self.search.set_text("");
        let index = {
            let mut collections = self.collections.borrow_mut();
            if let Some(index) = collections
                .iter()
                .position(|item| item.source == collection.source)
            {
                collections[index] = collection;
                index
            } else {
                collections.push(collection);
                collections.len() - 1
            }
        };
        let title = self.collections.borrow()[index].title.clone();
        let old = self.source.selected();
        if index < self.names.n_items() as usize {
            self.names.splice(index as u32, 1, &[&title]);
        } else {
            self.names.append(&title);
        }
        self.source.set_selected(index as u32);
        if old == index as u32 {
            self.load();
        }
    }

    pub(super) fn ask_pack_change(self: &Rc<Self>) {
        if self.closed.get() || self.sending.get() || self.editing.get() {
            return;
        }
        let Some(collection) = self
            .collections
            .borrow()
            .get(self.source.selected() as usize)
            .cloned()
        else {
            return;
        };
        let (StickerSource::Set(id), Some(installed)) = (collection.source, collection.installed)
        else {
            return;
        };
        if !installed {
            self.change_pack(id, true);
            return;
        }
        let Some(parent) = self.dialog.upgrade() else {
            return;
        };
        let dialog = adw::AlertDialog::builder()
            .heading("Убрать набор стикеров?")
            .body(format!(
                "{}. Отправленные сообщения останутся.",
                collection.title
            ))
            .close_response("cancel")
            .default_response("cancel")
            .build();
        dialog.add_response("cancel", "Отмена");
        dialog.add_response("remove", "Убрать");
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak(rename_to = state)]
            self,
            async move {
                if dialog.choose_future(Some(&parent)).await == "remove" {
                    state.change_pack(id, false);
                }
            }
        ));
    }

    fn change_pack(self: &Rc<Self>, id: i64, installed: bool) {
        if self.closed.get() || self.sending.get() || self.editing.replace(true) {
            return;
        }
        if let Some(client) = self.context.client.clone() {
            self.controls_sensitive(false);
            self.status.set_text("Изменение набора…");
            let task = self.context.runtime.spawn(async move {
                tokio::time::timeout(
                    Duration::from_secs(35),
                    client.set_sticker_pack_installed(id, installed),
                )
                .await
                .context("Результат неизвестен. Откройте набор заново")?
            });
            glib::MainContext::default().spawn_local(glib::clone!(
                #[weak(rename_to = state)]
                self,
                async move {
                    let result = task
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|result| result);
                    state.editing.set(false);
                    if state.closed.get() {
                        return;
                    }
                    state.controls_sensitive(true);
                    state.finish_pack_change(result);
                }
            ));
        } else {
            let result = self.context.demo_packs.borrow_mut().change(id, installed);
            self.editing.set(false);
            self.finish_pack_change(result);
        }
    }

    pub(super) fn finish_pack_change(
        self: &Rc<Self>,
        result: Result<crate::model::StickerPackChange>,
    ) {
        match result {
            Ok(change) => {
                self.notice.replace(Some(match change.archived {
                    Some(count) if count > 0 => format!("Набор добавлен. Telegram архивировал {count} других наборов. Откройте список заново для обновления."),
                    _ if change.collection.installed == Some(true) => "Набор добавлен".into(),
                    _ => "Набор убран из установленных".into(),
                }));
                self.select_collection(change.collection);
                self.pack_action.grab_focus();
            }
            Err(error) => {
                self.status
                    .set_text(&format!("Не удалось изменить набор. {error}"));
                self.retry.set_visible(true);
            }
        }
    }
}
