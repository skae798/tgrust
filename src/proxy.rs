use std::fs::{self, DirBuilder, File, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::net::Ipv6Addr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use grammers_client::sender::ConnectionParams;

use crate::model::ProxySettings;

const GROUP: &str = "SOCKS5";
const FILE: &str = "proxy.conf";
const MAX_FILE_BYTES: u64 = 16 * 1024;

pub fn validate(settings: &ProxySettings) -> Result<Option<url::Url>> {
    if !settings.enabled {
        return Ok(None);
    }
    let host = settings.host.trim();
    ensure!(
        !host.is_empty() && host.len() <= 253,
        "Укажите адрес SOCKS5"
    );
    let host = match host.parse::<Ipv6Addr>() {
        Ok(ip) => url::Host::Ipv6(ip),
        Err(_) => {
            url::Host::parse(host).map_err(|_| anyhow::anyhow!("Некорректный адрес SOCKS5"))?
        }
    };
    let port = settings
        .port
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .context("Порт должен быть числом от 1 до 65535")?;
    ensure!(
        settings.username.len() <= 255 && settings.password.len() <= 255,
        "Имя пользователя и пароль SOCKS5 допускают до 255 байт каждое"
    );
    ensure!(
        settings.username.is_empty() == settings.password.is_empty(),
        "Для аутентификации заполните имя пользователя и пароль"
    );
    ensure!(
        !settings.username.contains('\0') && !settings.password.contains('\0'),
        "Некорректные данные аутентификации SOCKS5"
    );
    let mut endpoint = url::Url::parse(&format!("socks5://{host}:{port}"))
        .map_err(|_| anyhow::anyhow!("Некорректный адрес SOCKS5"))?;
    if !settings.username.is_empty() {
        endpoint
            .set_username(&settings.username.replace('%', "%25"))
            .map_err(|_| anyhow::anyhow!("Некорректное имя пользователя SOCKS5"))?;
        endpoint
            .set_password(Some(&settings.password.replace('%', "%25")))
            .map_err(|_| anyhow::anyhow!("Некорректный пароль SOCKS5"))?;
    }
    Ok(Some(endpoint))
}

pub fn configuration(settings: &ProxySettings) -> Result<ConnectionParams> {
    Ok(ConnectionParams {
        proxy_url: validate(settings)?.map(|endpoint| endpoint.into()),
        ..ConnectionParams::default()
    })
}

pub fn load() -> Result<ProxySettings> {
    load_in(&gtk::glib::user_config_dir().join("tgrust"))
        .context("Не удалось прочитать настройки прокси")
}

pub fn save(settings: &ProxySettings) -> Result<()> {
    save_in(&gtk::glib::user_config_dir().join("tgrust"), settings)
        .context("Не удалось сохранить настройки прокси")
}

fn has_private_directory(directory: &Path) -> Result<bool> {
    let info = match fs::symlink_metadata(directory) {
        Ok(info) => info,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        info.is_dir() && info.mode() & 0o077 == 0,
        "Каталог настроек прокси должен быть обычным каталогом с правами 0700"
    );
    Ok(true)
}

fn load_in(directory: &Path) -> Result<ProxySettings> {
    if !has_private_directory(directory)? {
        return Ok(ProxySettings::default());
    }
    let path = directory.join(FILE);
    let info = match fs::symlink_metadata(&path) {
        Ok(info) => info,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(ProxySettings::default()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        info.is_file() && info.nlink() == 1 && info.mode() & 0o077 == 0,
        "Настройки прокси должны храниться в обычном файле с правами 0600"
    );
    let file = File::open(path)?;
    let opened = file.metadata()?;
    ensure!(
        (opened.dev(), opened.ino()) == (info.dev(), info.ino()),
        "Файл настроек изменился при чтении"
    );
    let mut data = String::new();
    file.take(MAX_FILE_BYTES + 1).read_to_string(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_FILE_BYTES && !data.contains('\0'),
        "Некорректный файл настроек прокси"
    );
    let key = gtk::glib::KeyFile::new();
    key.load_from_data(&data, gtk::glib::KeyFileFlags::NONE)
        .map_err(|_| anyhow::anyhow!("Некорректный файл настроек прокси"))?;
    let get = |name| {
        key.string(GROUP, name)
            .map(|value| value.to_string())
            .map_err(|_| anyhow::anyhow!("Некорректное поле в файле настроек прокси"))
    };
    let settings = ProxySettings {
        enabled: key
            .boolean(GROUP, "Enabled")
            .map_err(|_| anyhow::anyhow!("Не указан режим подключения"))?,
        host: get("Host")?,
        port: get("Port")?,
        username: get("Username")?,
        password: get("Password")?,
    };
    validate(&settings)?;
    Ok(settings)
}

fn save_in(directory: &Path, settings: &ProxySettings) -> Result<()> {
    validate(settings)?;
    ensure!(
        [
            &settings.host,
            &settings.port,
            &settings.username,
            &settings.password
        ]
        .iter()
        .all(|value| value.len() <= 1024 && !value.contains('\0')),
        "Некорректные или слишком длинные параметры прокси"
    );
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    has_private_directory(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.as_file()
        .set_permissions(Permissions::from_mode(0o600))?;
    let key = gtk::glib::KeyFile::new();
    key.set_boolean(GROUP, "Enabled", settings.enabled);
    for (name, value) in [
        ("Host", &settings.host),
        ("Port", &settings.port),
        ("Username", &settings.username),
        ("Password", &settings.password),
    ] {
        key.set_string(GROUP, name, value);
    }
    file.write_all(key.to_data().as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(directory.join(FILE))
        .map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4, SocketAddrV6};
    use std::os::unix::fs::symlink;
    use std::sync::Arc;
    use std::time::Duration;

    use grammers_client::{Client, sender::SenderPool};
    use grammers_session::{Session, storages::MemorySession, types::DcOption};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn example() -> ProxySettings {
        ProxySettings {
            enabled: true,
            host: "example.test".into(),
            port: "1080".into(),
            username: "имя@50%2F".into(),
            password: "пароль: /?#%40".into(),
        }
    }

    #[test]
    fn direct_mode_ignores_unused_fields_and_addresses_are_validated() {
        let mut settings = example();
        settings.host = "::1".into();
        assert_eq!(
            validate(&settings).unwrap().unwrap().host_str(),
            Some("[::1]")
        );
        settings.host = "https://example.test".into();
        assert!(validate(&settings).is_err());
        settings.host = "example.test".into();
        for port in ["0", "65536", "unknown"] {
            settings.port = port.into();
            assert!(validate(&settings).is_err());
        }
        settings.port = "1080".into();
        settings.username.clear();
        assert!(validate(&settings).is_err());
        settings.enabled = false;
        assert!(configuration(&settings).unwrap().proxy_url.is_none());
    }

    #[test]
    fn private_settings_round_trip_and_refuse_non_private_inputs() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("tgrust");
        assert!(!load_in(&directory).unwrap().enabled);
        let mut settings = example();
        save_in(&directory, &settings).unwrap();
        assert!(load_in(&directory).unwrap() == settings);
        let path = directory.join(FILE);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        settings.enabled = false;
        save_in(&directory, &settings).unwrap();
        assert!(load_in(&directory).unwrap() == settings);
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        assert!(load_in(&directory).is_err());
        fs::rename(&path, directory.join("other.conf")).unwrap();
        symlink("other.conf", &path).unwrap();
        assert!(load_in(&directory).is_err());
    }

    #[test]
    fn damaged_credentials_cannot_silently_enable_direct_or_anonymous_connection() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("tgrust");
        save_in(&directory, &example()).unwrap();
        fs::write(
            directory.join(FILE),
            "[SOCKS5]\nEnabled=true\nHost=example.test\nPort=1080\n",
        )
        .unwrap();
        assert!(load_in(&directory).is_err());
        fs::write(directory.join(FILE), "[SOCKS5]\nEnabled=unexpected\n").unwrap();
        assert!(load_in(&directory).is_err());
    }

    #[tokio::test]
    async fn sender_pool_authenticates_and_does_not_fall_back_to_the_direct_socket() {
        tokio::time::timeout(Duration::from_secs(10), async {
            check_proxy_route(false).await;
            check_proxy_route(true).await;
        })
        .await
        .unwrap();
    }

    async fn check_proxy_route(authenticated: bool) {
        let direct = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let destination = direct.local_addr().unwrap();
        let mut settings = example();
        settings.host = Ipv4Addr::LOCALHOST.to_string();
        settings.port = proxy.local_addr().unwrap().port().to_string();
        if !authenticated {
            settings.username.clear();
            settings.password.clear();
        }
        let session = Arc::new(MemorySession::default());
        session.set_home_dc_id(2).await.unwrap();
        session
            .set_dc_option(&DcOption {
                id: 2,
                ipv4: SocketAddrV4::new(Ipv4Addr::LOCALHOST, destination.port()),
                ipv6: SocketAddrV6::new(Ipv6Addr::LOCALHOST, destination.port(), 0, 0),
                auth_key: None,
            })
            .await
            .unwrap();
        let pool =
            SenderPool::with_configuration(session, 12345, configuration(&settings).unwrap());
        let sender = tokio::spawn(pool.runner.run());
        let client = Client::new(pool.handle);
        let request = tokio::spawn(async move { client.is_authorized().await });

        // The default sender policy retries an I/O failure once.
        for _ in 0..2 {
            reject_socks_request(&proxy, destination.port(), &settings).await;
        }
        let error = request.await.unwrap().unwrap_err().to_string();
        assert!(!error.contains(&example().password));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), direct.accept())
                .await
                .is_err()
        );
        sender.abort();
    }

    async fn reject_socks_request(
        proxy: &TcpListener,
        destination_port: u16,
        settings: &ProxySettings,
    ) {
        let (mut connection, _) = proxy.accept().await.unwrap();
        assert_eq!(connection.read_u8().await.unwrap(), 5);
        let methods_len = connection.read_u8().await.unwrap();
        let mut methods = vec![0; usize::from(methods_len)];
        connection.read_exact(&mut methods).await.unwrap();
        let method = if settings.username.is_empty() { 0 } else { 2 };
        assert!(methods.contains(&method));
        connection.write_all(&[5, method]).await.unwrap();
        if method == 2 {
            assert_eq!(connection.read_u8().await.unwrap(), 1);
            for expected in [&settings.username, &settings.password] {
                let len = connection.read_u8().await.unwrap();
                let mut value = vec![0; usize::from(len)];
                connection.read_exact(&mut value).await.unwrap();
                assert_eq!(value, expected.as_bytes());
            }
            connection.write_all(&[1, 0]).await.unwrap();
        }
        let mut command = [0; 10];
        connection.read_exact(&mut command).await.unwrap();
        assert_eq!(&command[..8], &[5, 1, 0, 1, 127, 0, 0, 1]);
        assert_eq!(
            u16::from_be_bytes([command[8], command[9]]),
            destination_port
        );
        connection
            .write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
    }
}
