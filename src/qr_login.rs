use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use grammers_client::tl::enums;
use grammers_session::updates::UpdatesLike;
use gtk::{gdk, glib};
use tokio::sync::{mpsc, watch};

pub fn texture(token: &[u8]) -> Result<gdk::MemoryTexture> {
    use qrcodegen::{QrCode, QrCodeEcc};
    ensure!(!token.is_empty(), "Telegram вернул пустой код входа");
    let url = format!("tg://login?token={}", URL_SAFE_NO_PAD.encode(token));
    let code = QrCode::encode_text(&url, QrCodeEcc::Medium)
        .map_err(|_| anyhow::anyhow!("Не удалось создать QR-код"))?;
    const BORDER: i32 = 4;
    let cells = code.size() + BORDER * 2;
    let scale = (220 / cells).max(1);
    let width = cells * scale;
    let row_length = width as usize * 3;
    let mut pixels = Vec::with_capacity(width as usize * row_length);
    for y in 0..width {
        for x in 0..width {
            let value = if code.get_module(x / scale - BORDER, y / scale - BORDER) {
                0
            } else {
                255
            };
            pixels.extend_from_slice(&[value, value, value]);
        }
    }
    Ok(gdk::MemoryTexture::new(
        width,
        width,
        gdk::MemoryFormat::R8g8b8,
        &glib::Bytes::from_owned(pixels),
        row_length,
    ))
}

fn remaining(expires: i32, now: SystemTime) -> Duration {
    // Keep the presented token until its server expiry, including small clock skew.
    let expiry = UNIX_EPOCH + Duration::from_secs(expires.max(0) as u64);
    expiry
        .duration_since(now)
        .unwrap_or(Duration::ZERO)
        .clamp(Duration::from_secs(1), Duration::from_secs(60))
}

fn accepted(update: &UpdatesLike) -> bool {
    let is_login = |update: &enums::Update| matches!(update, enums::Update::LoginToken);
    match update {
        UpdatesLike::Updates(enums::Updates::UpdateShort(update)) => is_login(&update.update),
        UpdatesLike::Updates(enums::Updates::Updates(update)) => {
            update.updates.iter().any(is_login)
        }
        UpdatesLike::Updates(enums::Updates::Combined(update)) => {
            update.updates.iter().any(is_login)
        }
        UpdatesLike::ConnectionClosed => true,
        _ => false,
    }
}

pub async fn wait(
    updates: &mut mpsc::Receiver<UpdatesLike>,
    cancel: &mut watch::Receiver<bool>,
    expires: i32,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + remaining(expires, SystemTime::now());
    loop {
        tokio::select! {
            biased;
            _ = cancel.changed() => return Ok(false),
            _ = tokio::time::sleep_until(deadline) => return Ok(true),
            update = updates.recv() => {
                if accepted(&update.context("Соединение с Telegram завершилось")?) {
                    return Ok(true);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_client::tl::types;

    #[test]
    fn qr_expiration_obeys_server_timestamp_with_retry_bounds() {
        let now = UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(remaining(125, now), Duration::from_secs(25));
        assert_eq!(remaining(90, now), Duration::from_secs(1));
        assert_eq!(remaining(800, now), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn qr_wait_wakes_for_telegram_confirmation_and_cancels() {
        let (tx, mut updates) = mpsc::channel(2);
        let (cancel, mut receiver) = watch::channel(false);
        let accepted = types::UpdateShort {
            update: enums::Update::LoginToken,
            date: 0,
        };
        tx.send(UpdatesLike::Updates(accepted.into()))
            .await
            .unwrap();
        let deadline = i32::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 30,
        )
        .unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                wait(&mut updates, &mut receiver, deadline)
            )
            .await
            .unwrap()
            .unwrap()
        );

        cancel.send_replace(true);
        tx.send(UpdatesLike::ConnectionClosed).await.unwrap();
        assert!(!wait(&mut updates, &mut receiver, deadline).await.unwrap());
    }

    #[test]
    #[ignore = "requires a graphical display"]
    fn login_qr_builds_a_scan_sized_texture() {
        gtk::test_synced(|| {
            use gtk::prelude::*;
            let paint = texture(&[0, 1, 32, 250, 255]).unwrap();
            assert_eq!(paint.width(), paint.height());
            assert!((100..=220).contains(&paint.width()));
            assert!(texture(&[]).is_err());
        });
    }
}
