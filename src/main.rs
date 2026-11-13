mod demo;
mod model;
mod storage;
mod telegram;
mod ui;

use std::env;
use std::process::ExitCode;

use adw::prelude::*;
use anyhow::{Context, Result, ensure};

const APP_ID: &str = "io.github.skae798.tgrust";

fn credentials() -> Result<Option<(i32, Option<String>)>> {
    let id = env::var("TGRUST_API_ID").ok();
    let hash = env::var("TGRUST_API_HASH").ok();
    match (id, hash) {
        (None, None) => Ok(storage::saved_session_api_id()?.map(|id| (id, None))),
        (Some(id), Some(hash)) => {
            let id: i32 = id
                .parse()
                .context("TGRUST_API_ID должен быть целым числом")?;
            ensure!(id > 0, "TGRUST_API_ID должен быть положительным");
            ensure!(
                hash.len() == 32 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "TGRUST_API_HASH должен содержать 32 шестнадцатеричных символа"
            );
            Ok(Some((id, Some(hash))))
        }
        _ => anyhow::bail!("Укажите TGRUST_API_ID и TGRUST_API_HASH вместе"),
    }
}

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
    let runtime = tokio::runtime::Runtime::new().expect("Не удалось запустить Tokio");
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| gtk::Window::set_default_icon_name(APP_ID));
    app.connect_activate({
        let handle = runtime.handle().clone();
        move |app| {
            if let Some(window) = app.active_window() {
                window.present();
                return;
            }
            ui::open(app, handle.clone(), stop_rx.clone(), demo);
        }
    });
    let result = app.run_with_args(&["tgrust"]);
    stop.send_replace(true);
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    ExitCode::from(result)
}
