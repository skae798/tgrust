use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::contact_picker::caption;
use crate::model::{GroupPermission, GroupPermissionSettings, GroupPermissions};

type SaveFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
pub type Save = Box<dyn Fn(GroupPermissionSettings, GroupPermissions) -> SaveFuture>;

pub fn open(
    anchor: &gtk::Button,
    runtime: Handle,
    original: GroupPermissionSettings,
    save: impl Fn(GroupPermissionSettings, GroupPermissions) -> SaveFuture + 'static,
    refresh: impl Fn() + 'static,
) {
    let (dialog, toolbar, body) = crate::chat_management::dialog_with_toolbar("Разрешения группы");
    let form = gtk::Box::new(gtk::Orientation::Vertical, 16);
    let sending = adw::PreferencesGroup::builder()
        .title("Сообщения и вложения")
        .build();
    let actions = adw::PreferencesGroup::builder()
        .title("Действия участников")
        .build();
    use GroupPermission::{LinkPreviews, Text, Topics};
    let rows: Vec<_> = GroupPermission::OPTIONS
        .into_iter()
        .filter(|(permission, _)| *permission != Topics || original.forum)
        .map(|(permission, title)| {
            let row = adw::SwitchRow::builder()
                .title(title)
                .active(original.allowed.has(permission))
                .build();
            if !original.editable.has(permission) {
                row.set_subtitle("Недоступно для изменения");
                row.set_sensitive(false);
            }
            if permission.is_sending() {
                sending.add(&row);
            } else {
                actions.add(&row);
            }
            (permission, row)
        })
        .collect();
    form.append(&sending);
    form.append(&actions);
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

    if let Some((_, text)) = rows.iter().find(|(permission, _)| *permission == Text)
        && let Some((_, previews)) = rows
            .iter()
            .find(|(permission, _)| *permission == LinkPreviews)
    {
        let editable = original.editable.has(LinkPreviews);
        let remember = Cell::new(previews.is_active());
        let sync = {
            let previews = previews.clone();
            move |text: &adw::SwitchRow| {
                if text.is_active() {
                    previews.set_active(remember.get());
                    previews.set_subtitle(if editable {
                        ""
                    } else {
                        "Недоступно для изменения"
                    });
                } else {
                    remember.set(previews.is_active());
                    previews.set_active(false);
                    previews.set_subtitle("Разрешите отправку текста");
                }
                previews.set_sensitive(editable && text.is_active());
            }
        };
        sync(text);
        text.connect_active_notify(sync);
        dialog.set_focus(Some(text));
    }

    let editor = Rc::new(Editor {
        runtime,
        original,
        save: Box::new(save),
        dialog: dialog.downgrade(),
        form,
        rows,
        status,
        submit,
        writing: Cell::new(false),
        attempted: Cell::new(false),
    });
    editor.submit.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |_| editor.submit()
    ));
    dialog.connect_closed(move |_| {
        if editor.attempted.get() {
            refresh();
        }
    });
    dialog.present(Some(anchor));
}

struct Editor {
    runtime: Handle,
    original: GroupPermissionSettings,
    save: Save,
    dialog: glib::WeakRef<adw::Dialog>,
    form: gtk::Box,
    rows: Vec<(GroupPermission, adw::SwitchRow)>,
    status: gtk::Label,
    submit: gtk::Button,
    writing: Cell<bool>,
    attempted: Cell<bool>,
}

impl Editor {
    fn feedback(&self, message: &str) {
        self.status.set_text(message);
        self.status.set_visible(true);
    }

    fn submit(self: &Rc<Self>) {
        if self.writing.get() {
            return;
        }
        let requested = self
            .rows
            .iter()
            .fold(self.original.allowed, |value, (permission, row)| {
                value.with(*permission, row.is_active())
            });
        match self.original.changes(self.original, requested) {
            Ok(changes) if changes.is_empty() => {
                self.feedback("Нет изменений.");
                return;
            }
            Err(error) => {
                self.feedback(&error.to_string());
                return;
            }
            Ok(_) => {}
        }
        let Some(dialog) = self.dialog.upgrade() else {
            return;
        };
        self.writing.set(true);
        self.attempted.set(true);
        dialog.set_can_close(false);
        self.form.set_sensitive(false);
        self.submit.set_sensitive(false);
        self.feedback("Сохранение разрешений…");
        let task = self.runtime.spawn((self.save)(self.original, requested));
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
                    editor.feedback(&format!("Не удалось сохранить разрешения. {error}"));
                    editor.form.set_sensitive(true);
                    editor.submit.set_sensitive(true);
                }
            }
        });
    }
}
