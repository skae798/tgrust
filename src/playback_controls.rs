use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

/// Standard GTK controls with atomic volume/mute updates. Keeping those updates
/// together prevents a muted stream's notification from resetting the slider
/// before the requested volume has been applied.
pub struct PlaybackControls {
    pub widget: gtk::Box,
    play: gtk::Button,
    elapsed: gtk::Label,
    remaining: gtk::Label,
    position: gtk::Scale,
    volume: gtk::Scale,
    volume_button: gtk::MenuButton,
    stream: RefCell<Option<gtk::MediaFile>>,
    updating: Cell<bool>,
}

impl PlaybackControls {
    pub fn new() -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        widget.set_margin_top(4);
        widget.set_margin_bottom(4);
        let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
        play.add_css_class("flat");
        let elapsed = gtk::Label::new(Some("0:00"));
        let remaining = gtk::Label::new(Some("−0:00"));
        let position = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
        position.set_draw_value(false);
        position.set_hexpand(true);
        position.set_size_request(72, -1);
        position.update_property(&[gtk::accessible::Property::Label("Position")]);
        let volume = gtk::Scale::with_range(gtk::Orientation::Vertical, 0.0, 1.0, 0.05);
        volume.set_draw_value(false);
        volume.set_inverted(true);
        volume.set_size_request(32, 120);
        volume.set_margin_top(8);
        volume.set_margin_bottom(8);
        volume.update_property(&[gtk::accessible::Property::Label("Громкость")]);
        let popup = gtk::Popover::builder().child(&volume).build();
        let volume_button = gtk::MenuButton::builder()
            .icon_name("audio-volume-high-symbolic")
            .popover(&popup)
            .tooltip_text("Громкость")
            .build();
        volume_button.add_css_class("flat");
        widget.append(&play);
        widget.append(&elapsed);
        widget.append(&position);
        widget.append(&remaining);
        widget.append(&volume_button);
        let controls = Rc::new(Self {
            widget,
            play,
            elapsed,
            remaining,
            position,
            volume,
            volume_button,
            stream: RefCell::new(None),
            updating: Cell::new(false),
        });
        controls.play.connect_clicked(glib::clone!(
            #[weak]
            controls,
            move |_| controls.toggle()
        ));
        controls.position.connect_value_changed(glib::clone!(
            #[weak]
            controls,
            move |position| {
                if controls.updating.get() {
                    return;
                }
                let stream = controls.stream.borrow().clone();
                if let Some(stream) =
                    stream.filter(|stream| stream.is_seekable() && stream.duration() > 0)
                {
                    let target =
                        ((position.value() * 1_000_000.0) as i64).clamp(0, stream.duration());
                    if target.abs_diff(stream.timestamp()) > 1 {
                        stream.seek(target);
                    }
                }
            }
        ));
        controls.volume.connect_value_changed(glib::clone!(
            #[weak]
            controls,
            move |volume| {
                if controls.updating.get() {
                    return;
                }
                let stream = controls.stream.borrow().clone();
                if let Some(stream) = stream {
                    let volume = volume.value();
                    let _notifications = stream.freeze_notify();
                    stream.set_volume(volume);
                    stream.set_muted(volume == 0.0);
                }
            }
        ));
        controls.update();
        controls
    }

    pub fn toggle(&self) {
        let Some(stream) = self.stream.borrow().clone() else {
            return;
        };
        if stream.is_playing() {
            stream.pause();
        } else {
            if stream.is_ended() && stream.is_seekable() {
                stream.seek(0);
            }
            stream.play();
        }
    }

    pub fn set_media_stream(self: &Rc<Self>, stream: Option<&gtk::MediaFile>) {
        self.stream.replace(stream.cloned());
        if let Some(stream) = stream {
            stream.connect_notify_local(
                None,
                glib::clone!(
                    #[weak(rename_to = controls)]
                    self,
                    move |stream, _| {
                        if controls.stream.borrow().as_ref() == Some(stream) {
                            controls.update();
                        }
                    }
                ),
            );
        }
        self.update();
    }

    fn update(&self) {
        self.updating.set(true);
        let stream = self.stream.borrow().clone();
        self.widget.set_sensitive(stream.is_some());
        if let Some(stream) = stream {
            let timestamp = stream.timestamp().max(0);
            let duration = stream.duration().max(0);
            let playing = stream.is_playing();
            self.play.set_icon_name(if playing {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            });
            self.play.set_tooltip_text(Some(if playing {
                "Пауза"
            } else {
                "Воспроизвести"
            }));
            self.elapsed.set_text(&time_label(timestamp));
            self.remaining
                .set_text(&format!("−{}", time_label((duration - timestamp).max(0))));
            self.position
                .set_range(0.0, (duration as f64 / 1_000_000.0).max(1.0));
            if !stream.is_seeking() {
                self.position.set_value(timestamp as f64 / 1_000_000.0);
            }
            self.position
                .set_sensitive(stream.is_seekable() && duration > 0);
            self.volume_button.set_sensitive(stream.has_audio());
            let volume = if stream.is_muted() {
                0.0
            } else {
                stream.volume()
            };
            self.volume.set_value(volume);
            self.volume_button.set_icon_name(if volume == 0.0 {
                "audio-volume-muted-symbolic"
            } else if volume < 0.33 {
                "audio-volume-low-symbolic"
            } else if volume < 0.67 {
                "audio-volume-medium-symbolic"
            } else {
                "audio-volume-high-symbolic"
            });
        }
        self.updating.set(false);
    }
}

impl std::ops::Deref for PlaybackControls {
    type Target = gtk::Box;

    fn deref(&self) -> &Self::Target {
        &self.widget
    }
}

fn time_label(microseconds: i64) -> String {
    let seconds = microseconds / 1_000_000;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}
