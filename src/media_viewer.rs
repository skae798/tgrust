use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::model::{PlayableMedia, PlaybackFile, PlaybackProgress};

type PlaybackFuture = Pin<Box<dyn Future<Output = Result<PlaybackFile>> + Send>>;

pub struct PlaybackContext {
    pub runtime: Handle,
    pub media: PlayableMedia,
    pub audio: crate::model::AudioInfo,
    pub filename: Option<String>,
    pub cover: Option<gtk::gdk::Texture>,
    pub muted: bool,
    pub load: Box<dyn Fn(watch::Sender<PlaybackProgress>) -> PlaybackFuture>,
}

struct Viewer {
    context: PlaybackContext,
    window: glib::WeakRef<adw::Window>,
    header: adw::HeaderBar,
    content: gtk::Overlay,
    stack: gtk::Stack,
    visual: gtk::Stack,
    status: gtk::Label,
    progress: gtk::ProgressBar,
    error: gtk::Label,
    retry: gtk::Button,
    media: RefCell<Option<gtk::MediaFile>>,
    picture: gtk::Picture,
    controls: Rc<crate::playback_controls::PlaybackControls>,
    file: RefCell<Option<PlaybackFile>>,
    task: RefCell<Option<tokio::task::AbortHandle>>,
    screen: (i32, i32),
    closed: Cell<bool>,
}

pub fn open(parent: &adw::ApplicationWindow, context: PlaybackContext) -> adw::Window {
    let display = gtk::prelude::WidgetExt::display(parent);
    let monitor = parent
        .surface()
        .and_then(|surface| display.monitor_at_surface(&surface))
        .or_else(|| {
            display
                .monitors()
                .item(0)
                .and_downcast::<gtk::gdk::Monitor>()
        });
    let screen = monitor.map_or((640, 480), |monitor| {
        let bounds = monitor.geometry();
        (bounds.width(), bounds.height())
    });
    let window = adw::Window::builder()
        .title(context.media.title())
        .modal(true)
        .resizable(false)
        .build();
    window.set_display(&display);
    window.set_application(parent.application().as_ref());
    window.set_size_request(-1, -1);
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::builder()
        .show_title(false)
        .decoration_layout(":close")
        .build();
    toolbar.add_top_bar(&header);
    let content = gtk::Overlay::new();
    let stack = gtk::Stack::new();
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);
    content.add_overlay(&stack);
    content.set_clip_overlay(&stack, true);
    toolbar.set_content(Some(&content));
    let busy = gtk::Box::new(gtk::Orientation::Vertical, 12);
    busy.set_margin_start(18);
    busy.set_margin_end(18);
    busy.set_margin_top(18);
    busy.set_margin_bottom(18);
    let spinner = gtk::Spinner::new();
    spinner.set_size_request(28, 28);
    spinner.start();
    busy.append(&spinner);
    let status = gtk::Label::new(Some("Загрузка медиа…"));
    status.set_wrap(true);
    busy.append(&status);
    let progress = gtk::ProgressBar::new();
    busy.append(&progress);
    stack.add_named(&busy, Some("loading"));
    let failure = gtk::Box::new(gtk::Orientation::Vertical, 12);
    failure.set_margin_start(16);
    failure.set_margin_end(16);
    failure.set_margin_top(12);
    failure.set_margin_bottom(12);
    let error = gtk::Label::new(None);
    error.set_wrap(true);
    error.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    error.set_max_width_chars(36);
    failure.append(&error);
    let retry = gtk::Button::with_label("Повторить");
    retry.set_halign(gtk::Align::Center);
    failure.append(&retry);
    let failure_scroll = gtk::ScrolledWindow::builder()
        .child(&failure)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    stack.add_named(&failure_scroll, Some("error"));
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .build();
    picture.update_property(&[gtk::accessible::Property::Label("Воспроизведение видео")]);
    let visual = gtk::Stack::new();
    visual.set_hhomogeneous(false);
    visual.set_vhomogeneous(false);
    visual.add_named(&picture, Some("video"));
    let (audio, cover) =
        crate::audio::content(context.media, &context.audio, context.filename.as_deref());
    audio.set_margin_start(8);
    audio.set_margin_end(8);
    audio.set_halign(gtk::Align::Center);
    audio.set_valign(gtk::Align::Center);
    cover.set_paintable(context.cover.as_ref());
    visual.add_named(&audio, Some("audio"));
    stack.add_named(&visual, Some("player"));
    let controls = crate::playback_controls::PlaybackControls::new();
    controls.set_margin_start(8);
    controls.set_margin_end(8);
    controls.set_visible(false);
    toolbar.add_bottom_bar(&controls.widget);
    window.set_content(Some(&toolbar));
    let state = Rc::new(Viewer {
        context,
        window: window.downgrade(),
        header,
        content,
        stack,
        visual,
        status,
        progress,
        error,
        retry,
        media: RefCell::new(None),
        picture,
        controls,
        file: RefCell::new(None),
        task: RefCell::new(None),
        screen,
        closed: Cell::new(false),
    });
    state.retry.connect_clicked(glib::clone!(
        #[weak]
        state,
        move |_| {
            if state.file.borrow().is_some() {
                state.start_playback();
            } else {
                state.load();
            }
        }
    ));
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(glib::clone!(
        #[weak]
        state,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if let Some(window) = state.window.upgrade() {
                    window.close();
                }
                return glib::Propagation::Stop;
            }
            let media = state.media.borrow().clone();
            if let Some(media) = media
                && media.is_prepared()
                && media.error().is_none()
            {
                if key == gtk::gdk::Key::space {
                    if media.is_playing() {
                        media.pause();
                    } else {
                        if media.is_ended() && media.is_seekable() {
                            media.seek(0);
                        }
                        media.play();
                    }
                    return glib::Propagation::Stop;
                }
                if (key == gtk::gdk::Key::Left || key == gtk::gdk::Key::Right)
                    && media.is_seekable()
                    && media.duration() > 0
                {
                    let step = if key == gtk::gdk::Key::Left {
                        -5_000_000
                    } else {
                        5_000_000
                    };
                    media.seek((media.timestamp().saturating_add(step)).clamp(0, media.duration()));
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        }
    ));
    window.add_controller(keys);
    state.load();
    window.connect_close_request(move |_| {
        state.stop();
        glib::Propagation::Proceed
    });
    window.present();
    window
}

