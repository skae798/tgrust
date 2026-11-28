use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result, bail, ensure};
use gtk::glib;

use super::{Ui, add_menu_button, label};
use crate::model::{
    Conversation, Message, Poll, PollAnswerAddition, PollAnswerChange, PollChoice, PollUpdate,
    normalized_poll_answer, restriction_now,
};

enum AnswerEditor {
    Add,
    Delete(Vec<(String, Vec<u8>)>),
}

pub(super) struct AnswerRefresh {
    poll_id: i64,
    refresh_again: bool,
}

impl Ui {
    pub(super) fn add_poll_answer_actions(
        self: &Rc<Self>,
        menu: &gtk::Box,
        target: Conversation,
        message: &Message,
    ) {
        let Some(poll) = self.editable_answers(target, message) else {
            return;
        };
        let (limit, period) = self.poll_answer_limits.get();
        let id = message.id;
        let expected = poll.id;
        if poll.choices.len() < limit {
            add_menu_button(
                menu,
                "Добавить вариант",
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || ui.open_answer_editor(target, id, expected, false)
                ),
            );
        }
        let own = self.poll_viewer_id();
        let now = restriction_now();
        if poll
            .choices
            .iter()
            .any(|choice| poll.can_delete_answer(choice, own, now, period))
        {
            add_menu_button(
                menu,
                "Удалить вариант",
                glib::clone!(
                    #[weak(rename_to = ui)]
                    self,
                    move || ui.open_answer_editor(target, id, expected, true)
                ),
            );
        }
    }

    fn editable_answers<'a>(&self, target: Conversation, message: &'a Message) -> Option<&'a Poll> {
        if self.conversation() != Some(target)
            || !self.input.is_sensitive()
            || message.forwarded
            || message.ephemeral
            || self
                .poll_actions
                .borrow()
                .contains(&(target.chat, message.id))
        {
            return None;
        }
        message.poll.as_ref().filter(|poll| poll.answers_open())
    }

    fn current_answer_poll(&self, target: Conversation, id: i32, expected: i64) -> Option<Poll> {
        self.messages
            .borrow()
            .get(&id)
            .and_then(|message| self.editable_answers(target, message))
            .filter(|poll| poll.id == expected)
            .cloned()
    }

    fn open_answer_editor(
        self: &Rc<Self>,
        target: Conversation,
        id: i32,
        expected: i64,
        delete: bool,
    ) {
        if self.chat_dialog_open() {
            return;
        }
        let (Some(window), Some(poll)) = (
            self.window.upgrade(),
            self.current_answer_poll(target, id, expected),
        ) else {
            return;
        };
        let poll = poll.for_viewer(self.poll_viewer_id());
        let (limit, period) = self.poll_answer_limits.get();
        let editor = if delete {
            let own = self.poll_viewer_id();
            let now = restriction_now();
            let choices: Vec<_> = poll
                .choices
                .iter()
                .filter(|choice| poll.can_delete_answer(choice, own, now, period))
                .map(|choice| (choice.label.clone(), choice.option.clone()))
                .collect();
            if choices.is_empty() {
                return;
            }
            AnswerEditor::Delete(choices)
        } else if poll.choices.len() < limit {
            AnswerEditor::Add
        } else {
            return;
        };
        let weak = Rc::downgrade(self);
        let dialog = answer_dialog(editor, move |change| {
            let weak = weak.clone();
            async move {
                let ui = weak.upgrade().context("Аккаунт закрыт")?;
                ui.submit_answer_change(target, id, expected, change).await
            }
        });
        dialog.connect_closed(glib::clone!(
            #[weak(rename_to = ui)]
            self,
            move |_| {
                ui.poll_dialog.set(None);
                ui.focus_composer();
            }
        ));
        self.poll_dialog.set(Some(&dialog));
        dialog.present(Some(&window));
    }

    async fn submit_answer_change(
        self: Rc<Self>,
        target: Conversation,
        id: i32,
        expected: i64,
        change: PollAnswerChange,
    ) -> Result<()> {
        let poll = self
            .current_answer_poll(target, id, expected)
            .context("Опрос больше недоступен")?;
        if self.is_demo.get() {
            return self.demo_answer_change(poll, change);
        }
        let client = self
            .client
            .borrow()
            .clone()
            .context("Нет подключения к Telegram")?;
        let address = (target.chat, id);
        self.poll_actions.borrow_mut().insert(address);
        let weak = Rc::downgrade(&self);
        let task = self.runtime.spawn(async move {
            tokio::time::timeout(
                Duration::from_secs(30),
                client.change_poll_answer(target.chat, id, expected, change),
            )
            .await
            .context("Telegram не подтвердил изменение вовремя")?
        });
        drop(self);
        let result = task
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result);
        if let Some(ui) = weak.upgrade() {
            ui.poll_actions.borrow_mut().remove(&address);
            match &result {
                Ok(Some(update)) => ui.apply_poll_update(update.clone()),
                Ok(None) | Err(_) => ui.refresh_open_poll(target.chat, id),
            }
        }
        result.map(drop)
    }

    fn demo_answer_change(self: &Rc<Self>, mut poll: Poll, change: PollAnswerChange) -> Result<()> {
        let (limit, period) = self.poll_answer_limits.get();
        let own = self.poll_viewer_id();
        let previous_vote = poll.has_vote();
        match change {
            PollAnswerChange::Add(text) => {
                let text = normalized_poll_answer(&text)?;
                ensure!(
                    poll.choices.len() < limit,
                    "Достигнут лимит вариантов ({limit})"
                );
                ensure!(
                    !poll.choices.iter().any(|choice| choice.label == text),
                    "Такой вариант уже есть"
                );
                poll.choices.push(PollChoice {
                    option: self.next_demo_message().to_be_bytes().to_vec(),
                    label: text,
                    votes: Some(0),
                    chosen: false,
                    correct: false,
                    addition: own.map(|author| PollAnswerAddition {
                        author,
                        date: restriction_now(),
                    }),
                });
            }
            PollAnswerChange::Delete(option) => {
                let Some(index) = poll.choices.iter().position(|choice| {
                    choice.option == option
                        && poll.can_delete_answer(choice, own, restriction_now(), period)
                }) else {
                    bail!("Удаление этого варианта недоступно")
                };
                poll.choices.remove(index);
            }
        }
        let total = poll
            .total
            .map(|total| (total - i32::from(previous_vote && !poll.has_vote())).max(0));
        self.apply_poll_update(PollUpdate {
            id: poll.id,
            target: None,
            definition: Some(poll),
            results: None,
            total,
            solution: None,
            partial: false,
        });
        Ok(())
    }

    pub(super) fn refresh_visible_open_polls(self: &Rc<Self>) {
        let Some(chat) = self.selection.get() else {
            return;
        };
        let ids: Vec<_> = self
            .messages
            .borrow()
            .values()
            .filter(|message| message.poll.as_ref().is_some_and(|poll| poll.open_answers))
            .map(|message| message.id)
            .collect();
        for id in ids {
            self.refresh_open_poll(chat, id);
        }
    }

    pub(super) fn refresh_open_poll(self: &Rc<Self>, chat: i64, id: i32) {
        let Some(client) = self.client.borrow().clone() else {
            return;
        };
        let visible = (self.selection.get() == Some(chat))
            .then(|| {
                self.messages
                    .borrow()
                    .get(&id)
                    .and_then(|message| message.poll.as_ref())
                    .filter(|poll| poll.open_answers)
                    .map(|poll| poll.id)
            })
            .flatten();
        let Some(expected) = visible.or_else(|| self.history_cache.borrow().open_poll_id(chat, id))
        else {
            return;
        };
        let target = (chat, id);
        {
            let mut pending = self.poll_answer_refreshes.borrow_mut();
            if let Some(refresh) = pending.get_mut(&target) {
                refresh.refresh_again = true;
                return;
            }
            pending.insert(
                target,
                AnswerRefresh {
                    poll_id: expected,
                    refresh_again: false,
                },
            );
        }
        self.job(
            async move {
                tokio::time::timeout(
                    Duration::from_secs(8),
                    client.refresh_poll_answers(chat, id, expected),
                )
                .await?
            },
            move |ui, result| {
                let pending = ui.poll_answer_refreshes.borrow_mut().remove(&target);
                if pending.is_some_and(|refresh| refresh.refresh_again) {
                    ui.refresh_open_poll(chat, id);
                } else if let Ok(Some(update)) = result {
                    ui.apply_poll_update(update);
                }
            },
        );
    }

    pub(super) fn invalidate_pending_answer_refreshes(&self, update: &PollUpdate) {
        for (&target, refresh) in self.poll_answer_refreshes.borrow_mut().iter_mut() {
            if refresh.poll_id == update.id && update.target.is_none_or(|scope| scope == target) {
                refresh.refresh_again = true;
            }
        }
    }
}

