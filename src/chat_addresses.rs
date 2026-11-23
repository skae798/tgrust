use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::contact_picker::caption;
use crate::model::{AttachedAddress, ChatAddresses};

pub type EditAddressesFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

pub fn open(
    anchor: &gtk::Button,
    runtime: Handle,
    original: ChatAddresses,
    save: impl Fn(ChatAddresses, ChatAddresses) -> EditAddressesFuture + 'static,
    refresh: impl Fn() + 'static,
) {
    let (dialog, toolbar, body) = crate::chat_management::dialog_with_toolbar("Публичные адреса");
    let form = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let basic = adw::PreferencesGroup::new();
    let primary = adw::EntryRow::builder()
        .title("Основное публичное имя")
        .text(&original.primary)
        .build();
    basic.add(&primary);
    form.append(&basic);
    let info = caption();
    info.set_text("Пустое основное имя освобождает адрес. Активное дополнительное имя сохраняет публичный доступ.");
    form.append(&info);
    let aliases = adw::PreferencesGroup::builder()
        .title("Дополнительные имена")
        .build();
    let mut rows = Vec::new();
    for address in &original.aliases {
        let row = adw::SwitchRow::builder()
            .title(format!("@{}", address.name))
            .use_markup(false)
            .active(address.active)
            .build();
        if !address.can_toggle {
            row.set_subtitle("Только просмотр");
            row.set_sensitive(false);
        }
        aliases.add(&row);
        rows.push((address.clone(), row));
    }
    if !rows.is_empty() {
        form.append(&aliases);
    }
    let visibility = caption();
    form.append(&visibility);
    body.append(&form);

    let footer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    footer.set_margin_start(16);
    footer.set_margin_end(16);
    footer.set_margin_top(8);
    footer.set_margin_bottom(12);
    let status = caption();
    status.set_visible(false);
    footer.append(&status);
    let submit = gtk::Button::builder()
        .label("Сохранить")
        .halign(gtk::Align::End)
        .css_classes(["suggested-action"])
        .build();
    footer.append(&submit);
    toolbar.add_bottom_bar(&footer);

    let editor = Rc::new(Editor {
        runtime,
        original,
        save: Box::new(save),
        dialog: dialog.downgrade(),
        form,
        primary,
        rows,
        visibility,
        status,
        submit,
        writing: Cell::new(false),
        attempted: Cell::new(false),
    });
    editor.primary.connect_changed(glib::clone!(
        #[weak]
        editor,
        move |_| editor.update_visibility()
    ));
    for (_, row) in &editor.rows {
        row.connect_active_notify(glib::clone!(
            #[weak]
            editor,
            move |_| editor.update_visibility()
        ));
    }
    editor.submit.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |_| editor.submit()
    ));
    dialog.connect_closed({
        let editor = editor.clone();
        move |_| {
            if editor.attempted.get() {
                refresh();
            }
        }
    });
    editor.update_visibility();
    dialog.set_focus(Some(&editor.primary));
    dialog.present(Some(anchor));
}

struct Editor {
    runtime: Handle,
    original: ChatAddresses,
    save: Box<dyn Fn(ChatAddresses, ChatAddresses) -> EditAddressesFuture>,
    dialog: glib::WeakRef<adw::Dialog>,
    form: gtk::Box,
    primary: adw::EntryRow,
    rows: Vec<(AttachedAddress, adw::SwitchRow)>,
    visibility: gtk::Label,
    status: gtk::Label,
    submit: gtk::Button,
    writing: Cell<bool>,
    attempted: Cell<bool>,
}

impl Editor {
    fn proposed(&self) -> ChatAddresses {
        ChatAddresses {
            primary: self.primary.text().trim().to_owned(),
            aliases: self
                .rows
                .iter()
                .map(|(address, row)| AttachedAddress {
                    active: row.is_active(),
                    ..address.clone()
                })
                .collect(),
        }
    }

    fn update_visibility(&self) {
        self.visibility.set_text(if self.proposed().is_public() {
            "Публичный чат"
        } else {
            "Закрытый чат"
        });
    }

    fn feedback(&self, message: &str) {
        self.status.set_text(message);
        self.status.set_visible(true);
    }

    fn submit(self: &Rc<Self>) {
        if self.writing.get() {
            return;
        }
        let proposed = self.proposed();
        match proposed.changes(&self.original, &self.original) {
            Ok(changes) if changes.is_empty() => {
                self.feedback("Нет изменений.");
                return;
            }
            Err(error) => {
                self.feedback(&error.to_string());
                return;
            }
            _ => {}
        }
        if self.original.is_public() && !proposed.is_public() {
            let prompt = adw::AlertDialog::new(
                Some("Закрыть публичный доступ?"),
                Some(
                    "Публичные адреса перестанут открывать этот чат. Пригласительные ссылки продолжат работать.",
                ),
            );
            prompt.add_responses(&[("cancel", "Отмена"), ("confirm", "Закрыть доступ")]);
            prompt.set_default_response(Some("cancel"));
            prompt.set_close_response("cancel");
            prompt.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
            if let Some(cancel) = prompt.default_widget() {
                prompt.set_focus(Some(&cancel));
            }
            prompt.connect_response(
                Some("confirm"),
                glib::clone!(
                    #[weak(rename_to = editor)]
                    self,
                    move |_, _| editor.write(proposed.clone())
                ),
            );
            prompt.present(Some(&self.submit));
        } else {
            self.write(proposed);
        }
    }

    fn write(self: &Rc<Self>, proposed: ChatAddresses) {
        let Some(dialog) = self.dialog.upgrade() else {
            return;
        };
        if self.writing.replace(true) {
            return;
        }
        self.attempted.set(true);
        dialog.set_can_close(false);
        self.form.set_sensitive(false);
        self.submit.set_sensitive(false);
        self.feedback("Сохранение адресов…");
        let request = (self.save)(self.original.clone(), proposed);
        let task = self.runtime.spawn(request);
        let editor = self.clone();
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            editor.writing.set(false);
            dialog.set_can_close(true);
            match result {
                Ok(()) => {
                    dialog.close();
                }
                Err(error) => {
                    editor.feedback(&format!("Не удалось сохранить адреса. {error}"));
                    editor.form.set_sensitive(true);
                    editor.submit.set_sensitive(true);
                }
            }
        });
    }
}
