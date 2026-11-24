use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::forum_topics::WriteFuture;
use crate::model::{
    ForumPermissions, ForumTopic, ForumTopicChange, ForumWrite, ForumWriteResult, forum_topic_title,
};

pub fn open(
    anchor: &gtk::Button,
    runtime: Handle,
    permissions: ForumPermissions,
    topic: Option<ForumTopic>,
    write: impl Fn(ForumWrite) -> WriteFuture + 'static,
    completed: impl Fn(Option<ForumWriteResult>) + 'static,
) {
    let title = if topic.is_some() {
        "Управление темой"
    } else {
        "Новая тема"
    };
    let (dialog, body) = crate::chat_management::dialog(title);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let can_rename = topic.as_ref().is_none_or(|topic| {
        permissions.allows(topic, &ForumTopicChange::Rename(topic.title.clone()))
    });
    let name = adw::EntryRow::builder()
        .title("Название темы")
        .text(topic.as_ref().map_or("", |topic| topic.title.as_str()))
        .build();
    let submit = if can_rename {
        let fields = adw::PreferencesGroup::new();
        fields.add(&name);
        form.append(&fields);
        let button = gtk::Button::with_label(if topic.is_some() {
            "Сохранить название"
        } else {
            "Создать тему"
        });
        button.add_css_class("suggested-action");
        button.set_halign(gtk::Align::End);
        form.append(&button);
        dialog.set_focus(Some(&name));
        Some(button)
    } else {
        let title = crate::contact_picker::caption();
        title.set_text(topic.as_ref().map_or("", |topic| topic.title.as_str()));
        form.append(&title);
        None
    };
    body.append(&form);
    let status = crate::contact_picker::caption();
    status.set_visible(false);
    body.append(&status);

    let editor = Rc::new(Editor {
        runtime,
        topic,
        write: Box::new(write),
        dialog: dialog.downgrade(),
        form,
        name,
        status,
        writing: Cell::new(false),
        attempted: Cell::new(false),
        attempt: RefCell::new(None),
        result: RefCell::new(None),
    });
    if let Some(submit) = submit {
        submit.connect_clicked(glib::clone!(
            #[weak]
            editor,
            move |_| editor.submit_title()
        ));
        editor.name.connect_entry_activated(glib::clone!(
            #[weak]
            editor,
            move |_| editor.submit_title()
        ));
    }
    if let Some(topic) = &editor.topic {
        for (label, change) in [
            (
                if topic.closed {
                    "Открыть тему"
                } else {
                    "Закрыть тему"
                },
                ForumTopicChange::Closed(!topic.closed),
            ),
            (
                if topic.pinned {
                    "Открепить тему"
                } else {
                    "Закрепить тему"
                },
                ForumTopicChange::Pinned(!topic.pinned),
            ),
            (
                if topic.hidden {
                    "Показать общую тему"
                } else {
                    "Скрыть общую тему"
                },
                ForumTopicChange::Hidden(!topic.hidden),
            ),
            ("Удалить тему", ForumTopicChange::Delete),
        ] {
            if !permissions.allows(topic, &change) {
                continue;
            }
            let button = gtk::Button::with_label(label);
            if matches!(change, ForumTopicChange::Delete) {
                button.add_css_class("destructive-action");
                button.connect_clicked(glib::clone!(
                    #[weak]
                    editor,
                    move |button| editor.confirm_delete(button)
                ));
            } else {
                button.connect_clicked(glib::clone!(
                    #[weak]
                    editor,
                    move |_| editor.change(change.clone())
                ));
            }
            editor.form.append(&button);
        }
    }

    dialog.connect_closed(move |_| {
        if editor.attempted.get() {
            completed(editor.result.take());
        }
    });
    dialog.present(Some(anchor));
}

struct Editor {
    runtime: Handle,
    topic: Option<ForumTopic>,
    write: Box<dyn Fn(ForumWrite) -> WriteFuture>,
    dialog: glib::WeakRef<adw::Dialog>,
    form: gtk::Box,
    name: adw::EntryRow,
    status: gtk::Label,
    writing: Cell<bool>,
    attempted: Cell<bool>,
    attempt: RefCell<Option<(String, i64)>>,
    result: RefCell<Option<ForumWriteResult>>,
}

