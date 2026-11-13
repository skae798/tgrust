use std::env;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

fn session_directory() -> Result<PathBuf> {
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("Укажите XDG_DATA_HOME или HOME")?;
    Ok(base.join("tgrust"))
}

pub fn session_path(api_id: i32) -> Result<PathBuf> {
    let directory = session_directory()?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .context("Не удалось создать каталог сессии")?;
    check_session_directory(&directory)?;
    Ok(directory.join(format!("{api_id}.session")))
}

fn check_session_directory(directory: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "Каталог сессии {} должен быть обычным каталогом с правами 0700",
        directory.display()
    );
    Ok(())
}

pub fn saved_session_api_id() -> Result<Option<i32>> {
    find_saved_session(&session_directory()?)
}

fn find_saved_session(directory: &Path) -> Result<Option<i32>> {
    if let Err(error) = check_session_directory(directory) {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(None);
        }
        return Err(error);
    }

    let mut selected = None;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".session"))
            .and_then(|stem| {
                stem.parse::<i32>()
                    .ok()
                    .filter(|id| *id > 0 && stem == id.to_string())
            })
        else {
            continue;
        };
        if !entry.file_type()?.is_file() || entry.metadata()?.len() == 0 {
            continue;
        }
        if selected.replace(id).is_some() {
            bail!("Найдено несколько сессий. Укажите TGRUST_API_ID и TGRUST_API_HASH при запуске.");
        }
    }
    Ok(selected)
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
    fn launcher_selects_only_a_single_existing_session() -> Result<()> {
        let root = tempfile::tempdir()?;
        let directory = root.path().join("tgrust");
        assert_eq!(find_saved_session(&directory)?, None);
        DirBuilder::new().mode(0o700).create(&directory)?;
        fs::write(directory.join("123.session-wal"), b"sidecar")?;
        fs::write(directory.join("456.session"), [])?;
        std::os::unix::fs::symlink("123.session-wal", directory.join("789.session"))?;
        fs::write(directory.join("0123.session"), b"invalid name")?;
        assert_eq!(find_saved_session(&directory)?, None);
        fs::write(directory.join("123.session"), b"session")?;
        assert_eq!(find_saved_session(&directory)?, Some(123));
        fs::write(directory.join("321.session"), b"another session")?;
        assert!(find_saved_session(&directory).is_err());
        Ok(())
    }

    #[test]
    fn attachment_names_stay_local_and_fit_linux_name_limit() {
        assert_eq!(attachment_name("../../photo.jpg", "file"), "photo.jpg");
        assert_eq!(attachment_name("..", "file"), "file");
        assert_eq!(attachment_name("\u{202e}\n", "file"), "file");
        assert!(attachment_name(&"💬".repeat(100), "file").len() <= 255);
    }
}
