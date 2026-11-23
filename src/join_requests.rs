use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::{runtime::Handle, sync::broadcast, task::AbortHandle};

use crate::model::{
    JoinDecision, JoinDecisionOutcome, JoinRequest, JoinRequestCursor, JoinRequestPage,
};
use crate::profile::AvatarLoader;

type Read = Pin<Box<dyn Future<Output = Result<JoinRequestPage>> + Send>>;
type Write = Pin<Box<dyn Future<Output = Result<JoinDecisionOutcome>> + Send>>;

pub struct JoinRequestsContext {
    pub runtime: Handle,
    pub load: Box<dyn Fn(Option<JoinRequestCursor>) -> Read>,
    pub decide: Box<dyn Fn(i64, JoinDecision) -> Write>,
    pub avatar: AvatarLoader,
    pub updates: Option<broadcast::Receiver<i64>>,
}

pub fn open(anchor: &gtk::Button, chat: i64, context: JoinRequestsContext) {
    let (dialog, _) = build(chat, context);
    dialog.present(Some(anchor));
}

fn build(chat: i64, mut context: JoinRequestsContext) -> (adw::Dialog, Rc<Requests>) {
    let (dialog, body) = crate::chat_management::dialog("Заявки на вступление");
    let refresh = gtk::Button::with_label("Обновить заявки");
    refresh.set_halign(gtk::Align::Start);
    body.append(&refresh);
    let feedback = status_label();
    let status = status_label();
    body.append(&feedback);
    body.append(&status);
    let list = adw::PreferencesGroup::new();
    body.append(&list);
    let more = gtk::Button::with_label("Показать ещё");
    more.set_halign(gtk::Align::Center);
    body.append(&more);

    let updates = context.updates.take();
    let state = Rc::new(Requests {
        context,
        refresh,
        feedback,
        status,
        list,
        more,
        rows: RefCell::new(Vec::new()),
        seen: RefCell::new(HashSet::new()),
        cursors: RefCell::new(HashSet::new()),
        next: Cell::new(None),
        read: RefCell::new(None),
        read_generation: Cell::new(0),
        writing: Cell::new(false),
        refresh_pending: Cell::new(false),
        closed: Cell::new(false),
    });
    state.refresh.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| {
            state.feedback.set_text("");
            state.refresh();
        }
    ));
    state.more.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.load()
    ));
    let listener = updates.map(|mut updates| {
        let weak = Rc::downgrade(&state);
        glib::spawn_future_local(async move {
            loop {
                let refresh = match updates.recv().await {
                    Ok(changed) => changed == chat,
                    Err(broadcast::error::RecvError::Lagged(_)) => true,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Some(state) = weak.upgrade() else { break };
                if refresh {
                    state.refresh();
                }
            }
        })
    });
    state.load();
    let closing = state.clone();
    let listener = RefCell::new(listener);
    dialog.connect_closed(move |_| {
        closing.closed.set(true);
        closing.abort_read();
        if let Some(listener) = listener.take() {
            listener.abort();
        }
    });
    (dialog, state)
}

struct Requests {
    context: JoinRequestsContext,
    refresh: gtk::Button,
    feedback: gtk::Label,
    status: gtk::Label,
    list: adw::PreferencesGroup,
    more: gtk::Button,
    rows: RefCell<Vec<adw::ActionRow>>,
    seen: RefCell<HashSet<i64>>,
    cursors: RefCell<HashSet<JoinRequestCursor>>,
    next: Cell<Option<JoinRequestCursor>>,
    read: RefCell<Option<AbortHandle>>,
    read_generation: Cell<u64>,
    writing: Cell<bool>,
    refresh_pending: Cell<bool>,
    closed: Cell<bool>,
}

impl Requests {
    fn abort_read(&self) {
        self.read_generation
            .set(self.read_generation.get().wrapping_add(1));
        if let Some(read) = self.read.take() {
            read.abort();
        }
    }

