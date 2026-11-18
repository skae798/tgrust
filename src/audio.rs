use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use tokio::sync::watch;

use crate::media_viewer::PlaybackContext;
use crate::model::{AudioInfo, PlayableMedia, PlaybackFile, PlaybackProgress};
use crate::playback_controls::PlaybackControls;

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

pub fn content(
    media: PlayableMedia,
    info: &AudioInfo,
    filename: Option<&str>,
) -> (gtk::Box, gtk::Picture) {
    let voice = media == PlayableMedia::Voice;
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let icon = gtk::Image::from_icon_name(if voice {
        "audio-input-microphone-symbolic"
    } else {
        "audio-x-generic-symbolic"
    });
    icon.set_pixel_size(26);
    icon.set_size_request(42, 42);
    let artwork = gtk::Overlay::new();
    artwork.set_child(Some(&icon));
    artwork.set_valign(gtk::Align::Center);
    let cover = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .can_shrink(true)
        .build();
    artwork.add_overlay(&cover);
    artwork.set_clip_overlay(&cover, true);
    row.append(&artwork);

    let details = gtk::Box::new(gtk::Orientation::Vertical, 3);
    details.set_valign(gtk::Align::Center);
    details.set_hexpand(true);
    let title = if voice {
        media.title()
    } else {
        info.title.as_deref().or(filename).unwrap_or("Аудиозапись")
    };
    let title = line(title);
    title.add_css_class("heading");
    details.append(&title);
    if voice && !info.waveform.is_empty() {
        let bar = gtk::DrawingArea::new();
        bar.set_content_width(128);
        bar.set_content_height(16);
        bar.update_property(&[gtk::accessible::Property::Label("Звуковая волна")]);
        let waveform = info.waveform.clone();
        bar.set_draw_func(move |widget, context, width, height| {
            let color = widget.color();
            context.set_source_rgba(
                f64::from(color.red()),
                f64::from(color.green()),
                f64::from(color.blue()),
                0.78,
            );
            let step = f64::from(width) / waveform.len() as f64;
            for (index, &sample) in waveform.iter().enumerate() {
                let extent = 2.0 + f64::from(sample) / 31.0 * f64::from((height - 2).max(0));
                context.rectangle(
                    index as f64 * step,
                    (f64::from(height) - extent) / 2.0,
                    (step * 0.60).max(1.0),
                    extent,
                );
            }
            let _ = context.fill();
        });
        details.append(&bar);
    }
    let time = info.duration.map(|seconds| {
        if seconds < 3600 {
            format!("{}:{:02}", seconds / 60, seconds % 60)
        } else {
            format!(
                "{}:{:02}:{:02}",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            )
        }
    });
    let subtitle = match (info.performer.as_deref().filter(|_| !voice), time) {
        (Some(performer), Some(time)) => Some(format!("{performer} · {time}")),
        (Some(performer), None) => Some(performer.to_owned()),
        (None, time) => time,
    };
    if let Some(subtitle) = subtitle {
        let subtitle = line(&subtitle);
        subtitle.add_css_class("dim-label");
        details.append(&subtitle);
    }
    row.append(&details);
    (row, cover)
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
    pub widget: gtk::Box,
    context: PlaybackContext,
    status: gtk::Label,
    retry: gtk::Button,
    controls: Rc<PlaybackControls>,
    file: RefCell<Option<PlaybackFile>>,
    stream: RefCell<Option<gtk::MediaFile>>,
    task: RefCell<Option<tokio::task::AbortHandle>>,
}

impl InlinePlayer {
    pub fn open(context: PlaybackContext) -> Rc<Self> {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        status.set_max_width_chars(34);
        status.add_css_class("dim-label");
        widget.append(&status);
        let retry = gtk::Button::with_label("Повторить");
        retry.set_halign(gtk::Align::Start);
        widget.append(&retry);
        let controls = PlaybackControls::new();
        widget.append(&controls.widget);
        let player = Rc::new(Self {
            widget,
            context,
            status,
            retry,
            controls,
            file: RefCell::new(None),
            stream: RefCell::new(None),
            task: RefCell::new(None),
        });
        player.retry.connect_clicked(glib::clone!(
            #[weak]
            player,
            move |_| {
                if player.file.borrow().is_some() {
                    player.prepare();
                } else {
                    player.load();
                }
            }
        ));
        player.load();
        player
    }

    pub fn toggle(&self) {
        self.controls.toggle();
    }

    pub fn detach(&self) {
        if let Some(parent) = self.widget.parent().and_downcast::<gtk::Box>() {
            parent.remove(&self.widget);
        }
    }

    fn load(self: &Rc<Self>) {
        self.loading("Загрузка аудио…");
        let (progress, mut updates) = watch::channel(PlaybackProgress::default());
        let task = self.context.runtime.spawn((self.context.load)(progress));
        self.task.replace(Some(task.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            while updates.changed().await.is_ok() {
                let Some(player) = weak.upgrade() else { break };
                let progress = *updates.borrow_and_update();
                if progress.total > 0 && player.task.borrow().is_some() {
                    let percent = progress.received.saturating_mul(100) / progress.total;
                    player
                        .status
                        .set_text(&format!("Загрузка аудио… {}%", percent.min(100)));
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
                Ok(file) if file.media == player.context.media => {
                    player.file.replace(Some(file));
                    player.prepare();
                }
                Ok(_) => player.error("Аудио в сообщении изменилось. Обновите чат."),
                Err(error) => player.error(&format!("Не удалось загрузить аудио. {error}")),
            }
        });
    }

    fn prepare(self: &Rc<Self>) {
        let Some(path) = self.file.borrow().as_ref().map(PlaybackFile::path) else {
            return;
        };
        self.loading("Подготовка аудио…");
        self.controls.set_media_stream(None);
        let stream = gtk::MediaFile::new();
        if let Some(previous) = self.stream.replace(Some(stream.clone())) {
            previous.clear();
        }
        stream.connect_prepared_notify(glib::clone!(
            #[weak(rename_to = player)]
            self,
            move |stream| {
                if player.stream.borrow().as_ref() == Some(stream)
                    && stream.is_prepared()
                    && stream.error().is_none()
                {
                    player.status.set_visible(false);
                    player.controls.set_media_stream(Some(stream));
                    player.controls.set_visible(true);
                }
            }
        ));
        stream.connect_error_notify(glib::clone!(
            #[weak(rename_to = player)]
            self,
            move |stream| {
                if player.stream.borrow().as_ref() == Some(stream)
                    && let Some(error) = stream.error()
                {
                    player.error(&format!("Не удалось воспроизвести аудио. {error}"));
                }
            }
        ));
        stream.set_filename(Some(path));
        stream.set_muted(self.context.muted);
        stream.play();
    }

    fn loading(&self, status: &str) {
        self.retry.set_visible(false);
        self.controls.set_visible(false);
        self.status.set_text(status);
        self.status.set_visible(true);
    }

    fn error(&self, message: &str) {
        self.loading(message);
        self.retry.set_visible(true);
    }
}

impl Drop for InlinePlayer {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
        self.controls.set_media_stream(None);
        if let Some(stream) = self.stream.get_mut().take() {
            stream.pause();
            stream.clear();
        }
        self.detach();
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
