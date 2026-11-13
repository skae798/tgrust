use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

pub fn open_session_file(api_id: i32) -> Result<(PathBuf, File)> {
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("Укажите XDG_DATA_HOME или HOME")?;
    let directory = base.join("tgrust");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .context("Не удалось создать каталог сессии")?;
    let metadata = fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "Каталог сессии {} должен быть обычным каталогом с правами 0700",
        directory.display()
    );
    let path = directory.join(format!("{api_id}.session"));
    let file = lock_session_file(&path)?;
    Ok((path, file))
}

fn lock_session_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    let file = match options.clone().create_new(true).mode(0o600).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            ensure!(
                metadata.is_file() && metadata.nlink() == 1,
                "Файл сессии должен быть обычным файлом без ссылок"
            );
            let file = options.open(path)?;
            let opened = file.metadata()?;
            ensure!(
                metadata.dev() == opened.dev() && metadata.ino() == opened.ino(),
                "Файл сессии изменился во время открытия"
            );
            file
        }
        Err(error) => return Err(error).context("Не удалось открыть файл сессии"),
    };
    // On Linux flock is independent of SQLite's POSIX byte-range locks.
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!("Эта сессия уже открыта в другом экземпляре tgrust");
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(error).context("Не удалось заблокировать файл сессии");
        }
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

pub fn attachment_name(name: &str, fallback: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.chars()
                .filter(|ch| !ch.is_control() && !matches!(ch, '\\' | ':' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
                .take(60)
                .collect::<String>()
        })
        .filter(|name| !name.trim().is_empty() && name != "." && name != "..")
        .unwrap_or_else(|| fallback.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_names_stay_local_and_fit_linux_name_limit() {
        assert_eq!(attachment_name("../../photo.jpg", "file"), "photo.jpg");
        assert_eq!(attachment_name("..", "file"), "file");
        assert_eq!(attachment_name("\u{202e}\n", "file"), "file");
        assert!(attachment_name(&"💬".repeat(100), "file").len() <= 255);
    }

    #[test]
    fn session_file_is_private_and_can_be_reopened_after_release() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.session");
        let first = lock_session_file(&path).unwrap();
        assert_eq!(
            first.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(lock_session_file(&path).is_err());
        drop(first);
        fs::write(&path, b"existing session").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let reopened = lock_session_file(&path).unwrap();
        assert_eq!(
            reopened.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read(path).unwrap(), b"existing session");
    }

    #[test]
    fn session_file_rejects_symlinks_and_hard_links() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.session");
        let target = directory.path().join("other");
        fs::write(&target, b"other data").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(lock_session_file(&path).is_err());
        fs::remove_file(&path).unwrap();
        fs::hard_link(&target, &path).unwrap();
        assert!(lock_session_file(&path).is_err());
        assert_eq!(fs::read(target).unwrap(), b"other data");
    }

    #[test]
    fn sqlite_session_can_initialize_and_reopen_the_locked_file() {
        use grammers_session::{Session, storages::SqliteSession, types::UpdateState};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.session");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            {
                let _lock = lock_session_file(&path).unwrap();
                let session = SqliteSession::open(&path).await.unwrap();
                session
                    .set_update_state(UpdateState::Primary {
                        pts: 42,
                        date: 1,
                        seq: 1,
                    })
                    .await
                    .unwrap();
            }
            let _lock = lock_session_file(&path).unwrap();
            let session = SqliteSession::open(&path).await.unwrap();
            assert_eq!(session.updates_state().await.unwrap().pts, 42);
        });
    }
}
