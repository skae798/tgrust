use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};

use crate::model::{
    FormattedText, SPOILER_PLACEHOLDER, TextLink, TextSpan, TextStyle, spoiler_ranges,
};

const STYLES: [(TextStyle, &str, &str, gdk::Key); 6] = [
    (TextStyle::Bold, "bold", "Полужирный", gdk::Key::b),
    (TextStyle::Italic, "italic", "Курсив", gdk::Key::i),
    (
        TextStyle::Underline,
        "underline",
        "Подчёркнутый",
        gdk::Key::u,
    ),
    (TextStyle::Strike, "strike", "Зачёркнутый", gdk::Key::x),
    (TextStyle::Code, "code", "Моноширинный", gdk::Key::m),
    (TextStyle::Spoiler, "spoiler", "Спойлер", gdk::Key::p),
];

pub fn setup_editor(editor: &gtk::TextView) {
    let buffer = editor.buffer();
    let actions = gio::SimpleActionGroup::new();
    let menu = gio::Menu::new();
    let shortcuts = gtk::ShortcutController::new();
    shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);
    for (style, name, label, key) in STYLES {
        let builder = gtk::TextTag::builder().name(name);
        let tag = match style {
            TextStyle::Bold => builder.weight(700),
            TextStyle::Italic => builder.style(pango::Style::Italic),
            TextStyle::Underline => builder.underline(pango::Underline::Single),
            TextStyle::Strike => builder.strikethrough(true),
            TextStyle::Code => builder.family("monospace"),
            TextStyle::Spoiler => builder.underline(pango::Underline::Low),
        }
        .build();
        buffer.tag_table().add(&tag);
        let action = gio::SimpleAction::new(name, None);
        action.set_enabled(false);
        action.connect_activate(glib::clone!(
            #[weak]
            buffer,
            move |_, _| {
                let Some((start, end)) = buffer.selection_bounds() else {
                    return;
                };
                let mut next_toggle = start;
                if start.has_tag(&tag)
                    && (!next_toggle.forward_to_tag_toggle(Some(&tag)) || next_toggle >= end)
                {
                    buffer.remove_tag(&tag, &start, &end);
                } else {
                    for (other, name, _, _) in STYLES {
                        if (style == TextStyle::Code) != (other == TextStyle::Code) {
                            buffer.remove_tag_by_name(name, &start, &end);
                        }
                    }
                    buffer.apply_tag(&tag, &start, &end);
                }
            }
        ));
        buffer.connect_has_selection_notify(glib::clone!(
            #[weak]
            action,
            move |buffer| action.set_enabled(buffer.has_selection())
        ));
        actions.add_action(&action);
        let detailed_name = format!("format.{name}");
        menu.append(Some(label), Some(&detailed_name));
        let modifiers = if matches!(style, TextStyle::Strike | TextStyle::Spoiler) {
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK
        } else {
            gdk::ModifierType::CONTROL_MASK
        };
        shortcuts.add_shortcut(gtk::Shortcut::new(
            Some(gtk::KeyvalTrigger::new(key, modifiers)),
            Some(gtk::NamedAction::new(&detailed_name)),
        ));
    }

    let extra = gio::Menu::new();
    extra.append_submenu(Some("Форматирование"), &menu);
    editor.set_extra_menu(Some(&extra));
    editor.insert_action_group("format", Some(&actions));
    editor.add_controller(shortcuts);
}

pub fn editor_text(buffer: &gtk::TextBuffer) -> FormattedText {
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
    let boundaries = text
        .char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let mut spans = Vec::new();
    for (style, name, _, _) in STYLES {
        let Some(tag) = buffer.tag_table().lookup(name) else {
            continue;
        };
        let mut iter = buffer.start_iter();
        while !iter.is_end() {
            let start = iter.offset() as usize;
            let active = iter.has_tag(&tag);
            iter.forward_to_tag_toggle(Some(&tag));
            if active {
                spans.push(TextSpan {
                    range: boundaries[start]..boundaries[iter.offset() as usize],
                    style,
                });
            }
        }
    }
    spans.sort_by_key(|span| (span.range.start, span.range.end, span.style));
    FormattedText {
        text: text.into(),
        spans,
    }
}

pub fn set_editor_text(buffer: &gtk::TextBuffer, formatted: &FormattedText) {
    buffer.set_text(&formatted.text);
    for span in &formatted.spans {
        let Some((_, name, _, _)) = STYLES.iter().find(|(style, _, _, _)| *style == span.style)
        else {
            continue;
        };
        let start = formatted.text[..span.range.start].chars().count() as i32;
        let end = start + formatted.text[span.range.clone()].chars().count() as i32;
        buffer.apply_tag_by_name(
            name,
            &buffer.iter_at_offset(start),
            &buffer.iter_at_offset(end),
        );
    }
}

