use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::model::{MAX_POLL_CHOICES, Poll, PollDraft};

use crate::model::{PollDeadline, unix_now};

type SendPoll = Pin<Box<dyn Future<Output = anyhow::Result<()>>>>;

fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(32)
        .build()
}

pub fn card(poll: &Poll) -> gtk::Box {
    let show_results = poll.results_visible();
    let masked = (!show_results).then(|| {
        let mut poll = poll.clone();
        poll.total = None;
        poll.solution = None;
        for choice in &mut poll.choices {
            choice.correct = false;
            choice.votes = None;
        }
        poll
    });
    let poll = masked.as_ref().unwrap_or(poll);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let question = label(&poll.question);
    question.add_css_class("heading");
    card.append(&question);
    let kind = match (poll.quiz, poll.public) {
        (true, false) => "Анонимная викторина",
        (true, true) => "Публичная викторина",
        (false, false) => "Анонимный опрос",
        (false, true) => "Публичный опрос",
    };
    let kind = label(kind);
    kind.add_css_class("dim-label");
    card.append(&kind);
    for choice in &poll.choices {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let answer = label(&choice.label);
        answer.set_hexpand(true);
        if choice.chosen {
            answer.add_css_class("accent");
            answer.update_property(&[gtk::accessible::Property::Description("Ваш голос")]);
        }
        row.append(&answer);
        if choice.chosen || choice.correct {
            let icon = gtk::Image::from_icon_name("object-select-symbolic");
            icon.update_property(&[gtk::accessible::Property::Label(if choice.correct {
                "Правильный ответ"
            } else {
                "Ваш голос"
            })]);
            row.append(&icon);
        }
        if let Some(votes) = choice.votes {
            let count = label(&votes.to_string());
            count.add_css_class("dim-label");
            row.append(&count);
        }
        card.append(&row);
        if let Some(votes) = choice.votes
            && let Some(total) = poll.total.filter(|total| *total > 0)
        {
            let bar = gtk::ProgressBar::new();
            bar.set_fraction((f64::from(votes) / f64::from(total)).min(1.0));
            bar.update_property(&[gtk::accessible::Property::Label(&choice.label)]);
            card.append(&bar);
        }
    }
    if let Some(solution) = &poll.solution {
        card.append(&label(solution));
    }
    let status = match (poll.closed, poll.total) {
        (true, Some(total)) => format!("Опрос закрыт · Всего проголосовало: {total}"),
        (true, None) => "Опрос закрыт".to_owned(),
        (false, Some(total)) => format!("Всего проголосовало: {total}"),
        (false, None) => String::new(),
    };
    let mut elapsed = false;
    if !poll.closed
        && let Some(deadline) = poll.closes_at
    {
        elapsed = deadline <= unix_now();
        if !elapsed
            && let Ok(time) = glib::DateTime::from_unix_local(deadline)
            && let Ok(text) = time.format("%x %H:%M")
        {
            let deadline = label(&format!("До {text}"));
            deadline.add_css_class("dim-label");
            card.append(&deadline);
        }
    }
    let status = if elapsed {
        "Приём голосов завершён. Ожидаются итоги Telegram.".into()
    } else if !show_results {
        "Итоги появятся после завершения".into()
    } else {
        status
    };
    if !status.is_empty() {
        let status = label(&status);
        status.add_css_class("dim-label");
        card.append(&status);
    }
    card
}

