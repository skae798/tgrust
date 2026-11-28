use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::PlaybackSource;

type Load = Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<PlaybackSource>> + Send>>>;

mod frame {
    use super::*;
    use gtk::gdk::subclass::prelude::*;

    #[derive(Default)]
    pub struct SizedFrame(pub std::cell::OnceCell<gtk::gdk::Paintable>);

    #[glib::object_subclass]
    impl ObjectSubclass for SizedFrame {
        const NAME: &'static str = "TgrustStickerFrame";
        type Type = super::StickerFrame;
        type Interfaces = (gtk::gdk::Paintable,);
    }

    impl ObjectImpl for SizedFrame {}

    impl SizedFrame {
        fn size(&self) -> (i32, i32) {
            let Some(source) = self.0.get() else {
                return (160, 160);
            };
            let width = source.intrinsic_width().max(1);
            let height = source.intrinsic_height().max(1);
            let scale = 160.0 / f64::from(width.max(height));
            (
                (f64::from(width) * scale).round().max(1.0) as i32,
                (f64::from(height) * scale).round().max(1.0) as i32,
            )
        }
    }

    impl PaintableImpl for SizedFrame {
        fn intrinsic_width(&self) -> i32 {
            self.size().0
        }
        fn intrinsic_height(&self) -> i32 {
            self.size().1
        }
        fn intrinsic_aspect_ratio(&self) -> f64 {
            let (width, height) = self.size();
            f64::from(width) / f64::from(height)
        }
        fn flags(&self) -> gtk::gdk::PaintableFlags {
            gtk::gdk::PaintableFlags::empty()
        }
        fn snapshot(&self, snapshot: &gtk::gdk::Snapshot, width: f64, height: f64) {
            if let Some(source) = self.0.get() {
                source.snapshot(snapshot, width, height);
            }
        }
    }
}

glib::wrapper! {
    pub struct StickerFrame(ObjectSubclass<frame::SizedFrame>) @implements gtk::gdk::Paintable;
}

impl StickerFrame {
    fn new(source: &impl IsA<gtk::gdk::Paintable>) -> Self {
        use glib::subclass::prelude::ObjectSubclassIsExt;
        let frame: Self = glib::Object::new();
        frame.imp().0.set(source.as_ref().clone()).unwrap();
        source.connect_invalidate_contents(glib::clone!(
            #[weak]
            frame,
            move |_| frame.invalidate_contents()
        ));
        source.connect_invalidate_size(glib::clone!(
            #[weak]
            frame,
            move |_| frame.invalidate_size()
        ));
        frame
    }
}

struct Player {
    button: glib::WeakRef<gtk::Button>,
    stack: gtk::Stack,
    picture: gtk::Picture,
    indicator: gtk::Image,
    runtime: Handle,
    load: Load,
    source: RefCell<Option<PlaybackSource>>,
    media: RefCell<Option<gtk::MediaFile>>,
    task: RefCell<Option<tokio::task::AbortHandle>>,
    timer: RefCell<Option<glib::SourceId>>,
    generation: Cell<u64>,
    active: Cell<bool>,
    paused: Cell<bool>,
    failed: Cell<bool>,
}

pub fn build(thumbnail: &gtk::Stack, runtime: Handle, load: Load) -> gtk::Button {
    build_player(thumbnail, runtime, load).0
}

