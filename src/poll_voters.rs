use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::runtime::Handle;
use tokio::task::AbortHandle;

use crate::model::{Poll, PollVoterPage};
use crate::profile::AvatarLoader;

type PageFuture = Pin<Box<dyn Future<Output = Result<PollVoterPage>> + Send>>;
type PageLoader = dyn Fn(Option<Vec<u8>>, Option<String>) -> PageFuture;

pub struct VoterContext {
    pub runtime: Handle,
    pub load: Box<PageLoader>,
    pub avatar: AvatarLoader,
}

pub fn dialog(poll: &Poll, context: VoterContext) -> adw::Dialog {
    let (dialog, toolbar, body) = crate::chat_management::dialog_with_toolbar("Кто проголосовал");
    let controls = gtk::Box::new(gtk::Orientation::Vertical, 8);
    controls.set_margin_start(16);
    controls.set_margin_end(16);
    controls.set_margin_bottom(8);
    let question = gtk::Label::builder()
        .label(&poll.question)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .lines(3)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .tooltip_text(&poll.question)
        .css_classes(["title-4"])
        .build();
    controls.append(&question);

    let filters = gtk::DropDown::from_strings(
        &std::iter::once("Все варианты")
            .chain(poll.choices.iter().map(|choice| choice.label.as_str()))
            .collect::<Vec<_>>(),
    );
    filters.update_property(&[gtk::accessible::Property::Label("Вариант ответа")]);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        item.set_child(Some(
            &gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .max_width_chars(32)
                .build(),
        ));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        if let (Some(label), Some(text)) = (
            item.child().and_downcast::<gtk::Label>(),
            item.item().and_downcast::<gtk::StringObject>(),
        ) {
            label.set_text(&text.string());
        }
    });
    filters.set_factory(Some(&factory));
    filters.set_list_factory(Some(&factory));
    controls.append(&filters);
    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["dim-label"])
        .build();
    controls.append(&status);
    toolbar.add_top_bar(&controls);
    let rows = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    body.append(&rows);
    let more = gtk::Button::with_label("Показать ещё");
    more.set_halign(gtk::Align::Center);
    more.set_margin_top(6);
    more.set_margin_bottom(12);
    more.set_visible(false);
    toolbar.add_bottom_bar(&more);
    let state = Rc::new(VoterList {
        context,
        options: poll
            .choices
            .iter()
            .map(|choice| choice.option.clone())
            .collect(),
        filters,
        status,
        rows,
        more,
        next: RefCell::new(None),
        loaded_offsets: RefCell::new(HashSet::new()),
        peers: RefCell::new(HashSet::new()),
        pending: RefCell::new(None),
        generation: Cell::new(0),
    });
    state.filters.connect_selected_notify(glib::clone!(
        #[weak]
        state,
        move |_| state.reset()
    ));
    state.more.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.load()
    ));
    dialog.connect_closed({
        let state = state.clone();
        move |_| state.cancel()
    });
    dialog.set_focus(Some(&state.filters));
    state.load();
    dialog
}

struct VoterList {
    context: VoterContext,
    options: Vec<Vec<u8>>,
    filters: gtk::DropDown,
    status: gtk::Label,
    rows: gtk::ListBox,
    more: gtk::Button,
    next: RefCell<Option<String>>,
    loaded_offsets: RefCell<HashSet<String>>,
    peers: RefCell<HashSet<i64>>,
    pending: RefCell<Option<AbortHandle>>,
    generation: Cell<u64>,
}

impl VoterList {
    fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(task) = self.pending.take() {
            task.abort();
        }
    }

    fn reset(self: &Rc<Self>) {
        self.cancel();
        self.next.take();
        self.loaded_offsets.borrow_mut().clear();
        self.peers.borrow_mut().clear();
        while let Some(row) = self.rows.first_child() {
            self.rows.remove(&row);
        }
        self.more.set_visible(false);
        self.load();
    }

    fn load(self: &Rc<Self>) {
        if self.pending.borrow().is_some() {
            return;
        }
        let option = self
            .filters
            .selected()
            .checked_sub(1)
            .and_then(|index| self.options.get(index as usize))
            .cloned();
        let show_answers = option.is_none();
        let offset = self.next.borrow().clone();
        let request = (self.context.load)(option, offset.clone());
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(15), request)
                .await
                .context("Telegram не прислал список вовремя")?
        });
        self.pending.replace(Some(task.abort_handle()));
        self.more.set_label("Загрузка…");
        self.status.set_text("Загрузка списка…");
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|page| page);
            let Some(state) = weak
                .upgrade()
                .filter(|state| state.generation.get() == generation)
            else {
                return;
            };
            state.pending.take();
            match result {
                Ok(page) => state.show(page, offset, show_answers),
                Err(error) => {
                    state
                        .status
                        .set_text(&format!("Не удалось загрузить список. {error}"));
                    state.more.set_label("Повторить");
                    state.more.set_visible(true);
                }
            }
        });
    }

    fn show(&self, page: PollVoterPage, offset: Option<String>, show_answers: bool) {
        let mut offsets = self.loaded_offsets.borrow_mut();
        offsets.extend(offset);
        let next = page.next.filter(|next| !offsets.contains(next));
        let mut peers = self.peers.borrow_mut();
        for voter in page.voters {
            if !peers.insert(voter.peer) {
                continue;
            }
            let row = adw::ActionRow::builder()
                .title(&voter.name)
                .title_lines(1)
                .subtitle_lines(2)
                .use_markup(false)
                .activatable(false)
                .selectable(false)
                .build();
            if show_answers && !voter.answers.is_empty() {
                row.set_subtitle(&voter.answers.join(", "));
            }
            let avatar = adw::Avatar::new(32, Some(&voter.name), true);
            if let Some(photo) = voter.avatar {
                (self.context.avatar)(&avatar, photo);
            }
            row.add_prefix(&avatar);
            self.rows.append(&row);
        }
        if peers.is_empty() && next.is_none() {
            self.status.set_text("Голоса не найдены.");
        } else {
            self.status
                .set_text(&format!("Проголосовали: {}", page.total));
        }
        self.more.set_label("Показать ещё");
        if next.is_none() && self.more.has_focus() {
            self.filters.grab_focus();
        }
        self.more.set_visible(next.is_some());
        self.next.replace(next);
    }
}
