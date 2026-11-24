use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::{Context, Result, ensure};
use gtk::glib;
use tokio::runtime::Handle;

use crate::contact_picker::caption;
use crate::group_permissions::{permission_rows, wire_link_previews};
use crate::model::{
    GroupPermission, MemberRestrictionEdit, MemberRestrictionSettings, restriction_now,
};

pub type LoadFuture = Pin<Box<dyn Future<Output = Result<MemberRestrictionSettings>> + Send>>;
pub type SaveFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
pub type Load = Box<dyn Fn(i64) -> LoadFuture>;
pub type Save = Box<dyn Fn(i64, MemberRestrictionSettings, MemberRestrictionEdit) -> SaveFuture>;

pub fn open(
    anchor: &gtk::Button,
    runtime: Handle,
    name: &str,
    original: MemberRestrictionSettings,
    save: impl Fn(MemberRestrictionEdit) -> SaveFuture + 'static,
    refresh: impl Fn() + 'static,
) {
    let (dialog, toolbar, body) =
        crate::chat_management::dialog_with_toolbar("Ограничения участника");
    let heading = gtk::Label::builder()
        .label(name)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["heading"])
        .build();
    body.append(&heading);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let view = original.view();
    let reason = "Отключено в разрешениях группы";
    let rows = permission_rows(&form, view, reason);
    let duration = adw::PreferencesGroup::new();
    let options = if original.personal.active() {
        vec![
            Deadline::Keep,
            Deadline::Forever,
            Deadline::Day,
            Deadline::Week,
            Deadline::Custom,
        ]
    } else {
        vec![
            Deadline::Forever,
            Deadline::Day,
            Deadline::Week,
            Deadline::Custom,
        ]
    };
    let labels = options
        .iter()
        .map(|option| option.label(original.personal.until))
        .collect::<Vec<_>>();
    let labels = labels.iter().map(String::as_str).collect::<Vec<_>>();
    let term = adw::ComboRow::builder()
        .title("Срок ограничения")
        .model(&gtk::StringList::new(&labels))
        .build();
    duration.add(&term);
    let (day_row, days) = spin_row("Дни", 365.0);
    let (hour_row, hours) = spin_row("Часы", 23.0);
    let (minute_row, minutes) = spin_row("Минуты", 59.0);
    let custom_rows = [day_row, hour_row, minute_row];
    for row in &custom_rows {
        duration.add(row);
        row.set_visible(false);
    }
    term.connect_selected_notify({
        let options = options.clone();
        move |row| {
            let custom = options.get(row.selected() as usize) == Some(&Deadline::Custom);
            for row in &custom_rows {
                row.set_visible(custom);
            }
        }
    });
    form.append(&duration);
    body.append(&form);

    let footer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    footer.set_margin_start(16);
    footer.set_margin_end(16);
    footer.set_margin_top(8);
    footer.set_margin_bottom(12);
    let status = caption();
    status.set_visible(false);
    footer.append(&status);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let clear = gtk::Button::with_label("Снять ограничения");
    clear.add_css_class("destructive-action");
    clear.set_visible(original.personal.active());
    let submit = gtk::Button::with_label("Сохранить");
    submit.add_css_class("suggested-action");
    buttons.append(&clear);
    buttons.append(&submit);
    footer.append(&buttons);
    toolbar.add_bottom_bar(&footer);
    wire_link_previews(&dialog, &rows, view, reason);

    let editor = Rc::new(Editor {
        runtime,
        original,
        save: Box::new(save),
        dialog: dialog.downgrade(),
        rows,
        form,
        buttons,
        status,
        term,
        options,
        days,
        hours,
        minutes,
        chosen: Cell::new(None),
        attempted: Cell::new(false),
    });
    editor.term.connect_selected_notify(glib::clone!(
        #[weak]
        editor,
        move |_| editor.chosen.set(None)
    ));
    submit.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |_| editor.submit()
    ));
    clear.connect_clicked(glib::clone!(
        #[weak]
        editor,
        move |button| {
            let prompt = adw::AlertDialog::new(
                Some("Снять ограничения участника?"),
                Some("К участнику будут применены общие разрешения группы."),
            );
            prompt.add_responses(&[("cancel", "Отмена"), ("clear", "Снять")]);
            prompt.set_default_response(Some("cancel"));
            prompt.set_close_response("cancel");
            prompt.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
            if let Some(cancel) = prompt.default_widget() {
                prompt.set_focus(Some(&cancel));
            }
            prompt.connect_response(
                Some("clear"),
                glib::clone!(
                    #[weak]
                    editor,
                    move |_, _| editor.write(MemberRestrictionEdit::Clear)
                ),
            );
            prompt.present(Some(button));
        }
    ));
    dialog.connect_closed(move |_| {
        if editor.attempted.get() {
            refresh();
        }
    });
    dialog.present(Some(anchor));
}

