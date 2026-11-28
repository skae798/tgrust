use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;

use crate::model::{PlaybackSource, StickerAnimationSource};

type Load = Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<StickerAnimationSource>> + Send>>>;

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
    #[cfg(test)]
    fn image(&self) -> gtk::gdk::Texture {
        use glib::subclass::prelude::ObjectSubclassIsExt;
        self.imp()
            .0
            .get()
            .unwrap()
            .clone()
            .downcast::<gtk::gdk::Texture>()
            .unwrap()
    }
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

enum Animation {
    Vector(crate::tgs::Renderer),
    Video(crate::sticker_video::Frames),
}

impl Animation {
    fn timing(&self) -> crate::model::StickerAnimationTiming {
        match self {
            Self::Vector(renderer) => renderer.timing,
            Self::Video(frames) => frames.timing,
        }
    }
    fn request(&self, frame: usize) -> Option<tokio::sync::oneshot::Receiver<Vec<u8>>> {
        match self {
            Self::Vector(renderer) => renderer.request(frame),
            Self::Video(frames) => frames.request(frame),
        }
    }
}

type AnimationLoad = Pin<Box<dyn Future<Output = Result<Animation>> + Send>>;

fn load_animation(source: &StickerAnimationSource) -> Result<AnimationLoad> {
    match source {
        StickerAnimationSource::Tgs(bytes) => {
            let bytes = bytes.clone();
            Ok(Box::pin(async move {
                crate::tgs::Renderer::open(bytes)
                    .await
                    .map(Animation::Vector)
            }))
        }
        StickerAnimationSource::Video(PlaybackSource::File(file)) => {
            let file = crate::model::PlaybackFile {
                file: file.file.try_clone()?,
                media: file.media,
            };
            Ok(Box::pin(async move {
                crate::sticker_video::Frames::open(file)
                    .await
                    .map(Animation::Video)
            }))
        }
        _ => anyhow::bail!("Стикер должен быть загружен целиком"),
    }
}

