# tgrust

[Русский](README.md) · English

An unofficial Telegram client for GNOME/Wayland, built with Rust, GTK 4, and Libadwaita.

## Quick start

For Fedora 45 x86_64, download `tgrust-fedora45-x86_64` from the [latest release](https://github.com/skae798/tgrust/releases/latest). Install the client and launch the demo from the download folder.

~~~sh
sudo dnf install gtk4 libadwaita gstreamer1-plugins-base gstreamer1-plugins-good
install -Dm755 ./tgrust-fedora45-x86_64 ~/.local/bin/tgrust
~/.local/bin/tgrust --demo
~~~

Demo mode works without an account. To sign in, launch tgrust without `--demo` and save your own [Telegram application credentials](https://my.telegram.org/apps) under **Аккаунты → Ключи приложения Telegram** (Accounts → Telegram application credentials). Sign in with a QR code or phone number.

The prebuilt client checks for updates automatically and opens the new build on the next launch.

## Features

- **Messages** — scroll to load history, search, reply, edit, react, and vote in polls.
- **Media** — albums, streaming video and music, inline voice messages, and separate photo and video viewers.
- **Chats** — create groups and channels, folders, archive, GNOME notifications, member and admin management, invitations, and recent actions.
- **Accounts** — separate windows, profile and privacy settings, session management, and SOCKS5.

## Documentation

- [Build from source or package as Flatpak (Russian)](docs/guide.md#установка)
- [User guide and Telegram API limitations (Russian)](docs/guide.md)
- [Automated and manual checks (Russian)](docs/testing.md)
- [Roadmap](https://github.com/skae798/tgrust/issues/4) · [GPL-3.0 license](LICENSE)
