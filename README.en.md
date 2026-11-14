# tgrust

[Русский](README.md) · English

An unofficial Telegram desktop client for GNOME. tgrust uses the Telegram API through [grammers](https://codeberg.org/Lonami/grammers). It is written in Rust with GTK 4 and Libadwaita and licensed under GPL-3.0. It has its own codebase.

## Features

- Adaptive GNOME interface with the system theme, accent color, and a chat list searchable by name.
- Sign in to an existing account with a phone number, a verification code, and a two-step verification password when required. A local session lets you reopen the client without signing in again.
- Up to 150 recent chats, paginated history, and text messages in private chats and groups. The composer is ready when you open a chat. Press Enter to send or Shift+Enter to insert a line break. History updates as Telegram events arrive.
- Send one local file or photo with a caption from the attachment menu. File mode preserves the original bytes. Photo mode uses Telegram's image processing. Telegram applies account and chat limits.
- Reply in the current chat. Select the quoted-message card to jump to the original. The client loads quoted messages outside the current page when available.
- Edit your own plain-text messages. Telegram determines editing permissions and time limits.
- Delete your own messages for all participants after confirmation, subject to Telegram's permissions and limits.
- Delete messages from Saved Messages, including forwarded items. This deletes the saved copy.
- Forward a single message to an available chat from the list. Telegram enforces forwarding restrictions on protected and self-destructing messages.
- Pin and unpin messages silently. Pins are marked in the loaded history. Telegram checks permissions.
- Add standard emoji reactions from the context menu and see totals and your own reactions in the history. Telegram determines which reactions are available and how many you can add.
- Search message text across chats and jump to a result. The client shows up to 50 available Telegram results per query.
- Switch to the archive view and return chats from the archive. The archive view shows archived chats among the loaded dialogs.
- Receive GNOME notifications about new messages while the client is running. Clicking a notification opens the chat if it is in the loaded list. Notifications show the chat name, honor chat and category mute settings, and skip messages with a timer. An active chat updates silently.
- View folders configured in Telegram, including imported shared folders. Folder filters and pins apply to the loaded chats.
- Omit Telegram sponsored messages.
- Save documents and photos through the system dialog, including from chats with saving disabled. Telegram must still grant the account access to the message and file. If a file reference expires, the client fetches the message again and retries the same file once. Self-destructing messages and timed attachments cannot be saved.
- Large thumbnails with a separate photo viewer. Open a photo by clicking it or using the context menu. The viewer uses an available image up to 1600 × 1600 pixels and keeps it in memory. Timed photos are excluded.
- See names and avatars on incoming and outgoing messages. Group chat previews show the last sender's @username or name. Initials appear when no profile photo is available. History displays thumbnails for photos and documents that have a server thumbnail. Spoiler and timed attachments hide their thumbnails by default.
- Demo mode with fictional chats, local messages, simulated attachment sending, and a downloadable sample file. Demo mode works offline.

Telegram requires clients to display sponsored messages and honor saving restrictions. Telegram may revoke the application's API access because tgrust omits sponsored messages and allows saving protected attachments. See the [Telegram API Terms of Service](https://core.telegram.org/api/terms) and [content protection documentation](https://core.telegram.org/api/content-protection).

## Build on Fedora

Use Rust 1.100.0 or later, GTK 4.16 or later, and Libadwaita 1.6 or later. Building the embedded session database requires CMake and libclang.

```sh
sudo dnf install gcc make cmake clang-devel pkgconf-pkg-config gtk4-devel libadwaita-devel
rustup update stable
cargo build --locked
cargo run --locked -- --demo
```

### Sign in

Create an application at [my.telegram.org](https://my.telegram.org/apps) to get an `api_id` and `api_hash`. Run the client from Bash with these values in its environment. Reading the hash interactively keeps it out of the shell history.

```bash
read -rp 'API ID: ' TGRUST_API_ID
read -rsp 'API hash: ' TGRUST_API_HASH
printf '\n'
TGRUST_API_ID="$TGRUST_API_ID" TGRUST_API_HASH="$TGRUST_API_HASH" cargo run --locked
unset TGRUST_API_ID TGRUST_API_HASH
```

Enter your phone number in international format, then your verification code and your two-step verification password if Telegram asks for it. When there is one valid saved session, tgrust reconnects automatically on the next launch. If Telegram is unreachable, restore the connection and select **Повторить подключение** (Retry connection). If the session expires, provide the application credentials again to sign in.

The session is stored at `${XDG_DATA_HOME:-$HOME/.local/share}/tgrust/<api_id>.session`. tgrust opens it in one instance at a time and sets mode `0600` for new and existing session files. Only the owner can access the session directory. Keep the session file local because its authorization key grants access to the account.

### Install and update from GitHub

Install the build dependencies, Git, `util-linux`, and `desktop-file-utils`. You can clone the repository without signing in to GitHub.

```sh
git clone https://github.com/skae798/tgrust.git
cd tgrust
./scripts/update.sh
```

Run the same script to update. It fetches the latest commit on `main` into a separate cache, uses `Cargo.lock` to build the client, and installs the executable at `~/.local/bin/tgrust`. The launcher and icon go in the user's data directory. Installation starts after a successful build. Restart the client if it was open during the update.

tgrust appears in the GNOME app menu. Select **Add to Favorites** to pin it to the dock. Launching from the menu or dock uses the saved session.

## Project layout

| File | Purpose |
| --- | --- |
| `src/main.rs` | Application startup, arguments, and shutdown |
| `src/ui.rs`, `src/style.css` | Libadwaita interface and communication with Tokio |
| `src/telegram.rs` | Sessions, authentication, chats, history, updates, and downloads |
| `src/storage.rs` | Session storage and filenames received over the network |
| `src/model.rs`, `src/demo.rs` | UI models and demo data |

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo run --locked -- --demo
```

The client interface currently uses Russian labels. The checks below give the original labels for controls that may be hard to find.

In demo mode, check chat search, navigation in a narrow window, sending text, and saving `заметки.txt`. For Telegram checks, use a test account and a chat you control. Verify sign-in, reuse of the saved session, message exchange with a second client, and saving your own files from ordinary and protected chats.

**Notifications.** Launch the installed app with a test session in GNOME. Minimize the window, send a message from a second client, and click the notification. Verify that it opens the correct chat. Repeat with the chat active and then with its notifications muted. Both cases should be silent.

**Sign-in recovery.** Start the client offline with a saved test session. Reconnect and select **Повторить подключение** (Retry connection). The chat list should open without a new code. For a first sign-in with two-step verification, check that you can retry after entering an incorrect password and request a new code after password verification fails.

**Update recovery.** Open a test chat and disconnect only the machine running tgrust. From a second client, send a message and edit it. Reconnect with tgrust open. Verify that the edited message appears in history. Send another message from the second client and check that it arrives automatically.

**History.** Add more than 40 messages to a test chat. Load an older page while sending a new message from another client. The message should appear without a manual refresh. Then take tgrust offline, add more than 40 messages from the other client, and reconnect. Load older messages and check that the pages have no gaps. Switch chats and back while sending a message. Verify that the message appears once and drafts in other chats are preserved.

**Downloads.** Send a photo and a document of at least 12 MiB to a test chat you control. Save them in tgrust. Open the photo and compare the document's size and SHA-256 hash with the original. Repeat with saving restricted and with an existing destination file. Compare the existing file's SHA-256 before and after the attempt. If Telegram returns `FILE_REFERENCE_EXPIRED` or `FILE_REFERENCE_INVALID` during transfer, the client should get a fresh reference and download the same bytes. If the message or file becomes unavailable, the dialog should report the error and the temporary file should be removed.

**Uploads.** Attach a file in a test chat, switch away, and return. Check that the filename and caption are still in the draft. Send the file and compare its SHA-256 with the original on a second client. Send a JPEG via **Фотография** (Photo) and verify the image and caption. If Telegram rejects the upload, the attachment and caption should remain available to retry.

**Replies.** Right-click a message to open its actions. With the keyboard, focus a message with Tab and press Shift+F10 or Menu. Choose **Ответить** (Reply), send the reply, and verify its link to the original message on a second client. Repeat with an attachment and verify that you can cancel the reply before sending.

**Edits.** Send a plain-text message in a test chat and choose **Редактировать** (Edit). Save new text and check the **Изменено** (Edited) label on both clients. Close the editor without saving and verify that the original message and the separate compose draft are preserved.

**Deletion.** Send a test message, open the delete dialog, and cancel. Verify that the message remains in history. Then confirm deletion and check that it disappears on both clients and from the chat's latest-message preview.

**Forwarding.** Choose **Переслать** (Forward) for an ordinary message and select another test chat. On a second client, verify that the destination receives one message with a forwarded marker. Repeat with a protected message and verify that forwarding is unavailable or rejected by Telegram.

**Pins.** Choose **Закрепить** (Pin) in a test chat where you have permission to pin. Verify the marker on both clients. Unpin from the second client and check that the marker disappears from the open history.

**Reactions.** Add a standard emoji reaction in a test chat and compare the count on both clients. Choose the same emoji again to remove it. If your account supports multiple reactions, add two from the second client, remove one in tgrust, and verify that the other remains.

**Search.** Send an uncommon phrase in two test chats. Find it via **Поиск по сообщениям** (Search messages) and open a result outside the first page. Verify that the matching message is highlighted among its neighbors. Select **К последним сообщениям** (Back to latest messages) to return to the end of the history.

**Photo viewer.** Open **Пробный кадр** (Sample photo) in demo mode. In a test chat, open an ordinary photo and a spoiler photo. The spoiler preview should open after you click it. Timed photos should have no preview button.

**Avatars and thumbnails.** Open a test conversation and a group with a profile photo. Send a photo and a video with a cover, then switch chats. Verify that the correct thumbnails and avatars are still shown for the messages. Check that a participant without a profile photo has initials.

**Archive.** Archive a test chat using the header button. Check that it appears in **Архив чатов** (Chat archive). Restore it and verify both moves on the second client.

**Folders.** On a second client, configure a folder with explicitly included and excluded chats, a pinned chat, and a filter by chat type. Select the folder in tgrust and check membership and order among the loaded chats. Change the folder on the second client and verify that the list updates.

## Roadmap

1. Stabilize sign-in, reconnection, and history sync. Add test coverage for large files and expired file references.
2. Expand media sending and previews, replies, edits, reactions, notifications, and global search.
3. Extend folder support and add multiple accounts, settings, and Flatpak packaging.
4. Evaluate calls, stories, secret chats, and other Telegram Desktop features.

See the [English developer and agent guide](AGENTS.en.md) or the [canonical agent instructions in Russian](AGENTS.md).
