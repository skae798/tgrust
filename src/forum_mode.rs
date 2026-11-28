use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::ForumMode;

type SaveFuture = Pin<Box<dyn Future<Output = Result<bool>> + Send>>;
pub type Save = Box<dyn Fn(ForumMode) -> SaveFuture>;

pub fn action(mode: ForumMode) -> &'static str {
    if mode.enabled {
        "Выключить темы"
    } else {
        "Включить темы"
    }
}

pub fn open(
    anchor: &gtk::Button,
    runtime: Handle,
    verified: ForumMode,
    save: impl Fn(ForumMode) -> SaveFuture + 'static,
    finish: impl Fn(Option<bool>) + 'static,
) {
    let title = format!("{}?", action(verified));
    let (dialog, body) = crate::chat_management::dialog(&title);
    let description = crate::contact_picker::caption();
    description.set_text(if verified.enabled {
        "Сообщения будут показаны в общей истории супергруппы."
    } else if verified.hidden_history {
        "Сообщения будут распределены по темам. История станет доступна новым участникам супергруппы."
    } else {
        "Супергруппа будет открываться со списком тем. Предыдущие сообщения будут доступны в общей теме."
    });
    body.append(&description);
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Отмена");
    let apply = gtk::Button::with_label(action(verified));
    apply.add_css_class("suggested-action");
    footer.append(&cancel);
    footer.append(&apply);
    body.append(&footer);
    let status = crate::contact_picker::caption();
    status.set_visible(false);
    body.append(&status);

    dialog.set_focus(Some(&cancel));
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    let attempted = Rc::new(Cell::new(false));
    let result = Rc::new(Cell::new(None));
    dialog.connect_closed({
        let attempted = attempted.clone();
        let result = result.clone();
        move |_| {
            if attempted.get() {
                finish(result.take());
            }
        }
    });
    apply.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        cancel,
        #[weak]
        status,
        move |button| {
            if !button.is_sensitive() {
                return;
            }
            attempted.set(true);
            dialog.set_can_close(false);
            cancel.set_sensitive(false);
            button.set_sensitive(false);
            status.set_text("Сохранение…");
            status.set_visible(true);
            let request = save(verified);
            let task = runtime.spawn(async move {
                tokio::time::timeout(Duration::from_secs(135), request)
                    .await
                    .context("Telegram не подтвердил изменение вовремя. Проверьте настройки группы перед повтором")?
            });
            let button = button.clone();
            let result = result.clone();
            glib::spawn_future_local(async move {
                let saved = task.await.map_err(anyhow::Error::from).and_then(|result| result);
                dialog.set_can_close(true);
                match saved {
                    Ok(enabled) => {
                        result.set(Some(enabled));
                        dialog.close();
                    }
                    Err(error) => {
                        status.set_text(&format!("Не удалось изменить режим тем. {error:#}"));
                        button.set_sensitive(true);
                        cancel.set_sensitive(true);
                        cancel.grab_focus();
                    }
                }
            });
        }
    ));
    dialog.present(Some(anchor));
}
