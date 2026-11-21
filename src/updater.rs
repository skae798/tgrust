use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const RELEASE_API: &str = "https://api.github.com/repos/skae798/tgrust/releases/latest";
const DOWNLOADS: &str = "https://github.com/skae798/tgrust/releases/download";
const TAG_PREFIX: &str = "native-fedora45-";
const ASSET: &str = "tgrust-fedora45-x86_64";
const MAX_METADATA: u64 = 256 * 1024;
const MAX_BINARY: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Clone)]
pub struct Installation {
    binary: PathBuf,
    cache: PathBuf,
    identity: Identity,
    build: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Current,
    Installed(u64),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    state: String,
    size: u64,
    digest: Option<String>,
}

struct Update {
    build: u64,
    url: String,
    size: u64,
    sha256: String,
}

pub fn version() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match (
        option_env!("TGRUST_UPDATE_BUILD"),
        option_env!("TGRUST_UPDATE_REVISION"),
    ) {
        (Some(build), Some(revision)) => format!("{version} · сборка {build} ({revision})"),
        _ => version.into(),
    }
}

impl Installation {
    pub fn detect(demo: bool) -> Result<Self, &'static str> {
        if demo {
            return Err("Обновления отключены в деморежиме.");
        }
        if Path::new("/.flatpak-info").exists() {
            return Err("Обновления Flatpak устанавливаются через менеджер приложений.");
        }
        let Some(build) = option_env!("TGRUST_UPDATE_BUILD").and_then(|value| value.parse().ok())
        else {
            return Err(
                "Этот экземпляр собран из исходников. Автообновление доступно в нативном выпуске для Fedora.",
            );
        };
        let supported = std::env::consts::ARCH == "x86_64"
            && fs::read_to_string("/etc/os-release").is_ok_and(|release| supported_os(&release));
        if !supported {
            return Err("Канал автоматических обновлений рассчитан на Fedora 45 x86_64.");
        }
        let binary = gtk::glib::home_dir().join(".local/bin/tgrust");
        let cache = gtk::glib::user_cache_dir().join("tgrust-update");
        let unsupported = "Автообновление доступно при запуске выпуска из ~/.local/bin/tgrust.";
        let installed = fs::symlink_metadata(&binary).map_err(|_| unsupported)?;
        let running = fs::metadata("/proc/self/exe").map_err(|_| unsupported)?;
        if !installed.is_file() || Identity::of(&running) != Identity::of(&installed) {
            return Err(unsupported);
        }
        Ok(Self {
            binary,
            cache,
            identity: Identity::of(&installed),
            build,
        })
    }

    pub fn check(&self, progress: impl Fn(u8)) -> Result<Outcome> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(Duration::from_secs(120)))
            .timeout_connect(Some(Duration::from_secs(12)))
            .timeout_per_read(Some(Duration::from_secs(20)))
            .user_agent("tgrust-updater")
            .build()
            .into();
        let mut response = agent
            .get(RELEASE_API)
            .header("Accept", "application/vnd.github+json")
            .call()
            .context("Не удалось получить сведения о выпуске с GitHub")?;
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(MAX_METADATA + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_METADATA,
            "Ответ GitHub слишком большой"
        );
        let release = serde_json::from_slice(&bytes).context("Неверные сведения о выпуске")?;
        let Some(update) = select_update(release, self.build)? else {
            return Ok(Outcome::Current);
        };

        fs::create_dir_all(&self.cache)?;
        let lock = File::options()
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.cache.join("update.lock"))?;
        lock.try_lock().context("Обновление уже выполняется")?;
        self.check_identity()?;
        progress(0);
        let mut response = agent
            .get(&update.url)
            .call()
            .context("Не удалось загрузить выпуск с GitHub")?;
        self.replace(&update, response.body_mut().as_reader(), progress)?;
        Ok(Outcome::Installed(update.build))
    }

    fn check_identity(&self) -> Result<()> {
        let current = fs::symlink_metadata(&self.binary)?;
        ensure!(
            current.is_file() && Identity::of(&current) == self.identity,
            "Установленный файл изменился. Перезапустите tgrust"
        );
        Ok(())
    }

    fn replace(&self, update: &Update, reader: impl Read, progress: impl Fn(u8)) -> Result<()> {
        let directory = self
            .binary
            .parent()
            .context("Не найден каталог установки")?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        let mut source = reader.take(update.size + 1);
        let mut hash = Sha256::new();
        let mut received = 0;
        let mut percent = 0;
        let mut buffer = [0; 16 * 1024];
        loop {
            let length = source.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            received += length as u64;
            ensure!(
                received <= update.size,
                "Загруженный файл превышает размер выпуска"
            );
            let chunk = &buffer[..length];
            temporary.write_all(chunk)?;
            hash.update(chunk);
            let next_percent = (received * 100 / update.size) as u8;
            if next_percent != percent {
                percent = next_percent;
                progress(percent);
            }
        }
        ensure!(
            received == update.size,
            "Файл обновления загружен не полностью"
        );
        ensure!(
            hex(&hash.finalize()).eq_ignore_ascii_case(&update.sha256),
            "Контрольная сумма обновления не совпала"
        );
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
        temporary.as_file().sync_all()?;
        self.check_identity()?;
        temporary
            .persist(&self.binary)
            .map_err(|error| error.error)?;
        // The rename is already visible. A filesystem which cannot sync directories
        // still has a complete executable at the destination.
        let _ = File::open(directory).and_then(|directory| directory.sync_all());
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn supported_os(release: &str) -> bool {
    let field = |name: &str| {
        release.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key == name).then(|| value.trim_matches(['"', '\'']))
        })
    };
    field("ID") == Some("fedora") && field("VERSION_ID") == Some("45")
}