fn answer_dialog<F, Fut>(editor: AnswerEditor, submit: F) -> adw::Dialog
where
    F: Fn(PollAnswerChange) -> Fut + 'static,
    Fut: Future<Output = Result<()>> + 'static,
{
    let (title, action) = match editor {
        AnswerEditor::Add => ("Добавить вариант", "Добавить"),
        AnswerEditor::Delete(_) => ("Удалить вариант", "Удалить"),
    };
    let (dialog, toolbar, body) = crate::chat_management::dialog_with_toolbar(title);
    dialog.set_content_height(240);
    let confirm = gtk::Button::with_label(action);
    confirm.set_halign(gtk::Align::End);
    confirm.set_margin_end(12);
    confirm.set_margin_bottom(12);
    confirm.set_sensitive(false);
    type ReadAnswer = Box<dyn Fn() -> Option<PollAnswerChange>>;
    let (editor, read): (gtk::Widget, ReadAnswer) = match editor {
        AnswerEditor::Add => {
            confirm.add_css_class("suggested-action");
            let entries = adw::PreferencesGroup::new();
            let entry = adw::EntryRow::builder().title("Новый вариант").build();
            entries.add(&entry);
            body.append(&entries);
            entry.connect_changed(glib::clone!(
                #[weak]
                confirm,
                move |entry| {
                    confirm.set_sensitive(!entry.text().trim().is_empty());
                }
            ));
            (
                entry.clone().upcast(),
                Box::new(move || Some(PollAnswerChange::Add(entry.text().into()))),
            )
        }
        AnswerEditor::Delete(choices) => {
            confirm.add_css_class("destructive-action");
            let explanation = label(
                "Удаление варианта может изменить итоги опроса.",
                "dim-label",
            );
            explanation.set_wrap(true);
            body.append(&explanation);
            let labels: Vec<_> = std::iter::once("Выберите вариант")
                .chain(choices.iter().map(|(name, _)| name.as_str()))
                .collect();
            let entries = adw::PreferencesGroup::new();
            let selector = adw::ComboRow::builder()
                .title("Вариант ответа")
                .model(&gtk::StringList::new(&labels))
                .selected(0)
                .build();
            selector.connect_selected_notify(glib::clone!(
                #[weak]
                confirm,
                move |row| {
                    confirm.set_sensitive(
                        row.selected() > 0 && row.selected() != gtk::INVALID_LIST_POSITION,
                    );
                }
            ));
            entries.add(&selector);
            body.append(&entries);
            (
                selector.clone().upcast(),
                Box::new(move || {
                    selector
                        .selected()
                        .checked_sub(1)
                        .and_then(|index| choices.get(index as usize))
                        .map(|(_, id)| PollAnswerChange::Delete(id.clone()))
                }),
            )
        }
    };
    dialog.connect_map(glib::clone!(
        #[weak]
        editor,
        move |_| {
            editor.grab_focus();
        }
    ));
    toolbar.add_bottom_bar(&confirm);

    let status = label("", "dim-label");
    status.set_wrap(true);
    body.append(&status);
    confirm.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        body,
        #[weak]
        status,
        #[weak]
        editor,
        move |confirm| {
            let Some(change) = read() else { return };
            confirm.set_sensitive(false);
            body.set_sensitive(false);
            status.set_text("Сохранение…");
            let request = submit(change);
            glib::spawn_future_local(glib::clone!(
                #[weak]
                dialog,
                #[weak]
                body,
                #[weak]
                status,
                #[weak]
                confirm,
                #[weak]
                editor,
                async move {
                    match request.await {
                        Ok(()) => {
                            dialog.close();
                        }
                        Err(error) => {
                            status.set_text(&format!("Не удалось сохранить изменение. {error}"));
                            body.set_sensitive(true);
                            confirm.set_sensitive(true);
                            editor.grab_focus();
                        }
                    }
                }
            ));
        }
    ));
    dialog
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PollDraft;
    use crate::ui::{Account, Accounts, build_window, editor_text};
    use std::time::Instant;

    #[track_caller]
    fn settle(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !ready() && Instant::now() < deadline {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ready(), "Answer editor did not settle");
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

    fn confirm(dialog: &adw::Dialog, action: &str) -> gtk::Button {
        let button = find(dialog, &|button: &gtk::Button| {
            button.label().as_deref() == Some(action)
        })
        .unwrap();
        settle(|| button.is_mapped());
        assert!(button.grab_focus());
        assert!(gtk::prelude::WidgetExt::activate(&button));
        button
    }

    #[test]
    #[ignore = "requires a graphical display"]
    fn answer_editor_adds_deletes_and_rejects_stale_or_expired_operations() {
        gtk::test_synced(|| {
            let app = crate::application(true);
            app.set_application_id(Some("io.github.skae798.tgrust.PollAnswersTest"));
            app.set_flags(gtk::gio::ApplicationFlags::NON_UNIQUE);
            app.register(None::<&gtk::gio::Cancellable>).unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let (window, ui) = build_window(
                &app,
                Accounts::new(&app, true),
                Account::Demo(0),
                None,
                runtime,
            );
            window.set_default_size(360, 720);
            settle(|| (340..=360).contains(&window.width()));
            ui.show_chat(1, None);
            let target = ui.conversation().unwrap();
            ui.input.buffer().set_text("Черновик первого чата");
            ui.add_demo_messages(
                target,
                vec![crate::demo::poll_message(
                    903,
                    PollDraft {
                        question: "Когда встретимся?".into(),
                        choices: vec!["Утром".into(), "Днём".into()],
                        public: true,
                        open_answers: true,
                        ..PollDraft::default()
                    },
                )],
            );
            let expected = ui.messages.borrow()[&903].poll.as_ref().unwrap().id;
            let count = || {
                ui.messages.borrow()[&903]
                    .poll
                    .as_ref()
                    .unwrap()
                    .choices
                    .len()
            };
            let open = |delete| {
                ui.open_answer_editor(target, 903, expected, delete);
                let dialog = window.visible_dialog().unwrap();
                settle(|| dialog.is_mapped());
                dialog
            };
            let entry = |dialog: &adw::Dialog| {
                find(dialog, &|row: &adw::EntryRow| {
                    row.title() == "Новый вариант"
                })
                .unwrap()
            };
            let dialog = open(false);
            let add = find(&dialog, &|button: &gtk::Button| {
                button.label().as_deref() == Some("Добавить")
            })
            .unwrap();
            assert!(!add.is_sensitive());
            entry(&dialog).set_text("  Вечером  ");
            confirm(&dialog, "Добавить");
            settle(|| count() == 3 && window.visible_dialog().is_none());
            assert_eq!(
                ui.messages.borrow()[&903].poll.as_ref().unwrap().choices[2].label,
                "Вечером"
            );
            assert_eq!(
                editor_text(&ui.input.buffer()).text,
                "Черновик первого чата"
            );
            settle(|| gtk::prelude::RootExt::focus(&window) == Some(ui.input.clone().upcast()));
            let dialog = open(false);
            entry(&dialog).set_text("Вечером");
            let add = confirm(&dialog, "Добавить");
            settle(|| {
                add.is_sensitive()
                    && find(&dialog, &|label: &gtk::Label| {
                        label.text().contains("Такой вариант уже есть")
                    })
                    .is_some()
            });
            assert_eq!(count(), 3);
            entry(&dialog).set_text(&"я".repeat(101));
            confirm(&dialog, "Добавить");
            settle(|| {
                add.is_sensitive()
                    && find(&dialog, &|label: &gtk::Label| {
                        label.text().contains("до 100 символов")
                    })
                    .is_some()
            });
            assert_eq!(count(), 3);
            dialog.close();
            settle(|| window.visible_dialog().is_none());
            let dialog = open(true);
            let delete = find(&dialog, &|button: &gtk::Button| {
                button.label().as_deref() == Some("Удалить")
            })
            .unwrap();
            assert!(!delete.is_sensitive());
            dialog.close();
            settle(|| window.visible_dialog().is_none());
            assert_eq!(count(), 3);
            let dialog = open(false);
            entry(&dialog).set_text("Ночью");
            ui.show_chat(2, None);
            ui.input.buffer().set_text("Черновик второго чата");
            let add = confirm(&dialog, "Добавить");
            settle(|| {
                add.is_sensitive()
                    && find(&dialog, &|label: &gtk::Label| {
                        label.text().contains("Опрос больше недоступен")
                    })
                    .is_some()
            });
            assert_eq!(
                editor_text(&ui.input.buffer()).text,
                "Черновик второго чата"
            );
            dialog.close();
            settle(|| window.visible_dialog().is_none());
            ui.show_chat(1, None);
            assert_eq!(count(), 3);
            let option = ui.messages.borrow()[&903].poll.as_ref().unwrap().choices[2]
                .option
                .clone();
            ui.vote(target.chat, 903, expected, vec![option]);
            assert_eq!(
                ui.messages.borrow()[&903].poll.as_ref().unwrap().total,
                Some(1)
            );
            let dialog = open(true);
            let selector = find(&dialog, &|row: &adw::ComboRow| {
                row.title() == "Вариант ответа"
            })
            .unwrap();
            selector.set_selected(1);
            confirm(&dialog, "Удалить");
            settle(|| count() == 2 && window.visible_dialog().is_none());
            assert_eq!(
                ui.messages.borrow()[&903].poll.as_ref().unwrap().total,
                Some(0)
            );
            assert!(!ui.messages.borrow()[&903].poll.as_ref().unwrap().has_vote());
            assert_eq!(
                editor_text(&ui.input.buffer()).text,
                "Черновик первого чата"
            );
            ui.open_answer_editor(target, 903, expected, true);
            assert!(window.visible_dialog().is_none());
            ui.messages
                .borrow_mut()
                .get_mut(&903)
                .unwrap()
                .poll
                .as_mut()
                .unwrap()
                .closes_at = Some(crate::model::unix_now() - 1);
            ui.open_answer_editor(target, 903, expected, false);
            assert!(window.visible_dialog().is_none());
            assert!(!ui.messages.borrow()[&903].poll.as_ref().unwrap().closed);
            assert!(ui.toasts.measure(gtk::Orientation::Horizontal, -1).0 <= window.width());
            window.close();
        });
    }
}
