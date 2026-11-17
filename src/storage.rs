use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// Preview data has no directory entry, including after a crash. It is never a saved attachment.
pub struct PlaybackDownload {
    file: File,
    writer: tokio::fs::File,
    received: u64,
    expected: u64,
}

impl PlaybackDownload {
    pub fn new(expected: u64) -> Result<Self> {
        let base = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .context("Укажите XDG_CACHE_HOME или HOME")?;
        Self::new_in(expected, &base.join("tgrust/preview"))
    }

    fn new_in(expected: u64, directory: &Path) -> Result<Self> {
        ensure!(
            (1..=4 * 1024_u64.pow(3)).contains(&expected),
            "Размер медиа должен быть от 1 байта до 4 ГиБ"
        );
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        let metadata = fs::symlink_metadata(directory)?;
        ensure!(
            metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
            "Каталог предпросмотра должен быть обычным каталогом с правами 0700"
        );
        let file = tempfile::tempfile_in(directory)
            .context("Не удалось создать временное хранилище медиа")?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let writer = tokio::fs::File::from_std(file.try_clone()?);
        Ok(Self {
            file,
            writer,
            received: 0,
            expected,
        })
    }

    pub async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let received = self
            .received
            .checked_add(bytes.len() as u64)
            .context("Размер медиа изменился")?;
        ensure!(received <= self.expected, "Медиа больше указанного размера");
        self.writer.write_all(bytes).await?;
        self.received = received;
        Ok(())
    }

    pub async fn restart(&mut self) -> Result<()> {
        self.writer.flush().await?;
        self.writer.set_len(0).await?;
        self.writer.rewind().await?;
        self.received = 0;
        Ok(())
    }

    pub async fn finish(
        mut self,
        media: crate::model::PlayableMedia,
    ) -> Result<crate::model::PlaybackFile> {
        ensure!(
            self.received == self.expected,
            "Медиа загружено не полностью"
        );
        self.writer.flush().await?;
        drop(self.writer);
        Ok(crate::model::PlaybackFile {
            file: self.file,
            media,
        })
    }
}

fn session_directory() -> Result<PathBuf> {
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("Укажите XDG_DATA_HOME или HOME")?;
    Ok(base.join("tgrust"))
}

