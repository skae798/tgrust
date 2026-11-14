use adw::prelude::*;
use gtk::{gdk, gio, glib, pango};

use crate::model::{FormattedText, TextSpan, TextStyle};

const STYLES: [(TextStyle, &str, &str, gdk::Key); 5] = [
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
        let modifiers = if style == TextStyle::Strike {
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

pub fn message_attributes(spans: &[TextSpan]) -> pango::AttrList {
    let attributes = pango::AttrList::new();
    for span in spans {
        let mut attribute: pango::Attribute = match span.style {
            TextStyle::Bold => pango::AttrInt::new_weight(pango::Weight::Bold).into(),
            TextStyle::Italic => pango::AttrInt::new_style(pango::Style::Italic).into(),
            TextStyle::Underline => pango::AttrInt::new_underline(pango::Underline::Single).into(),
            TextStyle::Strike => pango::AttrInt::new_strikethrough(true).into(),
            TextStyle::Code => pango::AttrString::new_family("monospace").into(),
        };
        attribute.set_start_index(span.range.start as u32);
        attribute.set_end_index(span.range.end as u32);
        attributes.insert(attribute);
    }
    attributes
}