impl Viewer {
    fn show_video(&self, media: &gtk::MediaFile) -> bool {
        matches!(self.context.media, PlayableMedia::Video { .. }) && media.has_video()
    }

    fn current(&self, media: &gtk::MediaFile) -> bool {
        !self.closed.get() && self.media.borrow().as_ref() == Some(media)
    }

    fn connect_media(self: &Rc<Self>, media: &gtk::MediaFile) {
        let state = self.clone();
        media.connect_prepared_notify(glib::clone!(
            #[weak]
            state,
            move |media| {
                if !state.current(media) || !media.is_prepared() || media.error().is_some() {
                    return;
                }
                state
                    .visual
                    .set_visible_child_name(if state.show_video(media) {
                        "video"
                    } else {
                        "audio"
                    });
                state.stack.set_visible_child_name("player");
                state.controls.set_media_stream(Some(media));
                state.controls.set_visible(true);
                state.resize();
                state.focus_controls();
            }
        ));
        media.connect_error_notify(glib::clone!(
            #[weak]
            state,
            move |media| {
                if state.current(media)
                    && let Some(error) = media.error()
                {
                    state.show_error(&format!("Не удалось воспроизвести медиа. {error}"));
                }
            }
        ));
        media.connect_invalidate_size(glib::clone!(
            #[weak]
            state,
            move |media| {
                if state.current(media) && media.is_prepared() && media.error().is_none() {
                    state.resize();
                }
            }
        ));
    }