pub fn open_session_file(api_id: i32, create: bool) -> Result<(PathBuf, File)> {
    let directory = session_directory()?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .context("Не удалось создать каталог сессии")?;
    check_session_directory(&directory)?;
    let path = directory.join(format!("{api_id}.session"));
    let file = lock_session_file(&path, create)?;
    Ok((path, file))
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

fn lock_session_file(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    let created = create.then(|| options.clone().create_new(true).mode(0o600).open(path));
    let file = match created {
        Some(Ok(file)) => file,
        Some(Err(error)) if error.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(error).context("Не удалось открыть файл сессии");
        }
        _ => {
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

pub struct PendingSave {
    output: tokio::fs::File,
    temporary: tempfile::NamedTempFile,
    destination: PathBuf,
}

impl PendingSave {
    pub fn new(destination: PathBuf) -> Result<Self> {
        match fs::symlink_metadata(&destination) {
            Ok(_) => anyhow::bail!("Файл с таким именем уже существует"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Не удалось проверить путь сохранения"),
        }
        let directory = destination.parent().context("У файла нет каталога")?;
        let temporary = tempfile::Builder::new()
            .prefix(".tgrust-")
            .tempfile_in(directory)?;
        let output = tokio::fs::File::from_std(temporary.reopen()?);
        Ok(Self {
            output,
            temporary,
            destination,
        })
    }

    pub async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.output.write_all(bytes).await?;
        Ok(())
    }

    pub async fn restart(&mut self) -> Result<()> {
        self.output.flush().await?;
        self.output.set_len(0).await?;
        self.output.rewind().await?;
        Ok(())
    }

    pub async fn finish(mut self, expected: Option<usize>) -> Result<()> {
        self.output.flush().await?;
        if let Some(expected) = expected {
            ensure!(
                self.output.metadata().await?.len() == expected as u64,
                "Вложение загружено не полностью"
            );
        }
        self.output.sync_all().await?;
        drop(self.output);
        self.temporary.persist_noclobber(self.destination)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_storage_is_anonymous_private_bounded_and_can_restart() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("preview");
            let mut output = PlaybackDownload::new_in(4, &root).unwrap();
            assert_eq!(output.file.metadata().unwrap().nlink(), 0);
            assert_eq!(
                output.file.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
            output.write(b"old").await.unwrap();
            output.restart().await.unwrap();
            assert!(output.write(b"excess").await.is_err());
            output.write(b"good").await.unwrap();
            let file = output
                .finish(crate::model::PlayableMedia::Audio)
                .await
                .unwrap();
            assert_eq!(fs::read(file.path()).unwrap(), b"good");
            assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
            drop(file);
            assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
            assert!(PlaybackDownload::new_in(0, &root).is_err());
            assert!(PlaybackDownload::new_in(4 * 1024_u64.pow(3) + 1, &root).is_err());
            let mut incomplete = PlaybackDownload::new_in(4, &root).unwrap();
            incomplete.write(b"x").await.unwrap();
            assert!(
                incomplete
                    .finish(crate::model::PlayableMedia::Audio)
                    .await
                    .is_err()
            );
            assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        });
    }

    #[test]
    fn playback_storage_rejects_shared_and_symlink_directories() {
        let directory = tempfile::tempdir().unwrap();
        let shared = directory.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(PlaybackDownload::new_in(1, &shared).is_err());
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(directory.path(), &link).unwrap();
        assert!(PlaybackDownload::new_in(1, &link).is_err());
    }

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

    #[test]
    fn session_file_is_private_and_can_be_reopened_after_release() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.session");
        assert!(lock_session_file(&path, false).is_err());
        assert!(!path.exists());
        let first = lock_session_file(&path, true).unwrap();
        assert_eq!(
            first.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(lock_session_file(&path, true).is_err());
        drop(first);
        fs::write(&path, b"existing session").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let reopened = lock_session_file(&path, false).unwrap();
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
        assert!(lock_session_file(&path, true).is_err());
        fs::remove_file(&path).unwrap();
        fs::hard_link(&target, &path).unwrap();
        assert!(lock_session_file(&path, true).is_err());
        assert_eq!(fs::read(target).unwrap(), b"other data");
    }

    #[test]
    fn sqlite_session_can_initialize_and_reopen_the_locked_file() {
        use grammers_session::{Session, storages::SqliteSession, types::UpdateState};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.session");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            {
                let _lock = lock_session_file(&path, true).unwrap();
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
            let _lock = lock_session_file(&path, true).unwrap();
            let session = SqliteSession::open(&path).await.unwrap();
            assert_eq!(session.updates_state().await.unwrap().pts, 42);
        });
    }

    #[test]
    fn restarted_save_replaces_partial_data_with_a_complete_large_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.bin");
        let chunk = vec![7; 512 * 1024];
        let mut expected = Vec::new();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut save = PendingSave::new(path.clone()).unwrap();
            save.write(&[1; 200]).await.unwrap();
            save.restart().await.unwrap();
            for index in 0..25 {
                save.write(&[index]).await.unwrap();
                save.write(&chunk).await.unwrap();
                expected.push(index);
                expected.extend_from_slice(&chunk);
            }
            save.finish(Some(expected.len())).await.unwrap();
        });
        assert_eq!(fs::read(path).unwrap(), expected);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn incomplete_and_colliding_saves_remove_the_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("target.bin");
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut incomplete = PendingSave::new(path.clone()).unwrap();
            incomplete.write(b"partial").await.unwrap();
            assert!(incomplete.finish(Some(100)).await.is_err());
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);

            let mut colliding = PendingSave::new(path.clone()).unwrap();
            colliding.write(b"new").await.unwrap();
            fs::write(&path, b"existing").unwrap();
            assert!(colliding.finish(Some(3)).await.is_err());
            assert!(PendingSave::new(path.clone()).is_err());
        });
        assert_eq!(fs::read(path).unwrap(), b"existing");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
