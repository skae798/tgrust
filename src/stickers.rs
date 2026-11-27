use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::{Sticker, StickerCollection, StickerKind, StickerSource};
use crate::telegram::Telegram;

type SendSticker = Pin<Box<dyn Future<Output = Result<()>>>>;

pub struct ContextData {
    pub runtime: Handle,
    pub client: Option<Arc<Telegram>>,
    pub send: Box<dyn Fn(Sticker, i64) -> SendSticker>,
}

struct Picker {
    context: ContextData,
    dialog: glib::WeakRef<adw::Dialog>,
    collections: RefCell<Vec<StickerCollection>>,
    names: gtk::StringList,
    source: gtk::DropDown,
    flow: gtk::FlowBox,
    status: gtk::Label,
    retry: gtk::Button,
    request: RefCell<Option<tokio::task::AbortHandle>>,
    images: RefCell<Vec<tokio::task::AbortHandle>>,
    generation: Cell<u64>,
    closed: Cell<bool>,
    sending: Cell<bool>,
    attempt: Cell<Option<(i64, i64)>>,
}

pub fn open(window: &adw::ApplicationWindow, context: ContextData) -> adw::Dialog {
    build(window, context).0
}

fn build(window: &adw::ApplicationWindow, context: ContextData) -> (adw::Dialog, Rc<Picker>) {
    let dialog = adw::Dialog::builder()
        .title("Стикеры")
        .content_width(420)
        .content_height(480)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_top(8);
    body.set_margin_bottom(12);
    let names = gtk::StringList::new(&[]);
    let source = gtk::DropDown::new(Some(names.clone()), None::<gtk::Expression>);
    source.set_enable_search(true);
    source.update_property(&[gtk::accessible::Property::Label("Набор стикеров")]);
    body.append(&source);
    let status = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .build();
    status.add_css_class("dim-label");
    body.append(&status);
    let retry = gtk::Button::with_label("Повторить загрузку");
    retry.set_visible(false);
    body.append(&retry);
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(4)
        .row_spacing(6)
        .column_spacing(6)
        .valign(gtk::Align::Start)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&flow)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    body.append(&scroll);
    toolbar.set_content(Some(&body));
    dialog.set_child(Some(&toolbar));
    dialog.set_focus(Some(&source));
    let state = Rc::new(Picker {
        context,
        dialog: dialog.downgrade(),
        collections: RefCell::new(Vec::new()),
        names,
        source,
        flow,
        status,
        retry,
        request: RefCell::new(None),
        images: RefCell::new(Vec::new()),
        generation: Cell::new(0),
        closed: Cell::new(false),
        sending: Cell::new(false),
        attempt: Cell::new(None),
    });
    state.source.connect_selected_notify(glib::clone!(
        #[weak]
        state,
        move |_| state.load()
    ));
    state.retry.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| {
            if state.collections.borrow().is_empty() {
                state.load_collections();
            } else {
                state.load();
            }
        }
    ));
    dialog.connect_closed({
        let state = state.clone();
        move |_| {
            state.closed.set(true);
            state.cancel_loading();
        }
    });
    state.load_collections();
    dialog.present(Some(window));
    (dialog, state)
}

impl Picker {
    fn cancel_loading(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(request) = self.request.take() {
            request.abort();
        }
        for image in self.images.take() {
            image.abort();
        }
    }

