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
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| gtk::Window::set_default_icon_name(APP_ID));
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
    ExitCode::from(app.run_with_args(&["tgrust"]))
}