    fn busy(&self) -> bool {
        self.writing.get() || self.read.borrow().is_some()
    }

    fn refresh(self: &Rc<Self>) {
        if self.busy() {
            self.refresh_pending.set(true);
            return;
        }
        self.refresh_pending.set(false);
        self.next.set(None);
        self.load();
    }

    fn load(self: &Rc<Self>) {
        if self.closed.get() || self.busy() {
            return;
        }
        let after = self.next.get();
        let request = (self.context.load)(after);
        self.status.set_text("Загрузка заявок…");
        self.more.set_visible(false);
        self.list.set_sensitive(false);
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(15), request)
                .await
                .context("Telegram не прислал заявки вовремя")?
        });
        self.read.replace(Some(task.abort_handle()));
        let generation = self.read_generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(state) = weak
                .upgrade()
                .filter(|state| !state.closed.get() && state.read_generation.get() == generation)
            else {
                return;
            };
            state.read.take();
            if state.refresh_pending.get() {
                state.refresh();
                return;
            }
            match result {
                Ok(page) => {
                    state.list.set_sensitive(true);
                    state.show(page, after.is_none());
                }
                Err(error) => {
                    state
                        .status
                        .set_text(&format!("Не удалось получить заявки. {error}"));
                    state.more.set_label("Повторить");
                    state.more.set_visible(true);
                }
            }
        });
    }

    fn show(self: &Rc<Self>, page: JoinRequestPage, first: bool) {
        if first {
            for row in self.rows.take() {
                self.list.remove(&row);
            }
            self.seen.borrow_mut().clear();
            self.cursors.borrow_mut().clear();
        }
        for request in page.requests {
            if self.seen.borrow_mut().insert(request.cursor.user) {
                let row = self.row(request);
                self.list.add(&row);
                self.rows.borrow_mut().push(row);
            }
        }
        let loaded = self.seen.borrow().len();
        let next = page
            .next
            .filter(|cursor| loaded < page.total && self.cursors.borrow_mut().insert(*cursor));
        self.next.set(next);
        self.more.set_label("Показать ещё");
        self.more.set_visible(next.is_some());
        self.status.set_text(match (loaded, page.total, next) {
            (0, 0, _) => "Заявок нет.",
            (loaded, total, None) if loaded < total => "Часть заявок недоступна. Обновите список.",
            _ => "",
        });
    }

    fn row(self: &Rc<Self>, request: JoinRequest) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(&request.name)
            .subtitle(&request.about)
            .title_lines(1)
            .subtitle_lines(2)
            .use_markup(false)
            .build();
        let avatar = adw::Avatar::new(30, Some(&request.name), true);
        if let Some(photo) = request.avatar {
            (self.context.avatar)(&avatar, photo);
        }
        row.add_prefix(&avatar);
        if request.actionable {
            for (decision, icon, title) in [
                (
                    JoinDecision::Approve,
                    "emblem-ok-symbolic",
                    "Принять заявку",
                ),
                (
                    JoinDecision::Decline,
                    "window-close-symbolic",
                    "Отклонить заявку",
                ),
            ] {
                let button = crate::media_viewer::action_button(icon, title);
                button.set_valign(gtk::Align::Center);
                let name = request.name.clone();
                let user = request.cursor.user;
                button.connect_clicked(glib::clone!(
                    #[weak(rename_to = state)]
                    self,
                    move |button| state.confirm(button, user, &name, decision)
                ));
                row.add_suffix(&button);
            }
        }
        row
    }

    fn confirm(
        self: &Rc<Self>,
        button: &gtk::Button,
        user: i64,
        name: &str,
        decision: JoinDecision,
    ) {
        let approved = matches!(decision, JoinDecision::Approve);
        let (heading, label) = if approved {
            ("Принять заявку?", "Принять")
        } else {
            ("Отклонить заявку?", "Отклонить")
        };
        let prompt = adw::AlertDialog::new(Some(heading), Some(name));
        prompt.set_body_use_markup(false);
        prompt.add_responses(&[("cancel", "Отмена"), ("decide", label)]);
        if !approved {
            prompt.set_response_appearance("decide", adw::ResponseAppearance::Destructive);
        }
        prompt.set_default_response(Some("cancel"));
        prompt.set_close_response("cancel");
        if let Some(cancel) = prompt.default_widget() {
            prompt.set_focus(Some(&cancel));
        }
        prompt.connect_response(
            Some("decide"),
            glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |_, _| state.decide(user, decision)
            ),
        );
        prompt.present(Some(button));
    }

    fn decide(self: &Rc<Self>, user: i64, decision: JoinDecision) {
        if self.closed.get() || self.writing.replace(true) {
            return;
        }
        self.abort_read();
        self.list.set_sensitive(false);
        self.more.set_sensitive(false);
        self.feedback.set_text("Обработка заявки…");
        let operation = (self.context.decide)(user, decision);
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(25), operation)
                .await
                .context("Telegram не подтвердил действие вовремя")?
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(state) = weak.upgrade().filter(|state| !state.closed.get()) else {
                return;
            };
            state.writing.set(false);
            state.more.set_sensitive(true);
            state.refresh.grab_focus();
            let text = match result {
                Ok(JoinDecisionOutcome::AlreadyHandled) => {
                    "Заявка уже обработана или отозвана.".into()
                }
                Ok(JoinDecisionOutcome::Applied) => match decision {
                    JoinDecision::Approve => "Заявка принята.".into(),
                    JoinDecision::Decline => "Заявка отклонена.".into(),
                },
                Err(error) => format!("Telegram не подтвердил действие. {error}"),
            };
            state.feedback.set_text(&text);
            state.refresh();
        });
    }
}