impl Editor {
    fn submit_title(self: &Rc<Self>) {
        let title = match forum_topic_title(&self.name.text()) {
            Ok(title) => title,
            Err(error) => return self.feedback(&error.to_string()),
        };
        if self.topic.is_some() {
            self.change(ForumTopicChange::Rename(title));
            return;
        }
        let random_id = {
            let mut attempt = self.attempt.borrow_mut();
            if let Some((_, id)) = attempt.as_ref().filter(|(previous, _)| previous == &title) {
                *id
            } else {
                let id = (i64::from(glib::random_int()) << 32) | i64::from(glib::random_int()) | 1;
                *attempt = Some((title.clone(), id));
                id
            }
        };
        self.run(ForumWrite::Create { title, random_id });
    }

    fn change(self: &Rc<Self>, change: ForumTopicChange) {
        let Some(topic) = &self.topic else {
            return;
        };
        match change.needs_write(topic, topic) {
            Ok(false) => return self.feedback("Нет изменений."),
            Err(error) => return self.feedback(&error.to_string()),
            Ok(true) => {}
        }
        self.run(ForumWrite::Change {
            original: topic.clone(),
            change,
        });
    }

    fn confirm_delete(self: &Rc<Self>, anchor: &gtk::Button) {
        let Some(topic) = &self.topic else {
            return;
        };
        let body = format!(
            "«{}» и все сообщения в ней будут удалены у участников.",
            topic.title
        );
        let confirm = adw::AlertDialog::new(Some("Удалить тему?"), Some(&body));
        confirm.add_responses(&[("cancel", "Отмена"), ("delete", "Удалить тему")]);
        confirm.set_default_response(Some("cancel"));
        confirm.set_close_response("cancel");
        confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        if let Some(cancel) = confirm.default_widget() {
            confirm.set_focus(Some(&cancel));
        }
        confirm.connect_response(
            Some("delete"),
            glib::clone!(
                #[weak(rename_to = editor)]
                self,
                move |_, _| editor.change(ForumTopicChange::Delete)
            ),
        );
        confirm.present(Some(anchor));
    }

    fn feedback(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(true);
    }

    fn run(self: &Rc<Self>, write: ForumWrite) {
        let Some(dialog) = self.dialog.upgrade() else {
            return;
        };
        if self.writing.replace(true) {
            return;
        }
        let creating = matches!(write, ForumWrite::Create { .. });
        let deleting = matches!(
            &write,
            ForumWrite::Change {
                change: ForumTopicChange::Delete,
                ..
            }
        );
        self.attempted.set(true);
        dialog.set_can_close(false);
        self.form.set_sensitive(false);
        self.feedback(if deleting {
            "Удаление темы…"
        } else {
            "Сохранение темы…"
        });
        let request = (self.write)(write);
        let task = self.runtime.spawn(async move {
            match tokio::time::timeout(Duration::from_secs(120), request).await {
                Ok(result) => result,
                Err(_) if creating => Ok(ForumWriteResult::CreationUnconfirmed),
                Err(_) => {
                    anyhow::bail!("Telegram не подтвердил изменение вовремя. Обновите список тем")
                }
            }
        });
        let editor = self.clone();
        glib::spawn_future_local(async move {
            let result: Result<_> = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            editor.writing.set(false);
            dialog.set_can_close(true);
            match result {
                Ok(ForumWriteResult::CreationUnconfirmed) => {
                    editor.name.set_editable(false);
                    editor.form.set_sensitive(true);
                    editor.feedback("Создание пока не подтверждено. Повторите запрос или закройте окно и проверьте список тем.");
                }
                Ok(result) => {
                    editor.result.replace(Some(result));
                    dialog.close();
                }
                Err(error) => {
                    editor.form.set_sensitive(true);
                    editor.feedback(&format!("Не удалось сохранить изменение. {error}"));
                }
            }
        });
    }
}