pub fn vote(
    window: &adw::ApplicationWindow,
    poll: &Poll,
    submit: impl FnOnce(Vec<Vec<u8>>) + 'static,
) {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.append(&label(&poll.question));
    let mut controls: Vec<(gtk::CheckButton, Vec<u8>)> = Vec::new();
    for choice in &poll.choices {
        let option = gtk::CheckButton::new();
        option.set_child(Some(&label(&choice.label)));
        option.update_property(&[gtk::accessible::Property::Label(&choice.label)]);
        if !poll.multiple
            && let Some((first, _)) = controls.first()
        {
            option.set_group(Some(first));
        }
        option.set_active(choice.chosen);
        body.append(&option);
        controls.push((option, choice.option.clone()));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .max_content_height(300)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let dialog = adw::AlertDialog::builder()
        .heading("Голосование")
        .extra_child(&scroll)
        .default_response("cancel")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Отмена");
    if poll.has_vote() {
        dialog.add_response("retract", "Отозвать голос");
    }
    dialog.add_response("vote", "Голосовать");
    dialog.set_response_appearance("vote", adw::ResponseAppearance::Suggested);
    let buttons: Vec<_> = controls
        .iter()
        .map(|(button, _)| button.downgrade())
        .collect();
    let update = Rc::new(glib::clone!(
        #[weak]
        dialog,
        move || {
            dialog.set_response_enabled(
                "vote",
                buttons
                    .iter()
                    .filter_map(glib::WeakRef::upgrade)
                    .any(|button| button.is_active()),
            );
        }
    ));
    update();
    for (button, _) in &controls {
        let update = update.clone();
        button.connect_toggled(move |_| update());
    }
    let response = dialog.choose_future(Some(window));
    glib::MainContext::default().spawn_local(async move {
        match response.await.as_str() {
            "vote" => submit(
                controls
                    .into_iter()
                    .filter_map(|(button, key)| button.is_active().then_some(key))
                    .collect(),
            ),
            "retract" => submit(Vec::new()),
            _ => {}
        }
    });
}

pub fn create(
    window: &adw::ApplicationWindow,
    send: impl Fn(PollDraft, i64) -> SendPoll + 'static,
) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Новый опрос")
        .content_width(430)
        .content_height(555)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let question = adw::EntryRow::builder().title("Вопрос").build();
    let question_group = adw::PreferencesGroup::new();
    question_group.add(&question);
    content.append(&question_group);
    let answers = adw::PreferencesGroup::builder()
        .title("Варианты ответа")
        .build();
    let choices = Rc::new(RefCell::new(Vec::<adw::EntryRow>::new()));
    let append = |rows: &mut Vec<adw::EntryRow>, group: &adw::PreferencesGroup| {
        let row = adw::EntryRow::builder()
            .title(format!("Вариант {}", rows.len() + 1))
            .build();
        group.add(&row);
        rows.push(row.clone());
        row
    };
    append(&mut choices.borrow_mut(), &answers);
    append(&mut choices.borrow_mut(), &answers);
    content.append(&answers);
    let add = gtk::Button::with_label("Добавить вариант");
    add.set_halign(gtk::Align::Start);
    content.append(&add);
    add.connect_clicked(glib::clone!(
        #[weak]
        answers,
        #[strong]
        choices,
        move |add| {
            let mut rows = choices.borrow_mut();
            append(&mut rows, &answers).grab_focus();
            add.set_sensitive(rows.len() < MAX_POLL_CHOICES);
        }
    ));
    let settings = adw::PreferencesGroup::new();
    let multiple = adw::SwitchRow::builder().title("Несколько ответов").build();
    let public = adw::SwitchRow::builder()
        .title("Публичное голосование")
        .subtitle("Другие участники смогут видеть, кто за что проголосовал")
        .build();
    settings.add(&multiple);
    settings.add(&public);
    let timing = TimingControls::append(&settings);
    content.append(&settings);
    let status = label("");
    status.add_css_class("dim-label");
    content.append(&status);
    let submit = gtk::Button::with_label("Создать опрос");
    submit.add_css_class("suggested-action");
    submit.set_halign(gtk::Align::End);
    content.append(&submit);
    let attempt = RefCell::new(None::<(PollDraft, i64)>);
    submit.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        content,
        #[weak]
        status,
        move |_| {
            let draft = PollDraft {
                deadline: match timing.deadline() {
                    Ok(deadline) => deadline,
                    Err(error) => {
                        status.set_text(&error.to_string());
                        status.set_visible(true);
                        return;
                    }
                },
                hide_results_until_close: timing.mode.selected() != 0 && timing.hide.is_active(),
                question: question.text().into(),
                choices: choices
                    .borrow()
                    .iter()
                    .map(|choice| choice.text().into())
                    .collect(),
                multiple: multiple.is_active(),
                public: public.is_active(),
            };
            let draft = match draft.validate() {
                Ok(draft) => draft,
                Err(error) => {
                    status.set_text(&error.to_string());
                    return;
                }
            };
            content.set_sensitive(false);
            status.set_text("Отправка…");
            let random_id = {
                let mut attempt = attempt.borrow_mut();
                if let Some((previous, id)) = attempt.as_ref()
                    && previous == &draft
                {
                    *id
                } else {
                    let id =
                        (i64::from(glib::random_int()) << 32) | i64::from(glib::random_int()) | 1;
                    *attempt = Some((draft.clone(), id));
                    id
                }
            };
            let result = send(draft, random_id);
            glib::MainContext::default().spawn_local(glib::clone!(
                #[weak]
                dialog,
                #[weak]
                content,
                #[weak]
                status,
                async move {
                    match result.await {
                        Ok(()) => {
                            dialog.close();
                        }
                        Err(error) => {
                            status.set_text(&format!("Не удалось отправить опрос. {error}"));
                            content.set_sensitive(true);
                        }
                    }
                }
            ));
        }
    ));
    let scroll = gtk::ScrolledWindow::builder()
        .child(&content)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(window));
    dialog
}