fn build_player(thumbnail: &gtk::Stack, runtime: Handle, load: Load) -> (gtk::Button, Rc<Player>) {
    let stack = gtk::Stack::new();
    stack.add_named(thumbnail, Some("thumbnail"));
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .build();
    stack.add_named(&picture, Some("animation"));
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&stack));
    let indicator = gtk::Image::builder()
        .icon_name("media-playback-start-symbolic")
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .visible(false)
        .build();
    overlay.add_overlay(&indicator);
    let button = gtk::Button::builder().child(&overlay).build();
    button.add_css_class("flat");
    let state = Rc::new(Player {
        button: button.downgrade(),
        stack,
        picture,
        indicator,
        runtime,
        load,
        source: RefCell::default(),
        media: RefCell::default(),
        task: RefCell::default(),
        timer: RefCell::default(),
        generation: Cell::new(0),
        active: Cell::new(false),
        paused: Cell::new(false),
        failed: Cell::new(false),
    });
    state.hint(None);
    button.connect_map(glib::clone!(
        #[strong]
        state,
        move |_| {
            state.refresh();
            let weak = Rc::downgrade(&state);
            let timer = glib::timeout_add_local(Duration::from_millis(150), move || {
                let Some(state) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                state.refresh();
                glib::ControlFlow::Continue
            });
            if let Some(old) = state.timer.replace(Some(timer)) {
                old.remove();
            }
        }
    ));
    button.connect_unmap(glib::clone!(
        #[strong]
        state,
        move |_| {
            if let Some(timer) = state.timer.take() {
                timer.remove();
            }
            state.active.set(false);
            state.suspend();
            state.source.take();
        }
    ));
    button.connect_clicked(glib::clone!(
        #[strong]
        state,
        move |_| {
            if state.failed.replace(false) {
                state.activate();
            } else {
                state.paused.set(!state.paused.get());
                if let Some(media) = state.media.borrow().as_ref() {
                    if state.paused.get() {
                        media.pause();
                    } else {
                        media.play();
                    }
                }
                state.hint(None);
            }
        }
    ));
    (button, state)
}

impl Player {
    fn in_view(&self) -> bool {
        let Some(button) = self.button.upgrade().filter(WidgetExt::is_mapped) else {
            return false;
        };
        if button.width() <= 0 || button.height() <= 0 {
            return false;
        }
        let mut ancestor = button.parent();
        while let Some(widget) = ancestor {
            if widget.is::<gtk::ScrolledWindow>() {
                let Some(bounds) = button.compute_bounds(&widget) else {
                    return false;
                };
                if bounds.x() + bounds.width() <= 0.0
                    || bounds.y() + bounds.height() <= 0.0
                    || bounds.x() >= widget.width() as f32
                    || bounds.y() >= widget.height() as f32
                {
                    return false;
                }
            }
            ancestor = widget.parent();
        }
        true
    }

    fn refresh(self: &Rc<Self>) {
        let visible = self.in_view();
        if self.active.replace(visible) != visible {
            if visible {
                self.activate();
            } else {
                self.suspend();
            }
        }
    }

