# Agent instructions for tgrust

[Русский](AGENTS.md) · English

This is the English translation of [AGENTS.md](AGENTS.md).

## Goals and status

Develop an unofficial Telegram client in Rust for GNOME and Wayland. Verify advertised features against the code and tests. Update both README versions when launch instructions, dependencies, limitations, or features change. Update this translation when the agent instructions change.

## Code

- Keep GTK widgets and UI state on the GLib main thread. Run network requests and downloads in Tokio. Pass models from `src/model.rs` to the UI.
- Use grammers and Telegram types for MTProto. When updating grammers, check all its package versions together and review changes to the Telegram schema.
- When switching chats, make sure responses to earlier requests reach the correct chat. Preserve drafts and prevent duplicate messages.
- For downloads, use the local path chosen by the user and write to a temporary file in the same directory. Finalize without overwriting existing files. Use the Telegram filename as a suggestion in the save dialog.
- Respect the separate restrictions on self-destructing messages. Downloads use the account's usual authorization and message data that the account can access.
- Preserve the current behavior for sponsored messages and manual saving of accessible attachments. Update the API limitations in both README versions if this behavior changes.
- Keep the UI usable with a keyboard and in narrow windows. Use standard Libadwaita components and system colors.

## Sessions and verification

- Keep phone numbers, sign-in codes, two-step verification passwords, API hashes, session files, and conversations out of commits. Use fictional data in demo mode, tests, logs, and screenshots.
- Before pushing a commit, run `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, and `git diff --check`.
- Test UI changes with `cargo run --locked -- --demo`. For network changes, record what you verified with a test account. Automated checks run without a Telegram account.
- Keep `Cargo.lock` in the repository and update it alongside dependency changes.

## GitHub

- The `skae798/tgrust` repository is public. Change its visibility only at the owner's explicit request.
- Use a short branch from `main` for each separate task. Describe the scenarios you verified and any remaining limitations in the pull request. Link a related issue when one exists.
- Keep `main` buildable and preserve published history.
