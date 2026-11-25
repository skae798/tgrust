use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};
use grammers_client::tl::functions::messages::GetPollVotes;

use super::{Media, Peer, PeerId, Telegram, enums, peer_author, poll_view};
use crate::model::{POLL_VOTERS_PAGE_SIZE, Poll, PollVoter, PollVoterPage};

impl Telegram {
    pub async fn poll_voters(
        &self,
        chat: i64,
        message: i32,
        expected: i64,
        option: Option<Vec<u8>>,
        offset: Option<String>,
    ) -> Result<PollVoterPage> {
        let peer = self.peer(chat).await?;
        let item = self
            .message_by_id(peer, message)
            .await?
            .context("Опрос больше недоступен")?;
        let Some(Media::Poll(raw)) = item.media() else {
            bail!("Опрос больше недоступен");
        };
        let poll = poll_view(&raw);
        ensure!(poll.id == expected, "Опрос больше недоступен");
        ensure!(
            !item.post()
                && !matches!(item.peer(), Some(Peer::Channel(_)))
                && poll.can_list_voters(),
            "Список проголосовавших для этого опроса недоступен"
        );
        ensure!(
            option
                .as_ref()
                .is_none_or(|option| poll.choices.iter().any(|choice| &choice.option == option)),
            "Этот вариант ответа больше недоступен"
        );
        let response = self
            .client
            .invoke(&GetPollVotes {
                peer: peer.into(),
                id: message,
                option: option.clone(),
                offset,
                limit: POLL_VOTERS_PAGE_SIZE,
            })
            .await;
        let enums::messages::VotesList::List(raw) = match response {
            Err(error) if error.is("POLL_VOTE_REQUIRED") => {
                bail!("Telegram просит сначала проголосовать")
            }
            Err(error) if error.is("BROADCAST_FORBIDDEN") => {
                bail!("Telegram ограничил доступ к списку проголосовавших")
            }
            result => result?,
        };
        let peers = self.peers_from_entities(raw.users, raw.chats);
        let mut refs = HashMap::new();
        let mut voters = Vec::with_capacity(raw.votes.len());
        for vote in raw.votes {
            let (id, answers) = vote_answers(vote, &poll, option.as_deref());
            let Some(peer_id) = id.bot_api_dialog_id() else {
                continue;
            };
            let mut author = peers.get(&id).and_then(peer_author);
            if let Some(profile) = author.as_mut()
                && profile.avatar.is_some()
            {
                let reference = match peers.get(&id) {
                    Some(peer) => peer.to_ref().await.ok().flatten(),
                    None => None,
                };
                if let Some(reference) = reference {
                    refs.insert(peer_id, reference);
                } else {
                    profile.avatar = None;
                }
            }
            let (name, avatar) = author.map_or_else(
                || ("Недоступный профиль".into(), None),
                |author| (author.name, author.avatar),
            );
            voters.push(PollVoter {
                peer: peer_id,
                name,
                avatar,
                answers,
            });
        }
        if !refs.is_empty() {
            self.peers.write().await.extend(refs);
        }
        Ok(PollVoterPage {
            voters,
            total: raw.count.max(0),
            next: raw.next_offset.filter(|offset| !offset.is_empty()),
        })
    }
}

fn vote_answers(
    vote: enums::MessagePeerVote,
    poll: &Poll,
    filter: Option<&[u8]>,
) -> (PeerId, Vec<String>) {
    let (peer, options) = match vote {
        enums::MessagePeerVote::Vote(vote) => (vote.peer, vec![vote.option]),
        enums::MessagePeerVote::Multiple(vote) => (vote.peer, vote.options),
        enums::MessagePeerVote::InputOption(vote) => {
            (vote.peer, filter.into_iter().map(<[u8]>::to_vec).collect())
        }
    };
    let answers = poll
        .choices
        .iter()
        .filter(|choice| options.contains(&choice.option))
        .map(|choice| choice.label.clone())
        .collect();
    (PeerId::from(peer), answers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PollDraft;
    use crate::telegram::types;

    #[test]
    fn vote_conversion_preserves_multiple_answers_and_filters_input_option() {
        let poll = PollDraft {
            question: "Когда провести встречу?".into(),
            choices: vec!["Утром".into(), "Днём".into(), "Вечером".into()],
            ..PollDraft::default()
        }
        .into_poll(30);
        let peer: enums::Peer = types::PeerChannel { channel_id: 47 }.into();
        let multiple = types::MessagePeerVoteMultiple {
            peer: peer.clone(),
            options: vec![vec![2], vec![7], vec![0], vec![2]],
            date: 100,
        }
        .into();
        let (id, answers) = vote_answers(multiple, &poll, None);
        assert_eq!(id, PeerId::from(peer.clone()));
        assert_eq!(answers, ["Утром", "Вечером"]);
        let single = types::MessagePeerVote {
            peer: peer.clone(),
            option: vec![1],
            date: 100,
        }
        .into();
        assert_eq!(vote_answers(single, &poll, None).1, ["Днём"]);
        let implicit: enums::MessagePeerVote =
            types::MessagePeerVoteInputOption { peer, date: 100 }.into();
        assert!(vote_answers(implicit.clone(), &poll, None).1.is_empty());
        assert_eq!(vote_answers(implicit, &poll, Some(&[1])).1, ["Днём"]);
    }
}
