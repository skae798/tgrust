use adw::prelude::*;

use crate::model::{AudioInfo, PlayableMedia};

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