fn message_attributes(spans: &[TextSpan]) -> pango::AttrList {
    let attributes = pango::AttrList::new();
    for span in spans {
        let mut attribute: pango::Attribute = match span.style {
            TextStyle::Bold => pango::AttrInt::new_weight(pango::Weight::Bold).into(),
            TextStyle::Italic => pango::AttrInt::new_style(pango::Style::Italic).into(),
            TextStyle::Underline => pango::AttrInt::new_underline(pango::Underline::Single).into(),
            TextStyle::Strike => pango::AttrInt::new_strikethrough(true).into(),
            TextStyle::Code => pango::AttrString::new_family("monospace").into(),
            TextStyle::Spoiler => continue,
        };
        attribute.set_start_index(span.range.start as u32);
        attribute.set_end_index(span.range.end as u32);
        attributes.insert(attribute);
    }
    attributes
}

pub const SPOILER_LINK: &str = "tgrust-spoiler";

pub fn render_message(
    label: &gtk::Label,
    message: &FormattedText,
    explicit: &[TextLink],
    concealed: bool,
) {
    let (markup, spans) = message_markup(message, explicit, concealed);
    label.set_markup(&markup);
    label.set_attributes(Some(&message_attributes(&spans)));
}

fn message_markup(
    message: &FormattedText,
    explicit: &[TextLink],
    concealed: bool,
) -> (String, Vec<TextSpan>) {
    let overlaps =
        |a: &std::ops::Range<usize>, b: &std::ops::Range<usize>| a.start < b.end && b.start < a.end;
    let spoilers = if concealed {
        spoiler_ranges(&message.spans)
    } else {
        Vec::new()
    };
    let mut links = explicit.to_vec();
    links.extend(
        crate::links::in_text(&message.text)
            .into_iter()
            .filter(|link| {
                !explicit
                    .iter()
                    .any(|item| overlaps(&item.range, &link.range))
                    && !message.spans.iter().any(|style| {
                        style.style == TextStyle::Code && overlaps(&style.range, &link.range)
                    })
            }),
    );
    let mut segments: Vec<_> = links
        .iter()
        .filter(|link| {
            !spoilers.iter().any(|span| overlaps(span, &link.range))
                && message
                    .text
                    .get(link.range.clone())
                    .is_some_and(|text| !text.is_empty())
        })
        .map(|link| (link.range.clone(), Some(link.uri.as_str())))
        .collect();
    segments.extend(spoilers.into_iter().map(|range| (range, None)));
    segments.sort_by_key(|(range, _)| (range.start, range.end));
    let mut markup = String::new();
    let mut spans = Vec::new();
    let mut original = 0;
    let mut visible = 0;
    let mut add_spans = |from, to, offset| {
        spans.extend(message.spans.iter().filter_map(|span| {
            let start = span.range.start.max(from);
            let end = span.range.end.min(to);
            (start < end).then(|| TextSpan {
                range: offset + start - from..offset + end - from,
                style: span.style,
            })
        }));
    };
    for (range, link) in segments {
        if range.start < original {
            continue;
        }
        markup.push_str(&glib::markup_escape_text(
            &message.text[original..range.start],
        ));
        add_spans(original, range.start, visible);
        visible += range.start - original;
        if let Some(link) = link {
            let target = glib::markup_escape_text(link);
            // GTK parses the decoded title again when showing the tooltip.
            let title = glib::markup_escape_text(&target);
            markup.push_str(&format!("<a href=\"{target}\" title=\"{title}\">"));
            markup.push_str(&glib::markup_escape_text(&message.text[range.clone()]));
            markup.push_str("</a>");
            add_spans(range.start, range.end, visible);
            visible += range.len();
        } else {
            markup.push_str(&format!(
                "<a href=\"{SPOILER_LINK}\" title=\"Показать спойлер\">"
            ));
            markup.push_str(SPOILER_PLACEHOLDER);
            markup.push_str("</a>");
            visible += SPOILER_PLACEHOLDER.len();
        }
        original = range.end;
    }
    markup.push_str(&glib::markup_escape_text(&message.text[original..]));
    add_spans(original, message.text.len(), visible);
    (markup, spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoilers_hide_link_targets_and_render_other_links() {
        let text = "😀 скрыто и https://example.org/?a=1&b=2";
        let start = text.find("скрыто").unwrap();
        let end = start + "скрыто".len();
        let message = FormattedText {
            text: text.into(),
            spans: vec![TextSpan {
                range: start..end,
                style: TextStyle::Spoiler,
            }],
        };
        let links = [TextLink {
            range: start..end,
            uri: "https://example.org/private".into(),
        }];
        let (concealed, _) = message_markup(&message, &links, true);
        assert!(concealed.contains(SPOILER_PLACEHOLDER));
        assert!(!concealed.contains("private"));
        assert!(!concealed.contains("скрыто"));
        assert!(concealed.contains(concat!(
            "<a href=\"https://example.org/?a=1&amp;b=2\" ",
            "title=\"https://example.org/?a=1&amp;amp;b=2\">",
            "https://example.org/?a=1&amp;b=2</a>"
        )));
        let (revealed, _) = message_markup(&message, &links, false);
        assert!(revealed.contains("https://example.org/private"));
        assert!(revealed.contains("скрыто"));
    }
}
