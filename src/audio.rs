use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tokio::sync::watch;

use crate::media_viewer::PlaybackContext;
use crate::model::{AudioInfo, PlayableMedia, PlaybackProgress, PlaybackSource};
use crate::playback_controls::time_label;

pub fn waveform(input: &[u8]) -> Vec<u8> {
    let input = &input[..input.len().min(256)];
    let samples: Vec<_> = (0..input.len() * 8 / 5)
        .map(|index| {
            let bit = index * 5;
            let byte = bit / 8;
            let word = u16::from(input[byte])
                | (u16::from(input.get(byte + 1).copied().unwrap_or_default()) << 8);
            ((word >> (bit % 8)) & 31) as u8
        })
        .collect();
    if samples.len() <= 48 {
        return samples;
    }
    (0..48)
        .map(|index| {
            let start = index * samples.len() / 48;
            let end = (index + 1) * samples.len() / 48;
            samples[start..end]
                .iter()
                .copied()
                .max()
                .unwrap_or_default()
        })
        .collect()
}

#[derive(Clone)]
pub struct AudioCard {
    button: gtk::Button,
    icon: gtk::Image,
    spinner: gtk::Spinner,
    position: gtk::Scale,
    subtitle: gtk::Label,
    updating: Rc<Cell<bool>>,
    media: PlayableMedia,
    performer: Option<String>,
    duration: Option<i64>,
}

impl AudioCard {
    pub fn new(
        media: PlayableMedia,
        info: &AudioInfo,
        filename: Option<&str>,
        has_cover: bool,
    ) -> (Self, gtk::Box, gtk::Picture) {
        let voice = media == PlayableMedia::Voice;
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("audio-attachment");
        let icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
        icon.set_pixel_size(18);
        let spinner = gtk::Spinner::new();
        let control = gtk::Stack::new();
        control.add_child(&icon);
        control.add_child(&spinner);
        let button = gtk::Button::builder()
            .child(&control)
            .valign(gtk::Align::Center)
            .build();
        button.set_size_request(38, 38);
        button.add_css_class("circular");
        button.add_css_class("suggested-action");
        row.append(&button);

        let cover = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .can_shrink(true)
            .build();
        if has_cover {
            let placeholder = gtk::Image::from_icon_name("audio-x-generic-symbolic");
            placeholder.set_pixel_size(22);
            placeholder.set_size_request(38, 38);
            let artwork = gtk::Overlay::new();
            artwork.set_child(Some(&placeholder));
            artwork.add_overlay(&cover);
            artwork.set_clip_overlay(&cover, true);
            artwork.set_valign(gtk::Align::Center);
            row.append(&artwork);
        }

        let details = gtk::Box::new(gtk::Orientation::Vertical, 1);
        details.set_valign(gtk::Align::Center);
        details.set_hexpand(true);
        if !voice {
            let title = line(info.title.as_deref().or(filename).unwrap_or("Аудиозапись"));
            title.add_css_class("heading");
            details.append(&title);
        }
        let position = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        position.set_draw_value(false);
        position.set_hexpand(true);
        position.set_size_request(150, -1);
        position.add_css_class("audio-position");
        position.update_property(&[gtk::accessible::Property::Label("Позиция воспроизведения")]);
        if voice && !info.waveform.is_empty() {
            let bar = waveform_widget(&info.waveform, &position);
            let seek = gtk::Overlay::new();
            seek.set_child(Some(&position));
            seek.add_overlay(&bar);
            position.add_css_class("waveform-position");
            details.append(&seek);
        } else {
            details.append(&position);
        }
        let subtitle = line("");
        subtitle.set_max_width_chars(1);
        subtitle.add_css_class("dim-label");
        details.append(&subtitle);
        row.append(&details);

        let card = Self {
            button,
            icon,
            spinner,
            position,
            subtitle,
            updating: Rc::new(Cell::new(false)),
            media,
            performer: info.performer.clone().filter(|_| !voice),
            duration: info.duration.map(|seconds| i64::from(seconds) * 1_000_000),
        };
        card.update(None, None, false);
        (card, row, cover)
    }

    pub fn connect_play(&self, action: impl Fn() + 'static) {
        self.button.connect_clicked(move |_| action());
    }

    pub fn connect_seek(&self, action: impl Fn(f64) + 'static) {
        let updating = self.updating.clone();
        self.position.connect_value_changed(move |position| {
            if !updating.get() {
                action(position.value());
            }
        });
    }

