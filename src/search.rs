use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::{Chat, SEARCH_PAGE_SIZE, SearchHandle, SearchHit, SearchPage};
use crate::telegram::Telegram;

pub struct SearchContext {
    pub runtime: Handle,
    pub client: Option<Arc<Telegram>>,
    pub demo_hits: Vec<SearchHit>,
    pub chat: Option<Chat>,
    pub selected: Box<dyn Fn(SearchHit)>,
}

enum Source {
    Live(Arc<SearchHandle>),
    Demo(VecDeque<SearchHit>),
}

struct SearchDialog {
    context: SearchContext,
    dialog: glib::WeakRef<adw::Dialog>,
    search: gtk::SearchEntry,
    scope: gtk::DropDown,
    status: gtk::Label,
    results: gtk::Box,
    scroll: gtk::ScrolledWindow,
    more: gtk::Button,
    source: RefCell<Option<Source>>,
    task: RefCell<Option<tokio::task::AbortHandle>>,
    generation: Cell<u64>,
    count: Cell<usize>,
    busy: Cell<bool>,
    has_more: Cell<bool>,
    auto_load: Cell<bool>,
    closed: Cell<bool>,
}

pub fn open(window: &adw::ApplicationWindow, context: SearchContext, scoped: bool) {
    let dialog = adw::Dialog::builder()
        .title("Поиск по сообщениям")
        .content_width(520)
        .content_height(480)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_start(14);
    content.set_margin_end(14);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    let names = gtk::StringList::new(&["Все чаты"]);
    if let Some(chat) = &context.chat {
        names.append(&chat.title);
    }
    let scope = gtk::DropDown::new(Some(names), None::<gtk::Expression>);
    scope.set_selected(u32::from(scoped && context.chat.is_some()));
    scope.set_visible(context.chat.is_some());
    scope.update_property(&[gtk::accessible::Property::Label("Область поиска")]);
    content.append(&scope);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Текст сообщения")
        .hexpand(true)
        .build();
    let submit = gtk::Button::with_label("Найти");
    submit.add_css_class("suggested-action");
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar.append(&search);
    bar.append(&submit);
    content.append(&bar);
    let status = gtk::Label::new(None);
    status.add_css_class("dim-label");
    status.set_wrap(true);
    content.append(&status);
    let results = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let more = gtk::Button::with_label("Показать ещё");
    more.add_css_class("flat");
    more.set_visible(false);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
    list.append(&results);
    list.append(&more);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    content.append(&scroll);
    toolbar.set_content(Some(&content));
    dialog.set_child(Some(&toolbar));
    dialog.set_focus(Some(&search));
    let state = Rc::new(SearchDialog {
        context,
        dialog: dialog.downgrade(),
        search,
        scope,
        status,
        results,
        scroll,
        more,
        source: RefCell::new(None),
        task: RefCell::new(None),
        generation: Cell::new(0),
        count: Cell::new(0),
        busy: Cell::new(false),
        has_more: Cell::new(false),
        auto_load: Cell::new(true),
        closed: Cell::new(false),
    });
    submit.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.start()
    ));
    state.search.connect_activate(glib::clone!(
        #[weak]
        state,
        move |_| state.start()
    ));
    state.search.connect_changed(glib::clone!(
        #[weak]
        state,
        move |_| state.reset()
    ));
    state.search.connect_stop_search(glib::clone!(
        #[weak]
        state,
        move |_| {
            if let Some(dialog) = state.dialog.upgrade() {
                dialog.close();
            }
        }
    ));
    state.scope.connect_selected_notify(glib::clone!(
        #[weak]
        state,
        move |_| state.start()
    ));
    state.more.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| state.load_more()
    ));
    let adjustment = state.scroll.vadjustment();
    adjustment.connect_value_changed(glib::clone!(
        #[weak]
        state,
        move |_| state.maybe_load_more()
    ));
    adjustment.connect_changed(glib::clone!(
        #[weak]
        state,
        move |_| state.maybe_load_more()
    ));
    dialog.connect_closed(move |_| {
        state.closed.set(true);
        state.reset();
    });
    dialog.present(Some(window));
}

impl SearchDialog {
    fn reset(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.source.take();
        self.count.set(0);
        self.busy.set(false);
        self.has_more.set(false);
        self.auto_load.set(true);
        self.more.set_visible(false);
        while let Some(child) = self.results.first_child() {
            self.results.remove(&child);
        }
        self.status.set_text("");
        self.scroll.vadjustment().set_value(0.0);
    }

