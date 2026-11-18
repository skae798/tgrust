use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
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

/// Explicitly unlock before closing: a forked child can briefly inherit the
/// same open file description before exec closes its CLOEXEC descriptor.
pub struct SessionLock(File);

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Holds the original session lock while a confirmed logout shuts down the client.
#[derive(Clone)]
pub struct SessionRemoval {
    path: PathBuf,
    lock: Arc<SessionLock>,
}

impl SessionRemoval {
    pub fn new(path: PathBuf, lock: SessionLock) -> Self {
        Self {
            path,
            lock: Arc::new(lock),
        }
    }

    pub fn remove(self) -> Result<()> {
        let locked = self.lock.0.metadata()?;
        let current = fs::symlink_metadata(&self.path)?;
        ensure!(
            current.is_file()
                && current.nlink() == 1
                && (current.dev(), current.ino()) == (locked.dev(), locked.ino()),
            "Файл сессии изменился после открытия"
        );
        let sidecars = ["-wal", "-shm", "-journal"].map(|suffix| {
            let mut path = self.path.clone().into_os_string();
            path.push(suffix);
            PathBuf::from(path)
        });
        for path in &sidecars {
            match fs::symlink_metadata(path) {
                Ok(info) => ensure!(
                    info.is_file() && info.nlink() == 1,
                    "Некорректный служебный файл сессии"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for path in &sidecars {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        fs::remove_file(&self.path)?;
        let _ = fs::remove_file(self.path.with_extension("session.identity"));
        Ok(())
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

pub fn open_session_file(
    profile: &crate::model::AccountProfile,
    create: bool,
) -> Result<(PathBuf, SessionLock)> {
    open_profile_in(&session_directory()?, profile, create)
}

fn open_profile_in(
    directory: &Path,
    profile: &crate::model::AccountProfile,
    create: bool,
) -> Result<(PathBuf, SessionLock)> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .context("Не удалось создать каталог сессии")?;
    check_session_directory(directory)?;
    let path = directory.join(format!("{}.session", profile.key()));
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

fn read_private_settings(path: &Path) -> Result<Option<gtk::glib::KeyFile>> {
    let info = match fs::symlink_metadata(path) {
        Ok(info) => info,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        info.is_file() && info.mode() & 0o077 == 0 && info.nlink() == 1 && info.len() <= 4096,
        "Настройки должны быть обычным файлом с правами 0600"
    );
    let file = File::open(path)?;
    let opened = file.metadata()?;
    ensure!(
        (info.dev(), info.ino()) == (opened.dev(), opened.ino()),
        "Файл настроек изменился при чтении"
    );
    let mut text = String::new();
    file.take(4097).read_to_string(&mut text)?;
    ensure!(
        text.len() <= 4096 && !text.contains('\0'),
        "Некорректный файл настроек"
    );
    let key_file = gtk::glib::KeyFile::new();
    key_file
        .load_from_data(&text, gtk::glib::KeyFileFlags::NONE)
        .map_err(|_| anyhow::anyhow!("Некорректный файл настроек"))?;
    Ok(Some(key_file))
}

fn write_private_settings(path: &Path, key_file: &gtk::glib::KeyFile) -> Result<()> {
    let directory = path.parent().context("Нет каталога настроек")?;
    check_session_directory(directory)?;
    let data = key_file.to_data();
    ensure!(data.len() <= 4096, "Файл настроек слишком длинный");
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(data.as_bytes())?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub fn account_identity(
    profile: &crate::model::AccountProfile,
) -> Option<crate::model::AccountIdentity> {
    let directory = session_directory().ok()?;
    check_session_directory(&directory).ok()?;
    let path = directory.join(format!("{}.session.identity", profile.key()));
    let key_file = read_private_settings(&path).ok()??;
    Some(crate::model::AccountIdentity {
        user_id: key_file.int64("Account", "UserId").ok()?,
        name: key_file.string("Account", "Name").ok()?.to_string(),
        username: key_file.string("Account", "Username").ok()?.to_string(),
    })
}

pub fn remember_account_identity(
    profile: &crate::model::AccountProfile,
    identity: &crate::model::AccountIdentity,
) -> Result<()> {
    let path = session_directory()?.join(format!("{}.session.identity", profile.key()));
    let key_file = gtk::glib::KeyFile::new();
    key_file.set_int64("Account", "UserId", identity.user_id);
    key_file.set_string("Account", "Name", &identity.name);
    key_file.set_string("Account", "Username", &identity.username);
    write_private_settings(&path, &key_file)
}

pub fn application_credentials() -> Result<Option<(i32, String)>> {
    let directory = gtk::glib::user_config_dir().join("tgrust");
    match check_session_directory(&directory) {
        Ok(()) => {}
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    }
    let Some(key_file) = read_private_settings(&directory.join("telegram-app.conf"))? else {
        return Ok(None);
    };
    let id = key_file
        .integer("Telegram", "ApiId")
        .map_err(|_| anyhow::anyhow!("Нет API ID приложения"))?;
    let hash = key_file
        .string("Telegram", "ApiHash")
        .map_err(|_| anyhow::anyhow!("Нет API hash приложения"))?;
    Ok(Some((id, hash.to_string())))
}

pub fn save_application_credentials(id: i32, hash: &str) -> Result<()> {
    let directory = gtk::glib::user_config_dir().join("tgrust");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    let key_file = gtk::glib::KeyFile::new();
    key_file.set_integer("Telegram", "ApiId", id);
    key_file.set_string("Telegram", "ApiHash", hash);
    write_private_settings(&directory.join("telegram-app.conf"), &key_file)
}

pub fn saved_accounts() -> Result<Vec<crate::model::AccountProfile>> {
    find_saved_accounts(&session_directory()?)
}

fn find_saved_accounts(directory: &Path) -> Result<Vec<crate::model::AccountProfile>> {
    if let Err(error) = check_session_directory(directory) {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(Vec::new());
        }
        return Err(error);
    }
    let mut profiles = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let Some(profile) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".session"))
            .and_then(crate::model::AccountProfile::parse)
        else {
            continue;
        };
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.is_file() && metadata.nlink() == 1 && metadata.len() != 0 {
            profiles.push(profile);
        }
    }
    profiles.sort();
    Ok(profiles)
}

pub(crate) fn lock_session_file(path: &Path, create: bool) -> Result<SessionLock> {
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
    let lock = SessionLock(file);
    // The path can be unlinked between opening it and taking the lock.
    let current = fs::symlink_metadata(path)?;
    let opened = lock.0.metadata()?;
    ensure!(
        current.is_file()
            && current.nlink() == 1
            && (current.dev(), current.ino()) == (opened.dev(), opened.ino()),
        "Файл сессии изменился во время открытия"
    );
    lock.0.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(lock)
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
    fn account_discovery_keeps_legacy_and_named_sessions_separate() -> Result<()> {
        use crate::model::AccountProfile;
        let root = tempfile::tempdir()?;
        let directory = root.path().join("tgrust");
        assert!(find_saved_accounts(&directory)?.is_empty());
        DirBuilder::new().mode(0o700).create(&directory)?;
        fs::write(directory.join("123.session-wal"), b"sidecar")?;
        fs::write(directory.join("456.session"), [])?;
        std::os::unix::fs::symlink("123.session-wal", directory.join("789.session"))?;
        fs::write(directory.join("0123.session"), b"invalid name")?;
        fs::write(directory.join("123-Upper.session"), b"invalid name")?;
        assert!(find_saved_accounts(&directory)?.is_empty());
        for name in ["123.session", "123-home.session", "123-work.session"] {
            fs::write(directory.join(name), b"fictional session")?;
        }
        fs::hard_link(
            directory.join("123.session-wal"),
            directory.join("123-link.session"),
        )?;
        assert_eq!(
            find_saved_accounts(&directory)?,
            [
                AccountProfile::legacy(123),
                AccountProfile::named(123, "home")?,
                AccountProfile::named(123, "work")?,
            ]
        );
        Ok(())
    }

    #[test]
    fn profiles_with_the_same_api_id_hold_independent_databases_and_logout_targets() -> Result<()> {
        use crate::model::AccountProfile;
        use grammers_session::{Session, storages::SqliteSession, types::UpdateState};
        let root = tempfile::tempdir()?;
        let dir = root.path().join("tgrust");
        let home = AccountProfile::named(100, "home")?;
        let work = AccountProfile::named(100, "work")?;
        let (home_path, home_lock) = open_profile_in(&dir, &home, true)?;
        let (work_path, work_lock) = open_profile_in(&dir, &work, true)?;
        assert_ne!(home_path, work_path);
        assert!(open_profile_in(&dir, &home, true).is_err());
        assert!(open_profile_in(&dir, &work, true).is_err());
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(async {
            let home_db = SqliteSession::open(&home_path).await?;
            let work_db = SqliteSession::open(&work_path).await?;
            home_db
                .set_update_state(UpdateState::Primary {
                    pts: 10,
                    date: 10,
                    seq: 10,
                })
                .await?;
            work_db
                .set_update_state(UpdateState::Primary {
                    pts: 20,
                    date: 20,
                    seq: 20,
                })
                .await?;
            assert_eq!(home_db.updates_state().await.unwrap().pts, 10);
            assert_eq!(work_db.updates_state().await.unwrap().pts, 20);
            Ok::<(), anyhow::Error>(())
        })?;
        SessionRemoval::new(work_path, work_lock).remove()?;
        assert_eq!(find_saved_accounts(&dir)?, std::slice::from_ref(&home));
        assert!(open_profile_in(&dir, &home, false).is_err());
        drop(home_lock);
        let (_, _reopened) = open_profile_in(&dir, &home, false)?;
        assert_eq!(
            runtime.block_on(async {
                SqliteSession::open(home_path)
                    .await
                    .unwrap()
                    .updates_state()
                    .await
                    .unwrap()
                    .pts
            }),
            10
        );
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
            first.0.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(lock_session_file(&path, true).is_err());
        drop(first);
        fs::write(&path, b"existing session").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let reopened = lock_session_file(&path, false).unwrap();
        assert_eq!(
            reopened.0.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read(path).unwrap(), b"existing session");
    }

    #[test]
    fn session_lock_is_released_even_while_an_inherited_descriptor_stays_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic.session");
        let owner = lock_session_file(&path, true).unwrap();
        // dup and fork share the same open file description for flock purposes.
        let inherited = owner.0.try_clone().unwrap();
        assert!(lock_session_file(&path, false).is_err());
        drop(owner);
        let next_owner = lock_session_file(&path, false).unwrap();
        assert!(inherited.metadata().unwrap().is_file());
        drop(inherited);
        assert!(lock_session_file(&path, false).is_err());
        drop(next_owner);
        assert!(lock_session_file(&path, false).is_ok());
    }

    #[test]
    fn logout_removes_only_the_locked_session_and_its_sqlite_sidecars() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let path = directory.path().join("123.session");
        let lock = lock_session_file(&path, true)?;
        let removal = SessionRemoval::new(path.clone(), lock);
        tokio::runtime::Runtime::new()?.block_on(async {
            use grammers_session::{Session, storages::SqliteSession, types::UpdateState};

            let session = SqliteSession::open(&path).await?;
            session
                .set_update_state(UpdateState::Primary {
                    pts: 1,
                    date: 1,
                    seq: 1,
                })
                .await?;
            Ok::<(), anyhow::Error>(())
        })?;
        for name in [
            "123.session-wal",
            "123.session-shm",
            "123.session-journal",
            "123.session.identity",
            "456.session",
            "456.session.identity",
            "saved.png",
        ] {
            fs::write(directory.path().join(name), b"fictional")?;
        }
        assert!(lock_session_file(&path, false).is_err());
        removal.remove()?;
        assert!(!path.exists());
        for name in [
            "123.session-wal",
            "123.session-shm",
            "123.session-journal",
            "123.session.identity",
        ] {
            assert!(!directory.path().join(name).exists());
        }
        assert!(directory.path().join("456.session.identity").is_file());
        assert_eq!(
            find_saved_accounts(directory.path())?,
            vec![crate::model::AccountProfile::legacy(456)]
        );
        assert_eq!(fs::read(directory.path().join("saved.png"))?, b"fictional");
        assert!(lock_session_file(&path, true).is_ok());
        Ok(())
    }

    #[test]
    fn logout_refuses_replaced_session_and_does_not_follow_sidecar_links() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("123.session");
        let lock = lock_session_file(&path, true)?;
        let removal = SessionRemoval::new(path.clone(), lock);
        let saved = directory.path().join("saved.png");
        fs::write(&saved, b"fictional")?;
        let sidecar = directory.path().join("123.session-wal");
        std::os::unix::fs::symlink(&saved, &sidecar)?;
        assert!(removal.clone().remove().is_err());
        assert!(path.is_file());
        assert!(sidecar.is_symlink());
        fs::remove_file(sidecar)?;
        fs::rename(&path, directory.path().join("original.session"))?;
        fs::write(&path, b"replacement")?;
        assert!(removal.remove().is_err());
        assert_eq!(fs::read(&path)?, b"replacement");
        assert_eq!(fs::read(saved)?, b"fictional");
        Ok(())
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
