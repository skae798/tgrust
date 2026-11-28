use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::model::{MAX_POLL_CHOICES, Poll, PollDraft, PollQuizDraft};

use crate::model::{PollDeadline, unix_now};

type SendPoll = Pin<Box<dyn Future<Output = anyhow::Result<()>>>>;
type ChoiceRows = Rc<RefCell<Vec<(adw::EntryRow, gtk::CheckButton)>>>;

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
    question.set_selectable(true);
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
    let correct_known = poll.quiz && poll.choices.iter().any(|choice| choice.correct);
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
            let (name, description, color) = if choice.correct {
                ("object-select-symbolic", "Правильный ответ", "success")
            } else if correct_known {
                ("window-close-symbolic", "Неправильный ответ", "error")
            } else {
                ("object-select-symbolic", "Ваш голос", "accent")
            };
            let icon = gtk::Image::from_icon_name(name);
            icon.add_css_class(color);
            icon.update_property(&[gtk::accessible::Property::Label(description)]);
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
    let closed = if poll.quiz {
        "Викторина завершена"
    } else {
        "Опрос закрыт"
    };
    let status = match (poll.closed, poll.total) {
        (true, Some(total)) => format!("{closed} · Всего проголосовало: {total}"),
        (true, None) => closed.to_owned(),
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
    allow_open_answers: bool,
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
    let quiz = adw::SwitchRow::builder().title("Викторина").build();
    question_group.add(&quiz);
    content.append(&question_group);
    let answers = adw::PreferencesGroup::builder()
        .title("Варианты ответа")
        .build();
    let correct_hint = label("Отметьте правильные варианты ответа");
    correct_hint.add_css_class("dim-label");
    quiz.bind_property("active", &correct_hint, "visible")
        .sync_create()
        .build();
    content.append(&correct_hint);
    let multiple = adw::SwitchRow::builder().title("Несколько ответов").build();
    let choices = ChoiceRows::default();
    let append = |rows: &ChoiceRows,
                  quiz: &adw::SwitchRow,
                  group: &adw::PreferencesGroup,
                  multiple: &adw::SwitchRow| {
        let number = rows.borrow().len() + 1;
        let row = adw::EntryRow::builder()
            .title(format!("Вариант {number}"))
            .build();
        let correct = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Правильный ответ")
            .sensitive(false)
            .build();
        correct.update_property(&[gtk::accessible::Property::Label(&format!(
            "Правильный ответ, вариант {number}"
        ))]);
        quiz.bind_property("active", &correct, "visible")
            .sync_create()
            .build();
        row.connect_changed(glib::clone!(
            #[weak]
            correct,
            move |row| {
                let filled = !row.text().trim().is_empty();
                if !filled {
                    correct.set_active(false);
                }
                correct.set_sensitive(filled);
            }
        ));
        let other_rows = Rc::downgrade(rows);
        correct.connect_toggled(glib::clone!(
            #[weak]
            multiple,
            move |checked| {
                if checked.is_active()
                    && !multiple.is_active()
                    && let Some(rows) = other_rows.upgrade()
                {
                    for (_, other) in rows.borrow().iter().filter(|(_, other)| other != checked) {
                        other.set_active(false);
                    }
                }
            }
        ));
        row.add_suffix(&correct);
        group.add(&row);
        rows.borrow_mut().push((row.clone(), correct));
        row
    };
    append(&choices, &quiz, &answers, &multiple);
    append(&choices, &quiz, &answers, &multiple);
    content.append(&answers);
    let add = gtk::Button::with_label("Добавить вариант");
    add.set_halign(gtk::Align::Start);
    content.append(&add);
    let settings = adw::PreferencesGroup::new();
    add.connect_clicked(glib::clone!(
        #[weak]
        answers,
        #[weak]
        quiz,
        #[weak]
        multiple,
        #[strong]
        choices,
        move |add| {
            append(&choices, &quiz, &answers, &multiple).grab_focus();
            add.set_sensitive(choices.borrow().len() < MAX_POLL_CHOICES);
        }
    ));
    multiple.connect_active_notify(glib::clone!(
        #[strong]
        choices,
        move |multiple| {
            if !multiple.is_active() {
                for (_, extra) in choices
                    .borrow()
                    .iter()
                    .filter(|(_, correct)| correct.is_active())
                    .skip(1)
                {
                    extra.set_active(false);
                }
            }
        }
    ));
    let public = adw::SwitchRow::builder()
        .title("Публичное голосование")
        .subtitle("Другие участники смогут видеть, кто за что проголосовал")
        .build();
    settings.add(&multiple);
    settings.add(&public);
    let timing = TimingControls::append(&settings);
    content.append(&settings);
    let open_answers = adw::SwitchRow::builder()
        .title("Участники могут добавлять варианты")
        .visible(false)
        .build();
    if allow_open_answers {
        public.connect_active_notify(glib::clone!(
            #[weak]
            quiz,
            #[weak]
            open_answers,
            move |public| open_answers.set_visible(public.is_active() && !quiz.is_active())
        ));
        quiz.connect_active_notify(glib::clone!(
            #[weak]
            public,
            #[weak]
            open_answers,
            move |quiz| open_answers.set_visible(public.is_active() && !quiz.is_active())
        ));
        settings.add(&open_answers);
    }
    let explanation = gtk::Box::new(gtk::Orientation::Vertical, 6);
    explanation.append(&label("Пояснение к ответу (необязательно)"));
    let solution = gtk::TextView::builder()
        .height_request(64)
        .accepts_tab(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .css_classes(["card"])
        .build();
    solution.update_property(&[gtk::accessible::Property::Label("Пояснение к ответу")]);
    explanation.append(&solution);
    quiz.bind_property("active", &explanation, "visible")
        .sync_create()
        .build();
    content.append(&explanation);
    let actions = gtk::Box::new(gtk::Orientation::Vertical, 6);
    actions.set_margin_top(8);
    actions.set_margin_bottom(12);
    actions.set_margin_start(12);
    actions.set_margin_end(12);
    let status = label("");
    status.add_css_class("dim-label");
    status.set_visible(false);
    actions.append(&status);
    let submit = gtk::Button::with_label("Создать опрос");
    submit.add_css_class("suggested-action");
    submit.set_halign(gtk::Align::End);
    actions.append(&submit);
    quiz.connect_active_notify(glib::clone!(
        #[weak]
        submit,
        #[weak]
        dialog,
        #[weak]
        status,
        move |quiz| {
            let (title, action) = if quiz.is_active() {
                ("Новая викторина", "Создать викторину")
            } else {
                ("Новый опрос", "Создать опрос")
            };
            dialog.set_title(title);
            submit.set_label(action);
            status.set_visible(false);
        }
    ));
    let attempt = RefCell::new(None::<(PollDraft, i64)>);
    submit.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        content,
        #[weak]
        status,
        move |submit| {
            let rows = choices.borrow();
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
                open_answers: allow_open_answers
                    && public.is_active()
                    && !quiz.is_active()
                    && open_answers.is_active(),
                choices: rows.iter().map(|(row, _)| row.text().into()).collect(),
                multiple: multiple.is_active(),
                public: public.is_active(),
                quiz: quiz.is_active().then(|| {
                    let buffer = solution.buffer();
                    PollQuizDraft {
                        correct: rows
                            .iter()
                            .enumerate()
                            .filter_map(|(index, (_, correct))| {
                                correct.is_active().then_some(index)
                            })
                            .collect(),
                        solution: buffer
                            .text(&buffer.start_iter(), &buffer.end_iter(), false)
                            .into(),
                    }
                }),
            };
            drop(rows);
            status.set_visible(true);
            let draft = match draft.validate() {
                Ok(draft) => draft,
                Err(error) => {
                    status.set_text(&error.to_string());
                    return;
                }
            };
            content.set_sensitive(false);
            submit.set_sensitive(false);
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
                #[weak]
                submit,
                async move {
                    match result.await {
                        Ok(()) => {
                            dialog.close();
                        }
                        Err(error) => {
                            status.set_text(&format!("Не удалось отправить. {error}"));
                            content.set_sensitive(true);
                            submit.set_sensitive(true);
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
    toolbar.add_bottom_bar(&actions);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[track_caller]
    fn settle(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !ready() && Instant::now() < deadline {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ready(), "Quiz editor did not settle");
    }

    fn find<W: IsA<gtk::Widget> + Clone>(
        root: &impl IsA<gtk::Widget>,
        predicate: &impl Fn(&W) -> bool,
    ) -> Option<W> {
        if let Ok(widget) = root.as_ref().clone().downcast::<W>()
            && predicate(&widget)
        {
            return Some(widget);
        }
        let mut child = root.as_ref().first_child();
        while let Some(widget) = child {
            if let Some(found) = find(&widget, predicate) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }

    #[test]
    #[ignore = "requires a graphical display"]
    fn quiz_editor_normalizes_choices_and_retries_without_duplicate_operations() {
        gtk::test_synced(|| {
            let app = adw::Application::builder()
                .application_id("io.github.skae798.tgrust.QuizEditorTest")
                .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
                .build();
            app.register(None::<&gtk::gio::Cancellable>).unwrap();
            let window = adw::ApplicationWindow::builder()
                .application(&app)
                .default_width(360)
                .default_height(720)
                .build();
            window.set_content(Some(&gtk::Box::new(gtk::Orientation::Vertical, 0)));
            window.present();
            let attempts = Rc::new(RefCell::new(Vec::<(PollDraft, i64)>::new()));
            let sent = attempts.clone();
            let dialog = create(&window, true, move |draft, id| {
                sent.borrow_mut().push((draft, id));
                Box::pin(async { anyhow::bail!("Synthetic send interruption") })
            });
            settle(|| dialog.is_mapped());
            let entry =
                |title: &str| find(&dialog, &|row: &adw::EntryRow| row.title() == title).unwrap();
            let switch =
                |title: &str| find(&dialog, &|row: &adw::SwitchRow| row.title() == title).unwrap();
            let question = entry("Вопрос");
            question.set_text("Что видно ночью?");
            let first = entry("Вариант 1");
            first.set_text("Звёзды");
            switch("Викторина").set_active(true);
            let add = find(&dialog, &|button: &gtk::Button| {
                button.label().as_deref() == Some("Добавить вариант")
            })
            .unwrap();
            add.emit_clicked();
            let third = entry("Вариант 3");
            third.set_text("Луна");
            let correct = |row: &adw::EntryRow| find(row, &|_: &gtk::CheckButton| true).unwrap();
            let submit = find(&dialog, &|button: &gtk::Button| {
                button.label().as_deref() == Some("Создать викторину")
            })
            .unwrap();
            settle(|| submit.is_mapped());
            assert!(submit.grab_focus());
            assert!(gtk::prelude::WidgetExt::activate(&submit));
            settle(|| {
                find(&dialog, &|label: &gtk::Label| {
                    label.is_visible() && label.text().contains("Отметьте правильный ответ")
                })
                .is_some()
            });
            assert!(attempts.borrow().is_empty());
            correct(&third).set_active(true);
            correct(&first).set_active(true);
            assert!(!correct(&third).is_active());
            correct(&third).set_active(true);
            let solution = find(&dialog, &|_: &gtk::TextView| true).unwrap();
            solution
                .buffer()
                .set_text("Луна отражает свет.\nЗвёзды светят сами.");
            let send = |count| {
                assert!(submit.grab_focus());
                assert!(gtk::prelude::WidgetExt::activate(&submit));
                settle(|| attempts.borrow().len() == count && submit.is_sensitive());
            };
            send(1);
            send(2);
            {
                let attempts = attempts.borrow();
                assert_eq!(attempts[0].1, attempts[1].1);
                let draft = &attempts[0].0;
                assert_eq!(draft.choices, ["Звёзды", "Луна"]);
                assert_eq!(draft.quiz.as_ref().unwrap().correct, [1]);
                assert!(draft.quiz.as_ref().unwrap().solution.contains('\n'));
            }
            switch("Несколько ответов").set_active(true);
            correct(&first).set_active(true);
            switch("Публичное голосование").set_active(true);
            send(3);
            {
                let attempts = attempts.borrow();
                assert_ne!(attempts[1].1, attempts[2].1);
                assert_eq!(attempts[2].0.quiz.as_ref().unwrap().correct, [0, 1]);
                assert!(attempts[2].0.multiple && attempts[2].0.public);
            }
            switch("Несколько ответов").set_active(false);
            assert!(correct(&first).is_active());
            assert!(!correct(&third).is_active());
            first.set_text("");
            assert!(!correct(&first).is_active());
            assert!(!correct(&first).is_sensitive());
            first.set_text("Звёзды");
            switch("Викторина").set_active(false);
            assert!(!correct(&first).is_visible());
            send(4);
            assert!(attempts.borrow()[3].0.quiz.is_none());
            switch("Викторина").set_active(true);
            correct(&third).set_active(true);
            let timing = find(&dialog, &|row: &adw::ComboRow| {
                row.title() == "Завершение опроса"
            })
            .unwrap();
            timing.set_selected(1);
            let hidden = switch("Скрыть итоги до завершения");
            assert!(hidden.is_visible());
            hidden.set_active(true);
            send(5);
            assert_eq!(
                attempts.borrow()[4].0.deadline,
                Some(PollDeadline::After(60))
            );
            assert!(attempts.borrow()[4].0.hide_results_until_close);
            assert!(attempts.borrow()[4].0.quiz.is_some());
            timing.set_selected(CUSTOM_TIME);
            let date = entry("Дата и время");
            assert!(date.is_visible());
            date.set_text("2026-02-30 12:00");
            assert!(submit.grab_focus());
            assert!(gtk::prelude::WidgetExt::activate(&submit));
            settle(|| {
                find(&dialog, &|label: &gtk::Label| {
                    label.is_visible() && label.text().contains("Введите местное время")
                })
                .is_some()
            });
            assert_eq!(attempts.borrow().len(), 5);
            let later = glib::DateTime::from_unix_local(unix_now() + 3600)
                .unwrap()
                .format("%Y-%m-%d %H:%M")
                .unwrap();
            date.set_text(&later);
            send(6);
            assert_eq!(
                attempts.borrow()[5].0.deadline,
                Some(PollDeadline::At(local_deadline(&later).unwrap()))
            );
            timing.set_selected(0);
            assert!(!hidden.is_visible() && !date.is_visible());
            send(7);
            assert!(attempts.borrow()[6].0.deadline.is_none());
            assert!(!attempts.borrow()[6].0.hide_results_until_close);
            let open = switch("Участники могут добавлять варианты");
            assert!(!open.is_visible());
            switch("Викторина").set_active(false);
            assert!(open.is_visible());
            open.set_active(true);
            send(8);
            assert!(attempts.borrow()[7].0.open_answers);
            switch("Викторина").set_active(true);
            assert!(!open.is_visible());
            send(9);
            assert!(!attempts.borrow()[8].0.open_answers);
            switch("Викторина").set_active(false);
            switch("Публичное голосование").set_active(false);
            assert!(!open.is_visible());
            send(10);
            assert!(!attempts.borrow()[9].0.open_answers);
            assert!(dialog.width() <= window.width());
            dialog.close();
            window.close();
        });
    }
}