    fn load(self: &Rc<Self>) {
        if self.closed.get() || self.task.borrow().is_some() {
            return;
        }
        self.stack.set_visible_child_name("loading");
        self.controls.set_visible(false);
        self.status.set_text("Загрузка медиа…");
        self.progress.set_fraction(0.0);
        self.resize();
        let (progress, mut updates) = watch::channel(PlaybackProgress::default());
        let task = self.context.runtime.spawn((self.context.load)(progress));
        self.task.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            while updates.changed().await.is_ok() {
                let Some(state) = weak.upgrade() else {
                    break;
                };
                if state.closed.get() || state.task.borrow().is_none() {
                    break;
                }
                let progress = *updates.borrow_and_update();
                if progress.total > 0 {
                    state.progress.set_fraction(
                        (progress.received as f64 / progress.total as f64).clamp(0.0, 1.0),
                    );
                    state.status.set_text(&format!(
                        "Загрузка: {} / {}",
                        glib::format_size(progress.received),
                        glib::format_size(progress.total)
                    ));
                } else {
                    state.progress.pulse();
                }
            }
        });
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            if let Some(state) = weak.upgrade()
                && !state.closed.get()
            {
                state.task.take();
                match result {
                    Ok(file) => {
                        state.file.replace(Some(file));
                        state.start_playback();
                    }
                    Err(error) => state.show_error(&format!("Не удалось загрузить медиа. {error}")),
                }
            }
        });
    }

    fn start_playback(self: &Rc<Self>) {
        if self.closed.get() {
            return;
        }
        let Some(file) = self
            .file
            .borrow()
            .as_ref()
            .map(|file| (file.path(), file.media))
        else {
            return;
        };
        self.controls.set_media_stream(None);
        let media = gtk::MediaFile::new();
        if let Some(previous) = self.media.replace(Some(media.clone())) {
            previous.pause();
            previous.clear();
        }
        self.picture.set_paintable(Some(&media));
        self.connect_media(&media);
        self.controls.set_visible(false);
        self.stack.set_visible_child_name("loading");
        self.status.set_text("Подготовка медиа…");
        self.progress.set_fraction(1.0);
        self.resize();
        if let Some(window) = self.window.upgrade() {
            window.set_title(Some(file.1.title()));
        }
        media.set_filename(Some(file.0));
        media.set_muted(self.context.muted);
        media.play();
    }

    fn show_error(&self, error: &str) {
        self.controls.set_media_stream(None);
        if let Some(media) = self.media.borrow().as_ref() {
            media.pause();
        }
        self.error.set_text(error);
        self.stack.set_visible_child_name("error");
        self.controls.set_visible(false);
        self.resize();
        self.retry.grab_focus();
    }

    fn resize(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let playing = self
            .stack
            .visible_child_name()
            .is_some_and(|name| name == "player");
        let media = self.media.borrow().clone();
        let dimensions =
            if let Some(media) = media.filter(|media| playing && self.show_video(media)) {
                let width = media.intrinsic_width();
                let height = media.intrinsic_height();
                if width > 0 && height > 0 {
                    (width, height)
                } else {
                    self.file
                        .borrow()
                        .as_ref()
                        .map_or(self.context.media, |file| file.media)
                        .dimensions()
                }
            } else if playing {
                PlayableMedia::Audio.dimensions()
            } else {
                (360, 144)
            };
        let (header_width, _, _, _) = self.header.measure(gtk::Orientation::Horizontal, -1);
        let (header_height, _, _, _) = self.header.measure(gtk::Orientation::Vertical, -1);
        let (control_width, _, _, _) = self.controls.measure(gtk::Orientation::Horizontal, -1);
        let (control_height, _, _, _) = self.controls.measure(gtk::Orientation::Vertical, -1);
        let chrome = (
            header_width.max(control_width),
            header_height + control_height + 2,
        );
        let (width, height) = fit_viewport(dimensions, self.screen, chrome, frame_spacing(&window));
        // Wayland places a toplevel on map. Let GNOME centre the final size after
        // switching from the loading panel or learning the decoded dimensions.
        let remap = window.is_visible()
            && (window.default_width() != width.max(chrome.0)
                || window.default_height() != height + chrome.1);
        if remap {
            window.set_visible(false);
        }
        self.content.set_size_request(width, height);
        window.set_default_size(width.max(chrome.0), height + chrome.1);
        if remap {
            window.present();
            self.focus_controls();
        }
    }

    fn focus_controls(&self) {
        if !self.controls.is_visible() {
            return;
        }
        let focused = self
            .window
            .upgrade()
            .and_then(|window| gtk::prelude::RootExt::focus(&window));
        if !focused.is_some_and(|widget| widget.is_ancestor(&self.controls.widget)) {
            self.controls.child_focus(gtk::DirectionType::TabForward);
        }
    }

    fn stop(&self) {
        if self.closed.replace(true) {
            return;
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.controls.set_media_stream(None);
        if let Some(media) = self.media.take() {
            media.pause();
            media.clear();
        }
        self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        self.file.take();
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn frame_spacing(window: &adw::Window) -> (i32, i32) {
    let Some(content) = window.content() else {
        return (0, 0);
    };
    let spacing = |orientation| {
        (window.measure(orientation, -1).0 - content.measure(orientation, -1).0).max(0)
    };
    (
        spacing(gtk::Orientation::Horizontal),
        spacing(gtk::Orientation::Vertical),
    )
}

pub fn fit_viewport(
    (width, height): (i32, i32),
    (screen_width, screen_height): (i32, i32),
    (chrome_width, chrome_height): (i32, i32),
    (frame_width, frame_height): (i32, i32),
) -> (i32, i32) {
    let (width, height) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let (screen_width, screen_height) = (
        f64::from(screen_width.max(1)),
        f64::from(screen_height.max(1)),
    );
    let (frame_width, frame_height) = (
        f64::from(frame_width.max(0)),
        f64::from(frame_height.max(0)),
    );
    let chrome_width = f64::from(chrome_width.max(1)) + frame_width;
    let chrome_height = f64::from(chrome_height.max(0)) + frame_height;
    let area = screen_width * screen_height / 4.0;
    // Solve (width * scale + frame_width) * (height * scale + chrome_height) <= area.
    let remaining = (area - frame_width * chrome_height).max(0.0);
    let linear = width * chrome_height + height * frame_width;
    let scale = (2.0 * remaining
        / (linear.hypot((4.0 * width * height * remaining).sqrt()) + linear))
        .min((area / chrome_width - chrome_height) / height)
        .min((screen_width - frame_width) / width)
        .min((screen_height - chrome_height) / height)
        .clamp(0.0, 1.0);
    (
        (width * scale).floor().max(1.0) as i32,
        (height * scale).floor().max(1.0) as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::fit_viewport;

    #[test]
    fn frame_and_controls_fit_inside_a_quarter_screen_for_video_audio_and_portraits() {
        for screen in [(1280, 800), (800, 1280), (640, 480)] {
            for dimensions in [(1920, 1080), (1080, 1920), (360, 96), (15, 1600), (1600, 9)] {
                for chrome in [(52, 46), (188, 82), (260, 110)] {
                    for frame in [(0, 0), (10, 10), (24, 24)] {
                        let fitted = fit_viewport(dimensions, screen, chrome, frame);
                        let outer = (
                            (fitted.0.max(chrome.0) + frame.0),
                            (fitted.1 + chrome.1 + frame.1),
                        );
                        assert!(
                            4 * outer.0 * outer.1 <= screen.0 * screen.1,
                            "{dimensions:?} {chrome:?} {frame:?} -> {outer:?}"
                        );
                        assert!(outer.0 <= screen.0 && outer.1 <= screen.1);
                        assert!(
                            (fitted.0 * dimensions.1 - fitted.1 * dimensions.0).abs()
                                <= dimensions.0.max(dimensions.1)
                        );
                    }
                }
            }
        }
        assert_eq!(
            fit_viewport((0, -1), (1280, 800), (188, 82), (10, 10)),
            (1, 1)
        );
    }
}
