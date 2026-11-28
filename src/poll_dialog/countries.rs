use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

pub(super) fn name(code: &str) -> String {
    crate::countries::name(code).map_or_else(
        || code.to_owned(),
        |name| glib::dgettext(Some("iso_3166-1"), name).into(),
    )
}

pub(super) fn description(codes: &[String], limit: usize) -> Option<String> {
    if codes.is_empty() {
        return None;
    }
    let mut names: Vec<_> = codes.iter().take(limit).map(|code| name(code)).collect();
    if codes.len() > limit {
        names.push(format!("ещё {}", codes.len() - limit));
    }
    Some(format!(
        "Голосовать могут аккаунты с номерами из стран: {}",
        names.join(", ")
    ))
}

pub(super) struct Controls {
    row: adw::ExpanderRow,
    choices: Rc<Vec<(&'static str, gtk::CheckButton)>>,
}

impl Controls {
    pub(super) fn append(group: &adw::PreferencesGroup) -> Self {
        let row = adw::ExpanderRow::builder()
            .title("Ограничить участие по странам")
            .subtitle("Все страны")
            .show_enable_switch(true)
            .enable_expansion(false)
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        body.set_margin_start(8);
        body.set_margin_end(8);
        body.set_margin_bottom(8);
        let hint = super::label(
            "Страна определяется по номеру аккаунта Telegram. Выберите хотя бы одну страну.",
        );
        body.append(&hint);
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Найти страну")
            .build();
        search.update_property(&[gtk::accessible::Property::Label("Поиск страны")]);
        body.append(&search);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list.set_placeholder(Some(&super::label("Страны не найдены")));
        let mut countries: Vec<_> = crate::countries::all()
            .map(|(code, _)| (code, name(code)))
            .collect();
        countries.sort_by_cached_key(|(_, name)| name.to_lowercase());
        let mut choices = Vec::new();
        let mut filters = Vec::new();
        for (code, name) in countries {
            let check = gtk::CheckButton::new();
            check.set_child(Some(&super::label(&name)));
            check.update_property(&[gtk::accessible::Property::Label(&name)]);
            let item = gtk::ListBoxRow::new();
            item.set_child(Some(&check));
            list.append(&item);
            filters.push((format!("{name} {code}").to_lowercase(), item));
            choices.push((code, check));
        }
        search.connect_changed(move |search| {
            let query = search.text().trim().to_lowercase();
            for (text, item) in &filters {
                item.set_visible(text.contains(&query));
            }
        });
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .height_request(180)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        body.append(&scroll);
        row.add_row(&body);
        group.add(&row);
        let choices = Rc::new(choices);
        for (_, check) in choices.iter() {
            check.connect_toggled(glib::clone!(
                #[weak]
                row,
                #[weak]
                choices,
                move |_| {
                    row.set_subtitle(&format!(
                        "Выбрано стран: {}",
                        choices
                            .iter()
                            .filter(|(_, check)| check.is_active())
                            .count()
                    ));
                }
            ));
        }
        row.connect_enable_expansion_notify(glib::clone!(
            #[strong]
            choices,
            move |row| {
                row.set_expanded(row.enables_expansion());
                row.set_subtitle(&if row.enables_expansion() {
                    format!(
                        "Выбрано стран: {}",
                        choices
                            .iter()
                            .filter(|(_, check)| check.is_active())
                            .count()
                    )
                } else {
                    "Все страны".into()
                });
            }
        ));
        Self { row, choices }
    }

    pub(super) fn selection(&self) -> anyhow::Result<Vec<String>> {
        if !self.row.enables_expansion() {
            return Ok(Vec::new());
        }
        let codes: Vec<_> = self
            .choices
            .iter()
            .filter(|(_, check)| check.is_active())
            .map(|(code, _)| (*code).to_owned())
            .collect();
        anyhow::ensure!(!codes.is_empty(), "Выберите хотя бы одну страну");
        Ok(codes)
    }
}
