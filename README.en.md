# tgrust

[Русский](README.md) · English

An unofficial Telegram client for GNOME/Wayland, built with Rust, GTK 4, and Libadwaita.

## Quick start

For Fedora 45 x86_64, download `tgrust-fedora45-x86_64` from the [latest release](https://github.com/skae798/tgrust/releases/latest). Install the client and launch the demo from the download folder.

~~~sh
sudo dnf install gtk4 libadwaita gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad-free
install -Dm755 ./tgrust-fedora45-x86_64 ~/.local/bin/tgrust
~/.local/bin/tgrust --demo
~~~

Demo mode works without an account. To sign in, launch tgrust without `--demo` and save your own [Telegram application credentials](https://my.telegram.org/apps) under **Аккаунты → Ключи приложения Telegram** (Accounts → Telegram application credentials). Sign in with a QR code or phone number.

The prebuilt client checks for updates automatically and opens the new build on the next launch.

In GNOME/Wayland, normal launches use Wayland, including after updates. Background UI checks use a private display and D-Bus through `scripts/test-ui.sh`, including activated portal services. Before publishing an update, CI checks Wayland startup using the finished executable.

## Features

- **Messages** — paginated history, search, replies, edits, and reactions. Completed actions survive switching chats.
- **Polls** — polls and quizzes, deadlines, hidden results, vote locks, shuffled answers, and subscriber-only participation in channels and supergroups, and country restrictions based on account phone numbers. Public polls include a voter list; regular public polls allow participants to add choices.
- **Media** — albums, streaming video and music, inline voice messages, and separate photo and video viewers.
- **Chats** — create groups and channels, enable and manage supergroup topics, folders, archive, GNOME notifications, member and admin management, group and individual restrictions, slow mode, invitations, public addresses, and recent actions.
- **Accounts** — separate windows, profile and privacy settings, session management, and SOCKS5.
- **Stickers** — search by emoji; preview, install, and remove packs by Telegram links; recent and favorite stickers; send to chats or topics, including replies. WebP images render in full; video stickers loop directly in messages, with transparency and no sound. TGS animations play concurrently within messages at the monitor’s scale. Stickers have no captions in history or the picker.

Building from source also requires C++ and `gstreamer1-devel`, `gstreamer1-plugins-base-devel`; see the guide for all dependencies.

## Documentation

- [Build from source or package as Flatpak (Russian)](docs/guide.md#установка)
- [User guide and Telegram API limitations (Russian)](docs/guide.md)
- [Automated and manual checks (Russian)](docs/testing.md)
- [Roadmap](https://github.com/skae798/tgrust/issues/4) · [GPL-3.0 license](LICENSE)
