use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::contact_picker::caption;
use crate::model::{
    GroupPermission, GroupPermissionEdit, GroupPermissionSettings, SLOW_MODE_OPTIONS,
    slow_mode_change,
};

type SaveFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
pub type Save = Box<dyn Fn(GroupPermissionSettings, GroupPermissionEdit) -> SaveFuture>;

pub fn open(
    anchor: &gtk::Button,
    runtime: Handle,
    original: GroupPermissionSettings,
    save: impl Fn(GroupPermissionSettings, GroupPermissionEdit) -> SaveFuture + 'static,
    refresh: impl Fn() + 'static,
) {
    let (dialog, toolbar, body) = crate::chat_management::dialog_with_toolbar("Разрешения группы");
    let form = gtk::Box::new(gtk::Orientation::Vertical, 16);
    let rows = permission_rows(&form, original, "Недоступно для изменения");
    let slow_mode = original.slow_mode.map(|seconds| {
        let mut choices = SLOW_MODE_OPTIONS
            .map(|(value, label)| (value, label.to_owned()))
            .to_vec();
        if !choices.iter().any(|(value, _)| *value == seconds) {
            choices.push((seconds, format!("{seconds} с")));
        }
        let labels: Vec<_> = choices.iter().map(|(_, label)| label.as_str()).collect();
        let group = adw::PreferencesGroup::new();
        let row = adw::ComboRow::builder()
            .title("Медленный режим")
            .subtitle("Интервал между сообщениями участников")
            .model(&gtk::StringList::new(&labels))
            .selected(
                choices
                    .iter()
                    .position(|(value, _)| *value == seconds)
                    .unwrap() as u32,
            )
            .build();
        group.add(&row);
        form.append(&group);
        (
            row,
            choices.into_iter().map(|(seconds, _)| seconds).collect(),
        )
    });
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

    wire_link_previews(&dialog, &rows, original, "Недоступно для изменения");

    let editor = Rc::new(Editor {
        runtime,
        original,
        save: Box::new(save),
        dialog: dialog.downgrade(),
        form,
        rows,
        slow_mode,
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

pub(crate) fn permission_rows(
    form: &gtk::Box,
    original: GroupPermissionSettings,
    locked_reason: &str,
) -> Vec<(GroupPermission, adw::SwitchRow)> {
    let sending = adw::PreferencesGroup::builder()
        .title("Сообщения и вложения")
        .build();
    let actions = adw::PreferencesGroup::builder()
        .title("Действия участников")
        .build();
    use GroupPermission::Topics;
    let rows: Vec<_> = GroupPermission::OPTIONS
        .into_iter()
        .filter(|(permission, _)| *permission != Topics || original.forum)
        .map(|(permission, title)| {
            let row = adw::SwitchRow::builder()
                .title(title)
                .active(original.allowed.has(permission))
                .build();
            if !original.editable.has(permission) {
                row.set_subtitle(locked_reason);
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
    rows
}

pub(crate) fn wire_link_previews(
    dialog: &adw::Dialog,
    rows: &[(GroupPermission, adw::SwitchRow)],
    original: GroupPermissionSettings,
    locked_reason: &'static str,
) {
    use GroupPermission::{LinkPreviews, Text};
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
                    previews.set_subtitle(if editable { "" } else { locked_reason });
                } else {
                    remember.set(previews.is_active());
                    previews.set_active(false);
                    previews.set_subtitle(if editable {
                        "Разрешите отправку текста"
                    } else {
                        locked_reason
                    });
                }
                previews.set_sensitive(editable && text.is_active());
            }
        };
        sync(text);
        text.connect_active_notify(sync);
        dialog.set_focus(Some(text));
    }
}

struct Editor {
    runtime: Handle,
    original: GroupPermissionSettings,
    save: Save,
    dialog: glib::WeakRef<adw::Dialog>,
    form: gtk::Box,
    rows: Vec<(GroupPermission, adw::SwitchRow)>,
    slow_mode: Option<(adw::ComboRow, Vec<i32>)>,
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
        let requested = GroupPermissionEdit {
            allowed: self
                .rows
                .iter()
                .fold(self.original.allowed, |value, (permission, row)| {
                    value.with(*permission, row.is_active())
                }),
            slow_mode: self
                .slow_mode
                .as_ref()
                .and_then(|(row, choices)| choices.get(row.selected() as usize).copied()),
        };
        let changes = self
            .original
            .changes(self.original, requested.allowed)
            .and_then(|changes| {
                let interval = slow_mode_change(
                    self.original.slow_mode,
                    self.original.slow_mode,
                    requested.slow_mode,
                )?;
                Ok(!changes.is_empty() || interval.is_some())
            });
        match changes {
            Ok(false) => {
                self.feedback("Нет изменений.");
                return;
            }
            Err(error) => {
                self.feedback(&error.to_string());
                return;
            }
            Ok(true) => {}
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