struct Player {
    button: glib::WeakRef<gtk::Button>,
    stack: gtk::Stack,
    picture: gtk::Picture,
    indicator: gtk::Image,
    runtime: Handle,
    load: Load,
    source: RefCell<Option<StickerAnimationSource>>,
    renderer: RefCell<Option<Animation>>,
    frame_timer: RefCell<Option<glib::SourceId>>,
    frame_pending: Cell<bool>,
    frame_index: Cell<Option<usize>>,
    elapsed: Cell<Duration>,
    last_tick: Cell<Instant>,
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
        renderer: RefCell::default(),
        frame_timer: RefCell::default(),
        frame_pending: Cell::new(false),
        frame_index: Cell::new(None),
        elapsed: Cell::new(Duration::ZERO),
        last_tick: Cell::new(Instant::now()),
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
        self.hint(Some("Загрузка стикера…"));
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
                    Ok(
                        source @ (StickerAnimationSource::Video(PlaybackSource::File(_))
                        | StickerAnimationSource::Tgs(_)),
                    ) => {
                        state.source.replace(Some(source));
                        state.play();
                    }
                    Ok(_) => state.fail("Стикер должен быть загружен целиком"),
                    Err(error) => state.fail(&format!("Не удалось загрузить стикер. {error}")),
                }
            }
        });
    }

    fn play(self: &Rc<Self>) {
        let Some(load) = self.source.borrow().as_ref().map(load_animation) else {
            return;
        };
        let load = match load {
            Ok(load) => load,
            Err(error) => {
                self.fail(&format!("Не удалось воспроизвести стикер. {error}"));
                return;
            }
        };
        let generation = self.generation.get();
        let task = self.runtime.spawn(load);
        self.task.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            if let Some(state) = weak.upgrade()
                && state.generation.get() == generation
                && state.active.get()
            {
                state.task.take();
                match result {
                    Ok(renderer) => {
                        state.renderer.replace(Some(renderer));
                        state.last_tick.set(Instant::now());
                        state.frame_tick();
                        let weak = Rc::downgrade(&state);
                        let timer = glib::timeout_add_local(Duration::from_millis(16), move || {
                            let Some(state) = weak.upgrade() else {
                                return glib::ControlFlow::Break;
                            };
                            state.frame_tick();
                            glib::ControlFlow::Continue
                        });
                        state.frame_timer.replace(Some(timer));
                        state.hint(None);
                    }
                    Err(error) => state.fail(&format!("Не удалось воспроизвести стикер. {error}")),
                }
            }
        });
    }

    fn frame_tick(self: &Rc<Self>) {
        let now = Instant::now();
        let delta = now.saturating_duration_since(self.last_tick.replace(now));
        if !self.paused.get() {
            self.elapsed.set(self.elapsed.get() + delta);
        }
        if self.frame_pending.get() {
            return;
        }
        let Some((frame, receiver)) = self.renderer.borrow().as_ref().and_then(|renderer| {
            let timing = renderer.timing();
            let frame = (self.elapsed.get().as_secs_f64() * timing.fps) as usize % timing.frames;
            if self.frame_index.get() == Some(frame) {
                return None;
            }
            renderer.request(frame).map(|receiver| (frame, receiver))
        }) else {
            return;
        };
        self.frame_pending.set(true);
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = receiver.await;
            if let Some(state) = weak.upgrade()
                && state.generation.get() == generation
                && state.active.get()
            {
                state.frame_pending.set(false);
                match result {
                    Ok(bytes) => {
                        let Some(timing) = state.renderer.borrow().as_ref().map(Animation::timing)
                        else {
                            return;
                        };
                        let texture = gtk::gdk::MemoryTexture::new(
                            timing.width as i32,
                            timing.height as i32,
                            gtk::gdk::MemoryFormat::B8g8r8a8Premultiplied,
                            &glib::Bytes::from_owned(bytes),
                            timing.width * 4,
                        );
                        state
                            .picture
                            .set_paintable(Some(&StickerFrame::new(&texture)));
                        state.stack.set_visible_child_name("animation");
                        state.frame_index.set(Some(frame));
                    }
                    Err(_) => state.fail("Обработчик анимации остановлен. Повторите попытку"),
                }
            }
        });
    }

    fn suspend(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(timer) = self.frame_timer.take() {
            timer.remove();
        }
        self.renderer.take();
        self.frame_pending.set(false);
        self.frame_index.set(None);
        self.elapsed.set(Duration::ZERO);
        if let Some(task) = self.task.take() {
            task.abort();
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
            "Повторить воспроизведение стикера"
        } else if self.paused.get() {
            self.indicator
                .set_icon_name(Some("media-playback-start-symbolic"));
            "Продолжить стикер"
        } else {
            "Приостановить стикер"
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
    #[ignore = "requires a graphical display"]
    fn tgs_sticker_loops_inline_and_stops_its_worker_when_scrolled_out() {
        gtk::test_synced(|| {
            let app = adw::Application::builder()
                .application_id("io.github.skae798.tgrust.InlineTgsTest")
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
                    counter.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async {
                        Ok(StickerAnimationSource::Tgs(
                            include_bytes!("../data/demo-sticker.tgs").to_vec(),
                        ))
                    })
                }),
            );
            content.append(&button);
            content.append(&gtk::Box::builder().height_request(1200).build());
            let frames = Rc::new(Cell::new(0));
            state.picture.connect_paintable_notify(glib::clone!(
                #[strong]
                frames,
                move |_| frames.set(frames.get() + 1)
            ));
            window.present();
            let windows = gtk::Window::list_toplevels().len();
            settle(|| frames.get() > 135);
            assert!(!state.failed.get());
            assert!(button.measure(gtk::Orientation::Horizontal, -1).1 <= 180);
            assert_eq!(gtk::Window::list_toplevels().len(), windows);
            assert!(button.grab_focus());
            assert!(gtk::prelude::WidgetExt::activate(&button));
            settle(|| state.paused.get() && !state.frame_pending.get());
            let paused = state.picture.paintable().unwrap();
            let waited = Rc::new(Cell::new(false));
            glib::timeout_add_local_once(
                Duration::from_millis(200),
                glib::clone!(
                    #[strong]
                    waited,
                    move || waited.set(true)
                ),
            );
            settle(|| waited.get());
            assert_eq!(
                state.picture.paintable().as_ref(),
                Some(&paused),
                "Paused TGS changed its frame"
            );
            let texture = paused.clone().downcast::<StickerFrame>().unwrap().image();
            let pixels = image::load_from_memory(texture.save_to_png_bytes().as_ref())
                .unwrap()
                .to_rgba8();
            assert_eq!(pixels.get_pixel(0, 0)[3], 0);
            assert!(pixels.get_pixel(pixels.width() / 2, pixels.height() / 2)[3] > 200);
            assert!(
                pixels.get_pixel(pixels.width() / 2, pixels.height() / 2)[0]
                    > pixels.get_pixel(pixels.width() / 2, pixels.height() / 2)[2],
                "BGRA frame changed colors during display"
            );
            assert!(gtk::prelude::WidgetExt::activate(&button));
            settle(|| !state.paused.get() && state.picture.paintable().as_ref() != Some(&paused));
            scroll.vadjustment().set_value(700.0);
            settle(|| state.renderer.borrow().is_none());
            assert!(state.frame_timer.borrow().is_none());
            assert!(state.source.borrow().is_some());
            scroll.vadjustment().set_value(0.0);
            settle(|| state.renderer.borrow().is_some() && state.picture.paintable().is_some());
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            content.remove(&button);
            assert!(state.renderer.borrow().is_none());
            assert!(state.frame_timer.borrow().is_none());
            assert!(state.source.borrow().is_none());
            assert!(state.task.borrow().is_none());
            window.close();
        });
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
                            return Ok(StickerAnimationSource::Video(PlaybackSource::File(
                                crate::model::PlaybackFile {
                                    file,
                                    media: crate::demo::sticker_video_media(),
                                },
                            )));
                        }
                        let (progress, _) = tokio::sync::watch::channel(Default::default());
                        crate::demo::sticker_video_preview(progress, true)
                            .await
                            .map(StickerAnimationSource::Video)
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
            assert!(state.source.borrow().is_none());
            assert!(
                button
                    .tooltip_text()
                    .unwrap()
                    .contains("Не удалось воспроизвести")
            );
            assert!(gtk::prelude::WidgetExt::activate(&button));
            settle(|| state.renderer.borrow().is_some() && state.picture.paintable().is_some());
            let count = state.renderer.borrow().as_ref().unwrap().timing().frames;
            let frames = Rc::new(Cell::new(0));
            state.picture.connect_paintable_notify(glib::clone!(
                #[strong]
                frames,
                move |_| frames.set(frames.get() + 1)
            ));
            settle(|| frames.get() > count + 15);
            assert!(!state.failed.get());
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
            settle(|| state.paused.get() && !state.frame_pending.get());
            let paused = state.picture.paintable().unwrap();
            let texture = paused.clone().downcast::<StickerFrame>().unwrap().image();
            let pixels = image::load_from_memory(texture.save_to_png_bytes().as_ref())
                .unwrap()
                .to_rgba8();
            assert_eq!(pixels.get_pixel(0, 0)[3], 0, "Lost sticker transparency");
            assert!(
                pixels.get_pixel(pixels.width() / 2, pixels.height() / 2)[3] > 200,
                "Empty frame"
            );
            assert!(gtk::prelude::WidgetExt::activate(&button));
            settle(|| !state.paused.get() && state.picture.paintable().as_ref() != Some(&paused));
            scroll.vadjustment().set_value(700.0);
            settle(|| state.renderer.borrow().is_none());
            assert!(state.frame_timer.borrow().is_none());
            assert!(state.source.borrow().is_some());
            scroll.vadjustment().set_value(0.0);
            settle(|| state.renderer.borrow().is_some() && state.picture.paintable().is_some());
            assert_eq!(
                attempts.load(Ordering::SeqCst),
                3,
                "Scrolling redownloaded the sticker"
            );
            content.remove(&button);
            assert!(state.renderer.borrow().is_none());
            assert!(state.frame_timer.borrow().is_none());
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
            settle(|| state.renderer.borrow().is_some() && state.picture.paintable().is_some());
            assert_eq!(attempts.load(Ordering::SeqCst), 5);
            assert!(state.task.borrow().is_none());
            content.remove(&button);
            assert!(state.source.borrow().is_none());
            assert_eq!(gtk::Window::list_toplevels().len(), windows);
            window.close();
        });
    }
}