    fn activate(self: &Rc<Self>) {
        if self.failed.get() || self.task.borrow().is_some() || !self.active.get() {
            return;
        }
        if self.source.borrow().is_some() {
            self.play();
            return;
        }
        let generation = self.generation.get();
        self.hint(Some("Загрузка видеостикера…"));
        let task = self.runtime.spawn((self.load)());
        self.task.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            if let Some(state) = weak.upgrade()
                && generation == state.generation.get()
                && state.active.get()
            {
                state.task.take();
                match result {
                    Ok(source @ PlaybackSource::File(_)) => {
                        state.source.replace(Some(source));
                        state.play();
                    }
                    Ok(_) => state.fail("Видеостикер должен быть загружен целиком"),
                    Err(error) => state.fail(&format!("Не удалось загрузить видеостикер. {error}")),
                }
            }
        });
    }

    fn play(self: &Rc<Self>) {
        let Some(input) = self
            .source
            .borrow()
            .as_ref()
            .map(|source| crate::media_viewer::media_input(source, &self.runtime))
        else {
            return;
        };
        let media = gtk::MediaFile::new();
        self.media.replace(Some(media.clone()));
        // Keep Telegram's 512px source from widening the message in a narrow window.
        self.picture.set_paintable(Some(&StickerFrame::new(&media)));
        media.connect_prepared_notify(glib::clone!(
            #[weak(rename_to = state)]
            self,
            move |media| {
                if state.media.borrow().as_ref() == Some(media)
                    && media.is_prepared()
                    && media.error().is_none()
                {
                    state.stack.set_visible_child_name("animation");
                    state.hint(None);
                }
            }
        ));
        media.connect_error_notify(glib::clone!(
            #[weak(rename_to = state)]
            self,
            move |media| {
                let current = state.media.borrow().as_ref() == Some(media);
                if current && let Some(error) = media.error() {
                    state.fail(&format!("Не удалось воспроизвести видеостикер. {error}"));
                }
            }
        ));
        // GtkGstMediaFile reads loop while opening the source, and mute after binding.
        media.set_loop(true);
        input.bind(&media);
        if self.failed.get() {
            return;
        }
        media.set_muted(true);
        if !self.paused.get() && !self.failed.get() {
            media.play();
        }
        self.hint(None);
    }

    fn suspend(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(media) = self.media.take() {
            media.pause();
            media.clear();
        }
        self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        self.stack.set_visible_child_name("thumbnail");
    }

    fn fail(&self, message: &str) {
        self.failed.set(true);
        self.suspend();
        self.source.take();
        self.hint(Some(message));
    }

    fn hint(&self, status: Option<&str>) {
        let hint = if self.failed.get() {
            self.indicator.set_icon_name(Some("view-refresh-symbolic"));
            "Повторить воспроизведение видеостикера"
        } else if self.paused.get() {
            self.indicator
                .set_icon_name(Some("media-playback-start-symbolic"));
            "Продолжить видеостикер"
        } else {
            "Приостановить видеостикер"
        };
        self.indicator
            .set_visible(self.failed.get() || self.paused.get());
        if let Some(button) = self.button.upgrade() {
            button.set_tooltip_text(Some(
                &status.map_or_else(|| hint.into(), |s| format!("{s}\n{hint}")),
            ));
            button.update_property(&[gtk::accessible::Property::Label(hint)]);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.take() {
            timer.remove();
        }
        self.suspend();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    fn settle(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(12);
        while !ready() && Instant::now() < deadline {
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ready(), "Inline sticker did not settle");
    }

    #[test]
    #[ignore = "requires a graphical display and VP9/alpha codecs"]
    fn sticker_loops_in_its_message_and_releases_decoders_outside_the_viewport() {
        gtk::test_synced(|| {
            let app = adw::Application::builder()
                .application_id("io.github.skae798.tgrust.InlineStickerTest")
                .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
                .build();
            app.register(None::<&gtk::gio::Cancellable>).unwrap();
            let window = adw::ApplicationWindow::builder()
                .application(&app)
                .default_width(360)
                .default_height(300)
                .build();
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let scroll = gtk::ScrolledWindow::builder().child(&content).build();
            window.set_content(Some(&scroll));
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let attempts = Arc::new(AtomicUsize::new(0));
            let counter = attempts.clone();
            let thumbnail = gtk::Stack::new();
            thumbnail.set_size_request(160, 160);
            thumbnail.add_child(&gtk::Picture::for_paintable(
                &crate::stickers::demo_texture(),
            ));
            let (button, state) = build_player(
                &thumbnail,
                runtime.handle().clone(),
                Box::new(move || {
                    let attempt = counter.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async move {
                        if attempt == 0 {
                            anyhow::bail!("Synthetic interrupted download");
                        }
                        if attempt == 1 {
                            let mut file = tempfile::tempfile()?;
                            std::io::Write::write_all(&mut file, b"invalid synthetic WebM")?;
                            return Ok(PlaybackSource::File(crate::model::PlaybackFile {
                                file,
                                media: crate::demo::sticker_video_media(),
                            }));
                        }
                        let (progress, _) = tokio::sync::watch::channel(Default::default());
                        crate::demo::sticker_video_preview(progress, true).await
                    })
                }),
            );
            content.append(&button);
            content.append(&gtk::Box::builder().height_request(1200).build());
            window.present();
            let windows = gtk::Window::list_toplevels().len();
            settle(|| state.failed.get());
            assert!(
                button
                    .tooltip_text()
                    .unwrap()
                    .contains("Synthetic interrupted download")
            );
            assert!(button.grab_focus());
            assert!(gtk::prelude::WidgetExt::activate(&button));
            settle(|| {
                attempts.load(Ordering::SeqCst) >= 2
                    && state.failed.get()
                    && state.task.borrow().is_none()
            });
            assert!(state.media.borrow().is_none());
            assert!(state.source.borrow().is_none());
            assert!(
                button
                    .tooltip_text()
                    .unwrap()
                    .contains("Не удалось воспроизвести")
            );
            assert!(gtk::prelude::WidgetExt::activate(&button));
            settle(|| {
                state
                    .media
                    .borrow()
                    .as_ref()
                    .is_some_and(|m| m.is_prepared())
            });
            let media = state.media.borrow().as_ref().unwrap().clone();
            let frames = Rc::new(Cell::new(0));
            media.connect_invalidate_contents(glib::clone!(
                #[strong]
                frames,
                move |_| frames.set(frames.get() + 1)
            ));
            settle(|| frames.get() > 45);
            assert!(media.is_loop());
            assert!(media.is_muted());
            assert!(media.error().is_none());
            assert!(button.width() <= 360, "Sticker widened its window");
            assert!(
                button.measure(gtk::Orientation::Horizontal, -1).1 <= 180,
                "Decoded sticker requested its original 512px width"
            );
            assert!(
                button.height() <= 180,
                "Sticker height grew to {}",
                button.height()
            );
            assert_eq!(gtk::Window::list_toplevels().len(), windows);
            assert_eq!(
                state.stack.visible_child_name().as_deref(),
                Some("animation")
            );
            assert!(gtk::prelude::WidgetExt::activate(&button));
            settle(|| !media.is_playing());
            let snapshot = gtk::Snapshot::new();
            media.current_image().snapshot(&snapshot, 160.0, 160.0);
            let texture = window.renderer().unwrap().render_texture(
                snapshot.to_node().expect("decoded sticker frame"),
                Some(&gtk::graphene::Rect::new(0.0, 0.0, 160.0, 160.0)),
            );
            let pixels = image::load_from_memory(texture.save_to_png_bytes().as_ref())
                .unwrap()
                .to_rgba8();
            assert_eq!(pixels.get_pixel(0, 0)[3], 0, "Lost sticker transparency");
            assert!(pixels.get_pixel(80, 80)[3] > 200, "Empty frame");
            assert!(gtk::prelude::WidgetExt::activate(&button));
            settle(|| media.is_playing());
            scroll.vadjustment().set_value(700.0);
            settle(|| state.media.borrow().is_none());
            assert!(!media.is_playing());
            assert!(state.source.borrow().is_some());
            scroll.vadjustment().set_value(0.0);
            settle(|| {
                state
                    .media
                    .borrow()
                    .as_ref()
                    .is_some_and(|m| m.is_prepared())
            });
            assert_eq!(
                attempts.load(Ordering::SeqCst),
                3,
                "Scrolling redownloaded the sticker"
            );
            let resumed = state.media.borrow().as_ref().unwrap().clone();
            content.remove(&button);
            assert!(!resumed.is_playing());
            assert!(state.source.borrow().is_none());
            assert!(state.task.borrow().is_none());
            assert!(state.timer.borrow().is_none());
            // A cancelled load must not attach its result to a remapped message or clear its task.
            content.prepend(&button);
            settle(|| state.task.borrow().is_some());
            assert_eq!(attempts.load(Ordering::SeqCst), 4);
            content.remove(&button);
            assert!(state.task.borrow().is_none());
            content.prepend(&button);
            settle(|| {
                state
                    .media
                    .borrow()
                    .as_ref()
                    .is_some_and(|m| m.is_prepared())
            });
            assert_eq!(attempts.load(Ordering::SeqCst), 5);
            assert!(state.task.borrow().is_none());
            content.remove(&button);
            assert!(state.source.borrow().is_none());
            assert_eq!(gtk::Window::list_toplevels().len(), windows);
            window.close();
        });
    }
}