    fn start(self: &Rc<Self>) {
        self.reset();
        let query = self.search.text().trim().to_owned();
        if query.is_empty() || self.closed.get() {
            return;
        }
        let chat = (self.scope.selected() == 1)
            .then(|| self.context.chat.as_ref().map(|chat| chat.id))
            .flatten();
        self.status.set_text("Поиск…");
        if let Some(client) = self.context.client.clone() {
            self.run(
                async move {
                    let handle = Arc::new(client.search(&query, chat).await?);
                    let page = handle.next_page().await?;
                    Ok((handle, page))
                },
                |state, result| match result {
                    Ok((handle, page)) => {
                        state.source.replace(Some(Source::Live(handle)));
                        state.append_page(page);
                    }
                    Err(error) => state.failed(error),
                },
            );
        } else {
            let query = query.to_lowercase();
            let hits = self
                .context
                .demo_hits
                .iter()
                .filter(|hit| {
                    chat.is_none_or(|chat| hit.chat.id == chat)
                        && hit.message.text.to_lowercase().contains(&query)
                })
                .cloned()
                .collect();
            self.source.replace(Some(Source::Demo(hits)));
            self.has_more.set(true);
            self.load_more();
        }
    }

    fn run<T, F, C>(self: &Rc<Self>, future: F, complete: C)
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
        C: FnOnce(&Rc<Self>, Result<T>) + 'static,
    {
        self.busy.set(true);
        self.more.set_sensitive(false);
        let generation = self.generation.get();
        let task = self.context.runtime.spawn(future);
        self.task.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            if let Some(state) = weak.upgrade()
                && generation == state.generation.get()
                && !state.closed.get()
            {
                state.task.take();
                state.busy.set(false);
                state.more.set_sensitive(true);
                complete(&state, result);
            }
        });
    }

    fn load_more(self: &Rc<Self>) {
        if self.closed.get() || self.busy.get() || !self.has_more.get() {
            return;
        }
        let mut source = self.source.borrow_mut();
        match source.as_mut() {
            Some(Source::Live(handle)) => {
                let handle = handle.clone();
                drop(source);
                self.status.set_text("Поиск…");
                self.run(
                    async move { handle.next_page().await },
                    |state, result| match result {
                        Ok(page) => state.append_page(page),
                        Err(error) => state.failed(error),
                    },
                );
            }
            Some(Source::Demo(hits)) => {
                let page = SearchPage {
                    hits: hits.drain(..hits.len().min(SEARCH_PAGE_SIZE)).collect(),
                    has_more: !hits.is_empty(),
                };
                drop(source);
                self.append_page(page);
            }
            None => {}
        }
    }

    fn maybe_load_more(self: &Rc<Self>) {
        let adjustment = self.scroll.vadjustment();
        if self.auto_load.get()
            && adjustment.value() + adjustment.page_size() >= adjustment.upper() - 80.0
        {
            self.load_more();
        }
    }

    fn failed(&self, error: anyhow::Error) {
        self.auto_load.set(false);
        self.status
            .set_text(&format!("Не удалось выполнить поиск. {error}"));
        self.more.set_label("Повторить");
        self.more.set_visible(self.has_more.get());
    }

    fn append_page(self: &Rc<Self>, page: SearchPage) {
        self.busy.set(true);
        self.count.set(self.count.get() + page.hits.len());
        self.has_more.set(page.has_more);
        self.auto_load.set(true);
        self.more.set_label("Показать ещё");
        self.more.set_visible(page.has_more);
        self.status.set_text(&if self.count.get() == 0 {
            "Сообщения не найдены".to_owned()
        } else {
            format!("Показано сообщений: {}", self.count.get())
        });
        for hit in page.hits {
            let button = gtk::Button::new();
            button.add_css_class("flat");
            let body = gtk::Box::new(gtk::Orientation::Vertical, 3);
            let title = gtk::Label::new(Some(&hit.chat.title));
            title.set_xalign(0.0);
            title.add_css_class("heading");
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            let excerpt = gtk::Label::new(Some(&hit.message.reply_summary()));
            excerpt.set_xalign(0.0);
            excerpt.add_css_class("dim-label");
            excerpt.set_ellipsize(gtk::pango::EllipsizeMode::End);
            body.append(&title);
            body.append(&excerpt);
            button.set_child(Some(&body));
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |_| {
                    if let Some(dialog) = state.dialog.upgrade() {
                        (state.context.selected)(hit.clone());
                        dialog.close();
                    }
                }
            ));
            self.results.append(&button);
        }
        self.busy.set(false);
    }
}
