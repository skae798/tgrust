mod accounts;
mod blocked;
mod contacts;
mod demo;
mod devices;
mod formatting;
mod media_viewer;
mod model;
mod playback_controls;
mod privacy;
mod privacy_rules;
mod profile;
mod proxy;
mod proxy_dialog;
mod read_state;
mod search;
mod session_runner;
mod storage;
mod telegram;
mod ui;

use std::env;
use std::process::ExitCode;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "io.github.skae798.tgrust";

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    let demo = match args.as_slice() {
        [] => false,
        [flag] if flag == "--demo" => true,
        [flag] if flag == "--help" || flag == "-h" => {
            println!("Использование: tgrust [--demo]");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("Использование: tgrust [--demo]");
            return ExitCode::from(2);
        }
    };
    ExitCode::from(application(demo).run_with_args(&["tgrust"]))
}

fn application(demo: bool) -> adw::Application {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        gtk::Window::set_default_icon_name(APP_ID);
        if let Some(display) = gtk::gdk::Display::default() {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(include_str!("style.css"));
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    let accounts = accounts::Accounts::new(&app, demo);
    let target_type = <(String, i64)>::static_variant_type();
    let open_chat = gtk::gio::SimpleAction::new("open-account-chat", Some(target_type.as_ref()));
    open_chat.connect_activate({
        let accounts = accounts.clone();
        move |_, target| {
            if let Some((key, chat)) = target.and_then(glib::Variant::get::<(String, i64)>) {
                accounts.open_chat(&key, chat);
            }
        }
    });
    app.add_action(&open_chat);
    app.connect_activate(move |app| {
        if let Err(error) = accounts.activate() {
            eprintln!("{error:#}");
            app.quit();
        }
    });
    app
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a graphical display"]
    fn startup_registers_ui_styles() {
        gtk::test_synced(|| {
            let app = application(true);
            app.set_flags(gtk::gio::ApplicationFlags::NON_UNIQUE);
            app.register(None::<&gtk::gio::Cancellable>).unwrap();

            let window = gtk::ApplicationWindow::new(&app);
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            window.set_child(Some(&content));
            for (class, expected_padding) in
                [("chat-row", 20), ("message-bubble", 26), ("composer", 28)]
            {
                let widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                widget.add_css_class(class);
                content.append(&widget);
                assert_eq!(
                    widget.measure(gtk::Orientation::Horizontal, -1).0,
                    expected_padding,
                    "startup styles for {class}"
                );
            }
        });
    }
}