    fn run<T, F>(self: &Rc<Self>, future: F, complete: impl FnOnce(&Rc<Self>, Result<T>) + 'static)
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let generation = self.generation.get();
        let task = self.context.runtime.spawn(async move {
            tokio::time::timeout(Duration::from_secs(25), future)
                .await
                .context("Telegram не ответил вовремя")?
        });
        self.request.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            if let Some(state) = weak.upgrade()
                && !state.closed.get()
                && state.generation.get() == generation
            {
                state.request.take();
                complete(&state, result);
            }
        });
    }

    fn fail(&self, error: anyhow::Error) {
        self.status
            .set_text(&format!("Не удалось загрузить стикеры. {error}"));
        self.retry.set_visible(true);
        self.source.set_sensitive(true);
    }

    fn load_collections(self: &Rc<Self>) {
        self.cancel_loading();
        self.status.set_text("Загрузка наборов…");
        self.retry.set_visible(false);
        self.source.set_sensitive(false);
        if let Some(client) = self.context.client.clone() {
            self.run(
                async move { client.sticker_collections().await },
                |state, result| match result {
                    Ok(collections) => state.set_collections(collections),
                    Err(error) => state.fail(error),
                },
            );
        } else {
            self.set_collections(demo_collections());
        }
    }

    fn set_collections(self: &Rc<Self>, collections: Vec<StickerCollection>) {
        let labels = collections
            .iter()
            .map(|item| item.title.clone())
            .collect::<Vec<_>>();
        self.collections.replace(collections);
        let generation = self.generation.get();
        self.names.splice(
            0,
            self.names.n_items(),
            &labels.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        self.source.set_sensitive(true);
        // The first insertion emits selected-notify; load explicitly only if it did not.
        if self.generation.get() == generation {
            self.load();
        }
    }

    fn load(self: &Rc<Self>) {
        if self.closed.get() || self.sending.get() {
            return;
        }
        let Some(source) = self
            .collections
            .borrow()
            .get(self.source.selected() as usize)
            .map(|item| item.source)
        else {
            return;
        };
        self.cancel_loading();
        self.status.set_text("Загрузка стикеров…");
        self.retry.set_visible(false);
        while let Some(child) = self.flow.first_child() {
            self.flow.remove(&child);
        }
        if let Some(client) = self.context.client.clone() {
            self.run(
                async move { client.sticker_list(source).await },
                |state, result| match result {
                    Ok(stickers) => state.render(stickers),
                    Err(error) => state.fail(error),
                },
            );
        } else {
            self.render(demo_stickers(source));
        }
    }

    fn render(self: &Rc<Self>, stickers: Vec<Sticker>) {
        self.status.set_text(if stickers.is_empty() {
            "Здесь пока нет стикеров"
        } else {
            "Выберите стикер для отправки"
        });
        for sticker in stickers {
            let image = gtk::Stack::new();
            image.set_size_request(80, 80);
            let fallback = gtk::Label::new(Some(if sticker.emoji.is_empty() {
                "✦"
            } else {
                &sticker.emoji
            }));
            fallback.add_css_class("title-1");
            fallback.set_tooltip_text(Some("Изображение стикера недоступно"));
            image.add_named(&fallback, Some("fallback"));
            let picture = gtk::Picture::builder()
                .can_shrink(true)
                .content_fit(gtk::ContentFit::Contain)
                .build();
            image.add_named(&picture, Some("image"));
            let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
            body.append(&image);
            let caption = gtk::Label::new(Some(&sticker.emoji));
            caption.set_ellipsize(gtk::pango::EllipsizeMode::End);
            body.append(&caption);
            let button = gtk::Button::builder()
                .child(&body)
                .tooltip_text(sticker.label())
                .build();
            button.add_css_class("flat");
            button.update_property(&[gtk::accessible::Property::Label(&sticker.label())]);
            button.set_widget_name(&format!("sticker-{}", sticker.id));
            let id = sticker.id;
            let loaded = Cell::new(false);
            button.connect_map(glib::clone!(
                #[weak(rename_to = state)]
                self,
                #[weak]
                image,
                #[weak]
                picture,
                move |_| {
                    if !loaded.replace(true) {
                        state.load_image(id, &image, &picture);
                    }
                }
            ));
            button.connect_clicked(glib::clone!(
                #[weak(rename_to = state)]
                self,
                move |_| state.send(sticker.clone())
            ));
            self.flow.insert(&button, -1);
        }
    }

    fn load_image(self: &Rc<Self>, id: i64, image: &gtk::Stack, picture: &gtk::Picture) {
        let Some(client) = self.context.client.clone() else {
            picture.set_paintable(Some(&demo_texture()));
            image.set_visible_child_name("image");
            return;
        };
        let task = self.context.runtime.spawn(async move {
            client
                .chat_image(crate::model::ChatImage::Sticker(id))
                .await
        });
        self.images.borrow_mut().push(task.abort_handle());
        glib::MainContext::default().spawn_local(glib::clone!(
            #[weak]
            image,
            #[weak]
            picture,
            async move {
                if let Ok(Ok(bytes)) = task.await
                    && let Ok(texture) =
                        gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
                {
                    picture.set_paintable(Some(&texture));
                    image.set_visible_child_name("image");
                }
            }
        ));
    }

    fn send(self: &Rc<Self>, sticker: Sticker) {
        if self.closed.get() || self.sending.replace(true) {
            return;
        }
        self.source.set_sensitive(false);
        self.flow.set_sensitive(false);
        self.retry.set_visible(false);
        self.status.set_text("Отправка стикера…");
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.set_can_close(false);
        }
        let random_id = match self.attempt.get() {
            Some((id, random)) if id == sticker.id => random,
            _ => {
                let random =
                    (i64::from(glib::random_int()) << 32) | i64::from(glib::random_int()) | 1;
                self.attempt.set(Some((sticker.id, random)));
                random
            }
        };
        let future = (self.context.send)(sticker, random_id);
        glib::MainContext::default().spawn_local(glib::clone!(
            #[strong(rename_to = state)]
            self,
            async move {
                let result = future.await;
                state.sending.set(false);
                if let Some(dialog) = state.dialog.upgrade() {
                    dialog.set_can_close(true);
                    match result {
                        Ok(()) => {
                            dialog.close();
                        }
                        Err(error) => {
                            state
                                .status
                                .set_text(&format!("Не удалось отправить стикер. {error}"));
                            state.source.set_sensitive(true);
                            state.flow.set_sensitive(true);
                        }
                    }
                }
            }
        ));
    }
}