fn status_label() -> gtk::Label {
    gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn poll_main_until(mut ready: impl FnMut() -> bool) -> bool {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !ready() {
            if Instant::now() >= deadline {
                return false;
            }
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    #[test]
    #[ignore = "requires a graphical display"]
    fn pending_event_refreshes_the_open_chat_after_its_current_read() {
        gtk::test_synced(|| {
            adw::init().unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let window = adw::Window::new();
            let button = gtk::Button::with_label("Заявки");
            window.set_content(Some(&button));
            window.present();
            let (updates, receiver) = broadcast::channel(4);
            let (release, wait) = tokio::sync::oneshot::channel::<()>();
            let wait = RefCell::new(Some(wait));
            let reads = Rc::new(Cell::new(0));
            let count = reads.clone();
            let (dialog, state) = build(
                1,
                JoinRequestsContext {
                    runtime: runtime.handle().clone(),
                    load: Box::new(move |_| {
                        count.set(count.get() + 1);
                        let wait = wait.take();
                        Box::pin(async move {
                            if let Some(wait) = wait {
                                wait.await?;
                            }
                            Ok(JoinRequestPage {
                                requests: Vec::new(),
                                next: None,
                                total: 0,
                            })
                        })
                    }),
                    decide: Box::new(|_, _| Box::pin(async { Ok(JoinDecisionOutcome::Applied) })),
                    avatar: Box::new(|_, _| {}),
                    updates: Some(receiver),
                },
            );
            dialog.present(Some(&button));
            assert_eq!(reads.get(), 1);
            updates.send(9).unwrap();
            assert!(poll_main_until(|| updates.is_empty()));
            assert!(!state.refresh_pending.get());
            updates.send(1).unwrap();
            assert!(poll_main_until(|| state.refresh_pending.get()));
            assert_eq!(reads.get(), 1);
            release.send(()).unwrap();
            assert!(poll_main_until(
                || reads.get() == 2 && state.status.text() == "Заявок нет."
            ));
            dialog.force_close();
            assert!(poll_main_until(|| updates.receiver_count() == 0));
            window.close();
        });
    }
}
