use anyhow::{Context, Result, bail, ensure};
use grammers_client::tl::functions::channels::ToggleUsername;
use tokio::time::timeout;

use super::create_chat::{RPC_TIMEOUT, accepted, username_error};
use super::{PeerKind, PeerRef, Telegram, enums};
use crate::model::{AttachedAddress, ChatAddressChange, ChatAddresses};

impl Telegram {
    pub async fn edit_chat_addresses(
        &self,
        id: i64,
        original: ChatAddresses,
        proposed: ChatAddresses,
    ) -> Result<()> {
        let _serial = self.dialog_settings.lock().await;
        let reference = self.peer(id).await?;
        let current = self.owned_chat_addresses(reference).await?;
        let changes = proposed.changes(&original, &current)?;
        if changes.is_empty() {
            return Ok(());
        }
        for change in changes {
            let result = match change {
                ChatAddressChange::Primary(username) => {
                    self.update_channel_username(reference, &username).await
                }
                ChatAddressChange::Alias(address) => {
                    self.toggle_channel_username(reference, &address).await
                }
            };
            // Even a missing acknowledgement may follow an applied change.
            self.invalidate_views();
            result?;
        }
        let saved = self.owned_chat_addresses(reference).await?;
        ensure!(
            proposed.changes(&original, &saved)?.is_empty(),
            "Telegram не подтвердил все изменения адресов. Повторите сохранение"
        );
        Ok(())
    }

    async fn owned_chat_addresses(&self, reference: PeerRef) -> Result<ChatAddresses> {
        ensure!(
            reference.id.kind() == PeerKind::Channel,
            "У этого чата нет публичных адресов"
        );
        let full = timeout(RPC_TIMEOUT, self.full_chat(reference))
            .await
            .context("Telegram не прислал адреса чата вовремя")??;
        self.chat_profile_view(reference, full.chats)?
            .addresses
            .context("Telegram не разрешает менять адреса этого чата")
    }

    async fn toggle_channel_username(
        &self,
        reference: PeerRef,
        address: &AttachedAddress,
    ) -> Result<()> {
        let result = timeout(
            RPC_TIMEOUT,
            self.client.invoke(&ToggleUsername {
                channel: reference.into(),
                username: address.name.clone(),
                active: address.active,
            }),
        )
        .await;
        if accepted(&result, &["USERNAME_NOT_MODIFIED", "CHAT_NOT_MODIFIED"]) {
            return Ok(());
        }
        match result {
            Ok(Err(error)) if error.is("USERNAMES_ACTIVE_TOO_MUCH") => {
                bail!("Достигнут лимит активных имён. Отключите одно из них и сохраните изменения.")
            }
            Ok(Err(error)) => bail!("{}", username_error(error)),
            Ok(Ok(false)) => bail!("Telegram не подтвердил изменение коллекционного имени."),
            Ok(Ok(true)) => Ok(()),
            Err(_) => bail!("Telegram не подтвердил коллекционное имя вовремя."),
        }
    }
}

pub(super) fn channel_addresses(
    primary: Option<&str>,
    list: Option<&[enums::Username]>,
) -> ChatAddresses {
    let list = list.unwrap_or_default();
    let primary = primary
        .filter(|name| !name.is_empty())
        .or_else(|| {
            list.iter().find_map(|enums::Username::Username(name)| {
                (name.editable && !name.deletable && !name.username.is_empty())
                    .then_some(name.username.as_str())
            })
        })
        .unwrap_or_default();
    let mut aliases = Vec::<AttachedAddress>::new();
    for enums::Username::Username(address) in list {
        if !address.username.is_empty()
            && !address.username.eq_ignore_ascii_case(primary)
            && !aliases
                .iter()
                .any(|known| known.name.eq_ignore_ascii_case(&address.username))
        {
            aliases.push(AttachedAddress {
                name: address.username.clone(),
                active: address.active,
                can_toggle: !address.deletable,
            });
        }
    }
    ChatAddresses {
        primary: primary.to_owned(),
        aliases,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_client::tl::types;

    #[test]
    fn channel_addresses_keep_dormant_aliases_and_the_editable_username() {
        let make = |name: &str, editable, active, deletable| {
            types::Username {
                username: name.into(),
                editable,
                active,
                deletable,
                expired: false,
            }
            .into()
        };
        let addresses = channel_addresses(
            None,
            Some(&[
                make("DemoCollectible", false, true, false),
                make("demo_current", true, true, false),
                make("demo_standby", false, false, false),
                make("democollectible", false, true, false),
                make("demo_view_only", false, true, true),
                make("demo_additional", true, true, false),
            ]),
        );
        assert_eq!(addresses.primary, "demo_current");
        assert_eq!(
            addresses.aliases,
            [
                AttachedAddress {
                    name: "DemoCollectible".into(),
                    active: true,
                    can_toggle: true,
                },
                AttachedAddress {
                    name: "demo_standby".into(),
                    active: false,
                    can_toggle: true,
                },
                AttachedAddress {
                    name: "demo_view_only".into(),
                    active: true,
                    can_toggle: false,
                },
                AttachedAddress {
                    name: "demo_additional".into(),
                    active: true,
                    can_toggle: true,
                },
            ]
        );
        let old = channel_addresses(Some("demo_original"), None);
        assert_eq!(old.primary, "demo_original");
        assert!(old.aliases.is_empty());
    }
}