fn select_update(release: Release, installed: u64) -> Result<Option<Update>> {
    let tag = &release.tag_name;
    let (build, revision) = tag
        .strip_prefix(TAG_PREFIX)
        .and_then(|suffix| suffix.split_once('-'))
        .context("Последний выпуск не относится к каналу Fedora 45")?;
    ensure!(
        revision.len() == 12
            && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
            && !release.draft
            && !release.prerelease,
        "Неверная метка выпуска"
    );
    let number: u64 = build.parse().context("Неверный номер сборки")?;
    ensure!(
        number > 0 && number.to_string() == build,
        "Неверный номер сборки"
    );
    if number <= installed {
        return Ok(None);
    }
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == ASSET && asset.state == "uploaded")
        .context("В выпуске отсутствует сборка для Fedora 45 x86_64")?;
    ensure!(
        (1..=MAX_BINARY).contains(&asset.size),
        "Неверный размер файла обновления"
    );
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .context("GitHub не предоставил контрольную сумму обновления")?;
    Ok(Some(Update {
        build: number,
        url: format!("{DOWNLOADS}/{tag}/{ASSET}"),
        size: asset.size,
        sha256: sha256.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(data: &[u8]) -> Release {
        Release {
            tag_name: "native-fedora45-21-123abcdef456".into(),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: ASSET.into(),
                state: "uploaded".into(),
                size: data.len() as u64,
                digest: Some(format!("sha256:{}", hex(&Sha256::digest(data)))),
            }],
        }
    }

    #[test]
    fn selects_only_newer_published_assets_with_bounded_size_and_checksum() {
        assert!(select_update(release(b"new"), 21).unwrap().is_none());
        assert!(select_update(release(b"old"), 25).unwrap().is_none());
        let update = select_update(release(b"new"), 20).unwrap().unwrap();
        assert_eq!(update.build, 21);
        assert_eq!(
            update.url,
            format!("{DOWNLOADS}/native-fedora45-21-123abcdef456/{ASSET}")
        );
        for invalid_size in [0, MAX_BINARY + 1] {
            let mut bad = release(b"new");
            bad.assets[0].size = invalid_size;
            assert!(select_update(bad, 20).is_err());
        }
        let mut bad = release(b"new");
        bad.assets[0].digest = None;
        assert!(select_update(bad, 20).is_err());
        let mut draft = release(b"new");
        draft.draft = true;
        assert!(select_update(draft, 20).is_err());
        let mut bad_tag = release(b"new");
        bad_tag.tag_name = "native-fedora45-+21-123abcdef456".into();
        assert!(select_update(bad_tag, 20).is_err());
    }

    #[test]
    fn rejects_truncation_corruption_overruns_and_a_concurrent_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("tgrust");
        fs::write(&binary, b"old").unwrap();
        let installation = Installation {
            identity: Identity::of(&fs::metadata(&binary).unwrap()),
            binary: binary.clone(),
            cache: directory.path().into(),
            build: 20,
        };
        let update = select_update(release(b"new"), 20).unwrap().unwrap();
        for data in [b"ne".as_slice(), b"bad", b"new!"] {
            assert!(installation.replace(&update, data, |_| {}).is_err());
            assert_eq!(fs::read(&binary).unwrap(), b"old");
            assert_eq!(directory.path().read_dir().unwrap().count(), 1);
        }
        let old_executable = File::open(&binary).unwrap();
        installation
            .replace(&update, b"new".as_slice(), |_| {})
            .unwrap();
        assert_eq!(fs::read(&binary).unwrap(), b"new");
        assert_eq!(fs::metadata(&binary).unwrap().mode() & 0o777, 0o755);
        assert_ne!(
            Identity::of(&old_executable.metadata().unwrap()),
            Identity::of(&fs::metadata(&binary).unwrap())
        );
        assert!(
            installation
                .replace(&update, b"new".as_slice(), |_| {})
                .is_err()
        );
    }

    #[test]
    fn limits_the_binary_channel_to_its_build_platform() {
        assert!(supported_os("ID=fedora\nVERSION_ID=\"45\"\n"));
        assert!(!supported_os("ID=other\nID_LIKE=fedora\nVERSION_ID=45\n"));
        assert!(!supported_os("ID=fedora\nVERSION_ID=44\n"));
    }
}