fn spin_row(title: &str, max: f64) -> (adw::ActionRow, gtk::SpinButton) {
    let row = adw::ActionRow::builder().title(title).build();
    let spin = gtk::SpinButton::with_range(0.0, max, 1.0);
    spin.set_numeric(true);
    spin.set_valign(gtk::Align::Center);
    spin.update_property(&[gtk::accessible::Property::Label(title)]);
    row.add_suffix(&spin);
    (row, spin)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Deadline {
    Keep,
    Forever,
    Day,
    Week,
    Custom,
}

impl Deadline {
    fn label(self, until: i32) -> String {
        match self {
            Self::Keep if until == 0 => "Текущий срок · бессрочно".into(),
            Self::Keep => glib::DateTime::from_unix_local(i64::from(until))
                .and_then(|date| date.format("%d.%m.%Y %H:%M"))
                .map_or_else(|_| "Текущий срок".into(), |date| format!("До {date}")),
            Self::Forever => "Бессрочно".into(),
            Self::Day => "1 день".into(),
            Self::Week => "1 неделя".into(),
            Self::Custom => "Свой срок".into(),
        }
    }
}

struct Editor {
    runtime: Handle,
    original: MemberRestrictionSettings,
    save: Box<dyn Fn(MemberRestrictionEdit) -> SaveFuture>,
    dialog: glib::WeakRef<adw::Dialog>,
    rows: Vec<(GroupPermission, adw::SwitchRow)>,
    form: gtk::Box,
    buttons: gtk::Box,
    status: gtk::Label,
    term: adw::ComboRow,
    options: Vec<Deadline>,
    days: gtk::SpinButton,
    hours: gtk::SpinButton,
    minutes: gtk::SpinButton,
    chosen: Cell<Option<(i32, i32)>>,
    attempted: Cell<bool>,
}

impl Editor {
    fn deadline(&self) -> Result<Option<i32>> {
        let option = self
            .options
            .get(self.term.selected() as usize)
            .context("Выберите срок")?;
        let seconds = match option {
            Deadline::Keep => return Ok(None),
            Deadline::Forever => return Ok(Some(0)),
            Deadline::Day => 86_400,
            Deadline::Week => 7 * 86_400,
            Deadline::Custom => {
                for spin in [&self.days, &self.hours, &self.minutes] {
                    spin.update();
                }
                self.days.value_as_int() * 86_400
                    + self.hours.value_as_int() * 3_600
                    + self.minutes.value_as_int() * 60
            }
        };
        ensure!(seconds > 0, "Укажите срок от одной минуты");
        if let Some((chosen, until)) = self.chosen.get()
            && seconds == chosen
        {
            return Ok(Some(until));
        }
        let until = restriction_now()
            .checked_add(seconds)
            .context("Срок слишком большой")?;
        self.chosen.set(Some((seconds, until)));
        Ok(Some(until))
    }

    fn submit(self: &Rc<Self>) {
        let allowed = self.rows.iter().fold(
            self.original.view().allowed,
            |allowed, (permission, row)| allowed.with(*permission, row.is_active()),
        );
        let until = match self.deadline() {
            Ok(until) => until,
            Err(error) => return self.feedback(&error.to_string()),
        };
        self.write(MemberRestrictionEdit::Save { allowed, until });
    }

    fn feedback(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(true);
    }

    fn write(self: &Rc<Self>, edit: MemberRestrictionEdit) {
        let Some(dialog) = self.dialog.upgrade() else {
            return;
        };
        match self
            .original
            .prepare(self.original, edit, restriction_now())
        {
            Ok(None) => return self.feedback("Нет изменений."),
            Err(error) => return self.feedback(&error.to_string()),
            Ok(Some(_)) => {}
        }
        self.attempted.set(true);
        dialog.set_can_close(false);
        self.form.set_sensitive(false);
        self.buttons.set_sensitive(false);
        self.feedback("Сохранение ограничений…");
        let task = self.runtime.spawn((self.save)(edit));
        let editor = self.clone();
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            dialog.set_can_close(true);
            match result {
                Ok(()) => {
                    dialog.close();
                }
                Err(error) => {
                    editor.feedback(&format!("Не удалось сохранить ограничения. {error}"));
                    editor.form.set_sensitive(true);
                    editor.buttons.set_sensitive(true);
                }
            }
        });
    }
}
