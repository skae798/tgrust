use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::model::{MAX_POLL_CHOICES, Poll, PollDraft};

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
