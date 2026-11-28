use std::time::Duration;

use anyhow::{Context, Result, ensure};
use grammers_client::tl::functions::{
    channels::{ToggleForum, TogglePreHistoryHidden},
    help::GetAppConfig,
};
use tokio::time::timeout;

use super::create_chat::RPC_TIMEOUT;
use super::{PeerRef, Telegram, enums, types};
use crate::model::ForumMode;

struct ForumState {
    mode: ForumMode,
    members: Option<i32>,
    tabs: bool,
    sidebar: bool,
}

impl Telegram {
    pub async fn set_forum_mode(&self, chat: i64, confirmed: ForumMode) -> Result<bool> {
        let _serial = self.dialog_settings.lock().await;
        let peer = self.forum_peer(chat).await?;
        let desired = !confirmed.enabled;
        let minimum = if desired {
            timeout(
                Duration::from_secs(4),
                self.client.invoke(&GetAppConfig { hash: 0 }),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(forum_upgrade_min_members)
        } else {
            None
        };
        let current = self.forum_mode_snapshot(peer).await?;
        if !current.mode.needs_change(confirmed)? {
            return Ok(desired);
        }
        if let Some((minimum, count)) = minimum.zip(current.members) {
            ensure!(
                count >= minimum,
                "Для включения тем нужно минимум {minimum} участников"
            );
        }
        if desired && current.mode.hidden_history {
            let visibility = timeout(
                RPC_TIMEOUT,
                self.client.invoke(&TogglePreHistoryHidden {
                    channel: peer.into(),
                    enabled: false,
                }),
            )
            .await;
            self.invalidate_views();
            let latest = self.forum_mode_snapshot(peer).await.context(
                "Не удалось проверить видимость истории. Проверьте настройки группы перед повтором",
            )?;
            if latest.mode.hidden_history {
                visibility.context("Telegram не подтвердил открытие истории вовремя")??;
                anyhow::bail!("Telegram не открыл историю для новых участников");
            }
            let change = async {
                if !latest.mode.needs_change(confirmed)? {
                    return Ok(desired);
                }
                self.toggle_forum(chat, peer, latest, desired).await
            }
            .await;
            return change
                .context("История открыта новым участникам, но включение тем не подтверждено");
        }
        self.toggle_forum(chat, peer, current, desired).await
    }

    async fn toggle_forum(
        &self,
        chat: i64,
        peer: PeerRef,
        current: ForumState,
        enabled: bool,
    ) -> Result<bool> {
        let result = timeout(
            RPC_TIMEOUT,
            self.client.invoke(&ToggleForum {
                channel: peer.into(),
                enabled,
                tabs: enabled && current.tabs,
                sidebar: enabled && current.sidebar,
            }),
        )
        .await;
        self.invalidate_history(Some(chat), true);
        self.invalidate_views();
        let readback = self.forum_mode_snapshot(peer).await;
        if readback
            .as_ref()
            .is_ok_and(|state| state.mode.enabled == enabled)
        {
            return Ok(enabled);
        }
        if let Err(error) = result.context("Telegram не подтвердил изменение режима вовремя")?
            && !error.is("CHAT_NOT_MODIFIED")
        {
            return Err(error.into());
        }
        readback?;
        anyhow::bail!("Telegram не подтвердил изменение режима тем")
    }

    async fn forum_mode_snapshot(&self, peer: PeerRef) -> Result<ForumState> {
        let result = timeout(RPC_TIMEOUT, self.full_chat(peer))
            .await
            .context("Telegram не прислал настройки супергруппы вовремя")??;
        let enums::ChatFull::ChannelFull(full) = result.full_chat else {
            anyhow::bail!("Темы доступны в супергруппах")
        };
        let channel = result
            .chats
            .into_iter()
            .find_map(|chat| match chat {
                enums::Chat::Channel(channel) if Some(channel.id) == peer.id.bare_id() => {
                    Some(channel)
                }
                _ => None,
            })
            .context("Супергруппа недоступна")?;
        let mut mode = owner_forum_mode(&channel)
            .context("Только владелец супергруппы может изменить режим тем")?;
        mode.linked_discussion = full.linked_chat_id.is_some();
        mode.hidden_history = full.hidden_prehistory;
        Ok(ForumState {
            mode,
            members: full.participants_count.or(channel.participants_count),
            tabs: channel.forum_tabs,
            sidebar: channel.forum_sidebar,
        })
    }
}

pub(super) fn owner_forum_mode(channel: &types::Channel) -> Option<ForumMode> {
    (channel.creator
        && channel.megagroup
        && !channel.broadcast
        && !channel.gigagroup
        && !channel.monoforum
        && !channel.min
        && !channel.left)
        .then_some(ForumMode {
            enabled: channel.forum,
            linked_discussion: false,
            hidden_history: false,
        })
}

fn forum_upgrade_min_members(config: enums::help::AppConfig) -> Option<i32> {
    let enums::help::AppConfig::Config(config) = config else {
        return None;
    };
    let enums::Jsonvalue::JsonObject(object) = config.config else {
        return None;
    };
    object.value.into_iter().find_map(|field| {
        let enums::JsonobjectValue::JsonObjectValue(field) = field;
        if field.key != "forum_upgrade_participants_min" {
            return None;
        }
        let enums::Jsonvalue::JsonNumber(value) = field.value else {
            return None;
        };
        let value = value.value;
        (value.fract() == 0.0 && (0.0..=f64::from(i32::MAX)).contains(&value))
            .then_some(value as i32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forum_minimum_comes_from_a_valid_server_value() {
        let config = |key: &str, value| {
            types::help::AppConfig {
                hash: 1,
                config: types::JsonObject {
                    value: vec![
                        types::JsonObjectValue {
                            key: key.into(),
                            value: types::JsonNumber { value }.into(),
                        }
                        .into(),
                    ],
                }
                .into(),
            }
            .into()
        };
        let key = "forum_upgrade_participants_min";
        assert_eq!(forum_upgrade_min_members(config(key, 25.0)), Some(25));
        assert_eq!(
            forum_upgrade_min_members(config("another_limit", 25.0)),
            None
        );
        assert_eq!(forum_upgrade_min_members(config(key, -1.0)), None);
        assert_eq!(forum_upgrade_min_members(config(key, 25.5)), None);
    }
}