    fn update(&self, stream: Option<&gtk::MediaFile>, loading: Option<&str>, failed: bool) {
        let playing = stream.is_some_and(gtk::prelude::MediaStreamExt::is_playing);
        let hint = if let Some(loading) = loading {
            loading
        } else if failed {
            "Повторить воспроизведение"
        } else if playing {
            match self.media {
                PlayableMedia::Voice => "Пауза голосового сообщения",
                _ => "Пауза аудио",
            }
        } else {
            self.media.open_label()
        };
        if self.button.tooltip_text().as_deref() != Some(hint) {
            self.button
                .update_property(&[gtk::accessible::Property::Label(hint)]);
            self.button.set_tooltip_text(Some(hint));
            self.icon.set_icon_name(Some(if playing {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            }));
            self.spinner.set_spinning(loading.is_some());
            if let Some(control) = self.button.child().and_downcast::<gtk::Stack>() {
                if loading.is_some() {
                    control.set_visible_child(&self.spinner);
                } else {
                    control.set_visible_child(&self.icon);
                }
            }
        }
        self.updating.set(true);
        let elapsed = stream.map_or(0, |stream| stream.timestamp().max(0));
        let duration = stream
            .map(|stream| stream.duration())
            .filter(|duration| *duration > 0)
            .or(self.duration);
        let can_seek = stream.is_some_and(|stream| stream.is_seekable() && stream.duration() > 0);
        self.position.set_sensitive(can_seek);
        if !stream.is_some_and(gtk::prelude::MediaStreamExt::is_seeking) {
            self.position.set_value(
                duration
                    .filter(|duration| *duration > 0)
                    .map_or(0.0, |duration| elapsed as f64 / duration as f64)
                    .clamp(0.0, 1.0),
            );
        }
        self.updating.set(false);
        let time = duration.map(|duration| {
            if stream.is_some() {
                format!("{} / {}", time_label(elapsed), time_label(duration))
            } else {
                time_label(duration)
            }
        });
        let subtitle = match (&self.performer, time) {
            (Some(performer), Some(time)) => format!("{performer} · {time}"),
            (Some(performer), None) => performer.clone(),
            (None, time) => time.unwrap_or_else(|| " ".to_owned()),
        };
        if self.subtitle.text() != subtitle {
            self.subtitle.set_text(&subtitle);
        }
    }
}

fn waveform_widget(samples: &[u8], position: &gtk::Scale) -> gtk::DrawingArea {
    let bar = gtk::DrawingArea::new();
    bar.set_content_height(16);
    bar.set_valign(gtk::Align::Center);
    bar.set_can_target(false);
    let samples = samples.to_vec();
    bar.set_draw_func(glib::clone!(
        #[weak]
        position,
        move |widget, context, width, height| {
            let color = widget.color();
            let step = f64::from(width) / samples.len() as f64;
            for (index, &sample) in samples.iter().enumerate() {
                let elapsed = (index as f64 + 0.5) / samples.len() as f64 <= position.value();
                context.set_source_rgba(
                    f64::from(color.red()),
                    f64::from(color.green()),
                    f64::from(color.blue()),
                    if elapsed { 1.0 } else { 0.36 },
                );
                let extent = 2.0 + f64::from(sample) / 31.0 * f64::from((height - 2).max(0));
                context.rectangle(
                    index as f64 * step,
                    (f64::from(height) - extent) / 2.0,
                    (step * 0.60).max(1.0),
                    extent,
                );
                let _ = context.fill();
            }
        }
    ));
    position.connect_value_changed(glib::clone!(
        #[weak]
        bar,
        move |_| bar.queue_draw()
    ));
    bar
}