pub fn demo_collections() -> Vec<StickerCollection> {
    vec![
        StickerCollection {
            source: StickerSource::Recent,
            title: "Недавние".into(),
        },
        StickerCollection {
            source: StickerSource::Favorites,
            title: "Избранные".into(),
        },
        StickerCollection {
            source: StickerSource::Set(1),
            title: "Демо — космический кот".into(),
        },
    ]
}

pub fn demo_stickers(source: StickerSource) -> Vec<Sticker> {
    if source == StickerSource::Favorites {
        return Vec::new();
    }
    [
        (1, "🐱", StickerKind::Static),
        (2, "🚀", StickerKind::Animated),
        (3, "✨", StickerKind::Video),
    ]
    .into_iter()
    .map(|(id, emoji, kind)| Sticker {
        id,
        emoji: emoji.into(),
        kind,
    })
    .collect()
}

pub fn demo_texture() -> gtk::gdk::Texture {
    gtk::gdk::Texture::from_bytes(&glib::Bytes::from_static(include_bytes!(
        "../data/demo-sticker.png"
    )))
    .expect("valid demo sticker")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settle(mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !ready() && std::time::Instant::now() < deadline {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ready(), "Sticker UI did not settle");
    }

    #[test]
    #[ignore = "requires a graphical display"]
    fn picker_retries_keep_the_same_operation_and_other_choices_get_a_new_id() {
        gtk::test_synced(|| {
            let app = adw::Application::builder()
                .application_id("io.github.skae798.tgrust.StickerTest")
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
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let attempts = Rc::new(RefCell::new(Vec::new()));
            let sent = attempts.clone();
            let (dialog, state) = build(
                &window,
                ContextData {
                    runtime: runtime.handle().clone(),
                    client: None,
                    send: Box::new(move |sticker, random| {
                        sent.borrow_mut().push((sticker.id, random));
                        Box::pin(async {
                            anyhow::bail!("Вымышленная потеря ответа")
                        })
                    }),
                },
            );
            assert_eq!(state.flow.observe_children().n_items(), 3);
            assert!(state.flow.measure(gtk::Orientation::Horizontal, -1).0 <= 320);
            let choices = demo_stickers(StickerSource::Recent);
            let first = state
                .flow
                .child_at_index(0)
                .unwrap()
                .child()
                .unwrap()
                .downcast::<gtk::Button>()
                .unwrap();
            settle(|| first.is_mapped());
            assert!(first.grab_focus());
            assert!(gtk::prelude::WidgetExt::activate(&first));
            settle(|| !attempts.borrow().is_empty() && !state.sending.get());
            assert!(state.status.text().contains("Вымышленная потеря ответа"));
            assert!(dialog.can_close());
            assert!(state.flow.is_sensitive());
            state.send(choices[0].clone());
            settle(|| !state.sending.get());
            assert_eq!(attempts.borrow()[0], attempts.borrow()[1]);
            state.send(choices[1].clone());
            settle(|| !state.sending.get());
            assert_ne!(attempts.borrow()[1].1, attempts.borrow()[2].1);
            state.source.set_selected(1);
            assert_eq!(state.flow.observe_children().n_items(), 0);
            assert!(state.status.text().contains("пока нет"));
            state.source.set_selected(2);
            assert_eq!(state.flow.observe_children().n_items(), 3);
            dialog.close();
            settle(|| state.closed.get());
            window.close();
        });
    }
}
