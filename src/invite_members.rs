use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::contact_picker::{ContactPicker, LoadContacts, caption};
use crate::model::{Contact, MemberInvitation, MemberInvitations};

type Invite = Pin<Box<dyn Future<Output = Result<MemberInvitations>> + Send>>;
pub type OpenInvite = Box<dyn Fn(&gtk::Button, Box<dyn Fn()>)>;
pub type OpenChatInvite = Box<dyn Fn(&gtk::Button, bool, Box<dyn Fn()>)>;

pub struct InviteContext {
    pub runtime: Handle,
    pub contacts: LoadContacts,
    pub invite: Box<dyn Fn(Vec<Contact>) -> Invite>,
    pub changed: Box<dyn Fn()>,
}

pub fn open(anchor: &gtk::Button, allow_bots: bool, context: InviteContext) {
    let (dialog, toolbar, body) =
        crate::chat_management::dialog_with_toolbar("Добавить участников");
    let picker = ContactPicker::new(context.runtime.clone(), context.contacts);
    picker.configure("Выберите участников", allow_bots);
    body.append(&picker.widget);

    let status = caption();
    status.set_visible(false);
    body.append(&status);
    let footer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    footer.set_margin_start(16);
    footer.set_margin_end(16);
    footer.set_margin_top(8);
    footer.set_margin_bottom(12);
    let add = gtk::Button::builder()
        .label("Добавить")
        .halign(gtk::Align::End)
        .css_classes(["suggested-action"])
        .build();
    footer.append(&add);
    toolbar.add_bottom_bar(&footer);

    let attempted = Rc::new(Cell::new(false));
    dialog.connect_closed({
        let attempted = attempted.clone();
        let picker = picker.clone();
        move |_| {
            picker.cancel_load();
            if attempted.get() {
                (context.changed)();
            }
        }
    });
    add.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        status,
        #[strong]
        picker,
        #[strong]
        attempted,
        move |add| {
            if attempted.get() {
                dialog.close();
                return;
            }
            let selected = picker.chosen();
            if selected.is_empty() {
                status.set_visible(true);
                status.set_text("Выберите хотя бы одного участника.");
                return;
            }
            attempted.set(true);
            picker.widget.set_visible(false);
            add.set_sensitive(false);
            dialog.set_can_close(false);
            status.set_visible(true);
            status.set_text("Добавление участников…");
            let task = context.runtime.spawn((context.invite)(selected));
            let add = add.clone();
            glib::spawn_future_local(async move {
                let result = task
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                let feedback = match result {
                    Ok(outcomes) => invitation_feedback(&outcomes),
                    Err(error) => format!("Не удалось добавить участников. {error}"),
                };
                status.set_text(&feedback);
                dialog.set_can_close(true);
                add.set_label("Готово");
                add.set_sensitive(true);
                add.grab_focus();
            });
        }
    ));
    dialog.set_focus(Some(&picker.search));
    dialog.present(Some(anchor));
}

fn invitation_feedback(outcomes: &MemberInvitations) -> String {
    outcomes
        .iter()
        .map(|(name, status)| {
            let status = match status {
                MemberInvitation::InChat => "В чате",
                MemberInvitation::Rejected(reason) => reason,
                MemberInvitation::Unconfirmed => {
                    "Telegram не подтвердил результат. Сверьте список участников"
                }
                MemberInvitation::NotSent => "Приглашение не отправлено",
            };
            format!("{name} · {status}")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