fn line(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .single_line_mode(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(34)
        .build()
}

pub struct InlinePlayer {
    context: PlaybackContext,
    card: RefCell<AudioCard>,
    source: RefCell<Option<PlaybackSource>>,
    stream: RefCell<Option<gtk::MediaFile>>,
    task: RefCell<Option<tokio::task::AbortHandle>>,
    loading: RefCell<Option<String>>,
    failed: Cell<bool>,
    toasts: adw::ToastOverlay,
}

impl InlinePlayer {
    pub fn open(context: PlaybackContext, card: AudioCard, toasts: adw::ToastOverlay) -> Rc<Self> {
        let player = Rc::new(Self {
            context,
            card: RefCell::new(card),
            source: RefCell::new(None),
            stream: RefCell::new(None),
            task: RefCell::new(None),
            loading: RefCell::new(None),
            failed: Cell::new(false),
            toasts,
        });
        player.load(false);
        player
    }

    pub fn attach(&self, card: AudioCard) {
        self.card.replace(card);
        self.update();
    }

    pub fn is_loading(&self) -> bool {
        self.loading.borrow().is_some()
    }

    pub fn toggle(self: &Rc<Self>) {
        if self.failed.get() {
            let streaming = matches!(
                self.source.borrow().as_ref(),
                Some(PlaybackSource::Stream(..))
            );
            if streaming {
                self.source.take();
                self.load(true);
            } else if self.source.borrow().is_some() {
                self.prepare();
            } else {
                self.load(false);
            }
        } else if let Some(stream) = self.stream.borrow().clone() {
            if stream.is_playing() {
                stream.pause();
            } else {
                if stream.is_ended() && stream.is_seekable() {
                    stream.seek(0);
                }
                stream.play();
            }
        }
    }

    pub fn seek(&self, fraction: f64) {
        let stream = self.stream.borrow().clone();
        if let Some(stream) = stream.filter(|stream| stream.is_seekable() && stream.duration() > 0)
        {
            let target = (fraction * stream.duration() as f64) as i64;
            if target.abs_diff(stream.timestamp()) > 1 {
                stream.seek(target);
            }
        }
    }

    fn load(self: &Rc<Self>, whole: bool) {
        if let Some(media) = self.stream.take() {
            media.clear();
        }
        self.loading("Загрузка аудио… Нажмите для отмены");
        let (progress, mut updates) = watch::channel(PlaybackProgress::default());
        let task = self
            .context
            .runtime
            .spawn((self.context.load)(progress, whole));
        self.task.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            while updates.changed().await.is_ok() {
                let Some(player) = weak.upgrade() else { break };
                let progress = *updates.borrow_and_update();
                if progress.total > 0 && player.task.borrow().is_some() {
                    let percent = progress.received.saturating_mul(100) / progress.total;
                    player.loading(&format!(
                        "Загрузка аудио… {}%. Нажмите для отмены",
                        percent.min(100)
                    ));
                }
            }
        });
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let Some(player) = weak.upgrade() else { return };
            player.task.take();
            match result {
                Ok(file) if file.media() == player.context.media => {
                    player.source.replace(Some(file));
                    player.prepare();
                }
                Ok(_) => player.error("Аудио в сообщении изменилось. Обновите чат."),
                Err(error) => player.error(&format!("Не удалось загрузить аудио. {error}")),
            }
        });
    }

    fn prepare(self: &Rc<Self>) {
        let Some(file) = self
            .source
            .borrow()
            .as_ref()
            .map(|source| crate::media_viewer::media_input(source, &self.context.runtime))
        else {
            return;
        };
        self.loading("Подготовка аудио… Нажмите для отмены");
        let stream = gtk::MediaFile::new();
        if let Some(previous) = self.stream.replace(Some(stream.clone())) {
            previous.clear();
        }
        stream.connect_notify_local(
            None,
            glib::clone!(
                #[weak(rename_to = player)]
                self,
                move |stream, property| {
                    if player.stream.borrow().as_ref() != Some(stream) {
                        return;
                    }
                    if property.name() == "error"
                        && let Some(error) = stream.error()
                    {
                        player.error(&format!("Не удалось воспроизвести аудио. {error}"));
                    } else if stream.is_prepared() && !player.failed.get() {
                        player.loading.take();
                        player.update();
                    }
                }
            ),
        );
        file.bind(&stream);
        stream.set_muted(self.context.muted);
        stream.play();
    }

    fn loading(&self, status: &str) {
        self.failed.set(false);
        self.loading.replace(Some(status.to_owned()));
        self.update();
    }

    fn error(&self, message: &str) {
        self.loading.take();
        self.failed.set(true);
        self.update();
        self.toasts.add_toast(adw::Toast::new(message));
    }

    fn update(&self) {
        let stream = self.stream.borrow();
        let loading = self.loading.borrow();
        let failed = self.failed.get();
        self.card.borrow().update(
            stream
                .as_ref()
                .filter(|stream| stream.is_prepared() && !failed && loading.is_none()),
            loading.as_deref(),
            failed,
        );
    }
}

impl Drop for InlinePlayer {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
        if let Some(PlaybackSource::Stream(_, stream)) = self.source.get_mut().as_ref() {
            stream.cancel();
        }
        if let Some(stream) = self.stream.get_mut().take() {
            stream.pause();
            stream.clear();
        }
        self.card.get_mut().update(None, None, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_decodes_cross_byte_samples_and_bounds_large_inputs() {
        assert_eq!(waveform(&[0xc1, 0xff, 0x07]), [1, 30, 31, 15]);
        assert!(waveform(&[]).is_empty());
        assert_eq!(waveform(&[255; 500]), [31; 48]);
    }
}