const POLL_PERIODS: [(&str, i32); 6] = [
    ("Через минуту", 60),
    ("Через час", 3600),
    ("Через 3 часа", 3 * 3600),
    ("Через 8 часов", 8 * 3600),
    ("Через сутки", 24 * 3600),
    ("Через 3 дня", 72 * 3600),
];
const CUSTOM_TIME: u32 = POLL_PERIODS.len() as u32 + 1;

struct TimingControls {
    mode: adw::ComboRow,
    date: adw::EntryRow,
    hide: adw::SwitchRow,
}

impl TimingControls {
    fn append(settings: &adw::PreferencesGroup) -> Self {
        let choices: Vec<_> = std::iter::once("Без срока")
            .chain(POLL_PERIODS.map(|(label, _)| label))
            .chain(["Дата и время"])
            .collect();
        let mode = adw::ComboRow::builder()
            .title("Завершение опроса")
            .model(&gtk::StringList::new(&choices))
            .selected(0)
            .build();
        settings.add(&mode);
        let date = adw::EntryRow::builder()
            .title("Дата и время")
            .visible(false)
            .build();
        date.set_tooltip_text(Some("Местное время, ГГГГ-ММ-ДД ЧЧ:ММ"));
        if let Ok(tomorrow) = glib::DateTime::from_unix_local(unix_now() + 86_400)
            && let Ok(formatted) = tomorrow.format("%Y-%m-%d %H:%M")
        {
            date.set_text(&formatted);
        }
        settings.add(&date);
        let hide = adw::SwitchRow::builder()
            .title("Скрыть итоги до завершения")
            .visible(false)
            .build();
        settings.add(&hide);
        mode.connect_selected_notify(glib::clone!(
            #[weak]
            date,
            #[weak]
            hide,
            move |mode| {
                date.set_visible(mode.selected() == CUSTOM_TIME);
                hide.set_visible(mode.selected() != 0);
            }
        ));
        Self { mode, date, hide }
    }

    fn deadline(&self) -> anyhow::Result<Option<PollDeadline>> {
        match self.mode.selected() {
            0 => Ok(None),
            CUSTOM_TIME => Ok(Some(PollDeadline::At(local_deadline(&self.date.text())?))),
            index => Ok(POLL_PERIODS
                .get(index as usize - 1)
                .map(|(_, seconds)| PollDeadline::After(*seconds))),
        }
    }
}

fn local_deadline(text: &str) -> anyhow::Result<i64> {
    let text = text.trim();
    let iso = format!("{}:00", text.replace(' ', "T"));
    let date = glib::DateTime::from_iso8601(&iso, Some(&glib::TimeZone::local()))
        .map_err(|_| anyhow::anyhow!("Введите местное время в формате ГГГГ-ММ-ДД ЧЧ:ММ"))?;
    anyhow::ensure!(
        date.format("%Y-%m-%d %H:%M")?.as_str() == text,
        "Введите местное время в формате ГГГГ-ММ-ДД ЧЧ:ММ"
    );
    Ok(date.to_unix())
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn custom_deadline_accepts_unambiguous_local_input_and_rejects_invalid_dates() {
        let time = glib::DateTime::from_unix_local(1_800_000_000).unwrap();
        let minute = time.format("%Y-%m-%d %H:%M").unwrap();
        let parsed = local_deadline(&minute).unwrap();
        assert!(parsed <= time.to_unix() && time.to_unix() - parsed < 60);
        assert!(local_deadline("2026-02-30 17:00").is_err());
        assert!(local_deadline("2026-02-28 17:00Z").is_err());
    }
}
