use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use anyhow::Result;

/// Orders read receipts and manual markers, including already queued history requests.
#[derive(Default)]
pub struct ReadState {
    chats: Mutex<HashMap<i64, ChatReadState>>,
}

#[derive(Default)]
struct ChatReadState {
    revision: u64,
    may_be_marked: bool,
    operations: Arc<tokio::sync::Mutex<()>>,
}

impl ReadState {
    pub fn may_be_marked(&self, chat: i64) -> bool {
        self.chats
            .lock()
            .unwrap()
            .get(&chat)
            .is_some_and(|state| state.may_be_marked)
    }

    pub fn set_may_be_marked(&self, chat: i64, marked: bool) {
        self.chats
            .lock()
            .unwrap()
            .entry(chat)
            .or_default()
            .may_be_marked = marked;
    }
    pub fn revision(&self, chat: i64) -> u64 {
        self.chats
            .lock()
            .unwrap()
            .get(&chat)
            .map_or(0, |state| state.revision)
    }

    pub fn advance(&self, chat: i64) -> u64 {
        let mut chats = self.chats.lock().unwrap();
        let state = chats.entry(chat).or_default();
        state.revision = state.revision.wrapping_add(1);
        state.revision
    }

    pub async fn run<T>(
        &self,
        chat: i64,
        revision: u64,
        operation: impl Future<Output = Result<T>>,
    ) -> Result<Option<T>> {
        let operations = self
            .chats
            .lock()
            .unwrap()
            .entry(chat)
            .or_default()
            .operations
            .clone();
        let _guard = operations.lock().await;
        if self.revision(chat) != revision {
            return Ok(None);
        }
        operation.await.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_failed_marker_response_is_reconciled_on_explicit_reopen() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let state = ReadState::default();
            let revision = state.advance(1);
            let result: Result<Option<()>> = state
                .run(1, revision, async {
                    state.set_may_be_marked(1, true);
                    anyhow::bail!("Synthetic lost response")
                })
                .await;
            assert!(result.is_err());
            let revision = state.advance(1);
            assert_eq!(
                state
                    .run(1, revision, async {
                        let clear = state.may_be_marked(1);
                        state.set_may_be_marked(1, false);
                        Ok(clear)
                    })
                    .await
                    .unwrap(),
                Some(true)
            );
        });
    }

    #[test]
    fn reopening_clears_a_marker_even_when_its_rpc_finishes_after_the_reopen() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let state = Arc::new(ReadState::default());
            let manual_revision = state.advance(1);
            let (started, entered) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let manual = {
                let state = state.clone();
                tokio::spawn(async move {
                    state
                        .run(1, manual_revision, async {
                            state.set_may_be_marked(1, true);
                            started.send(()).unwrap();
                            released.await.unwrap();
                            Ok(())
                        })
                        .await
                        .unwrap()
                })
            };
            entered.await.unwrap();
            let reopened = state.advance(1);
            let read = {
                let state = state.clone();
                tokio::spawn(async move {
                    state
                        .run(1, reopened, async {
                            let clear = state.may_be_marked(1);
                            state.set_may_be_marked(1, false);
                            Ok(clear)
                        })
                        .await
                        .unwrap()
                })
            };
            release.send(()).unwrap();
            assert_eq!(manual.await.unwrap(), Some(()));
            assert_eq!(read.await.unwrap(), Some(true));
            assert!(!state.may_be_marked(1));
        });
    }

    #[test]
    fn in_flight_receipt_finishes_before_manual_marker_and_stale_receipts_are_skipped() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let state = Arc::new(ReadState::default());
            let events = Arc::new(Mutex::new(Vec::new()));
            let (started, entered) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let old = {
                let state = state.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    state
                        .run(1, 0, async move {
                            started.send(()).unwrap();
                            released.await.unwrap();
                            events.lock().unwrap().push("read");
                            Ok(())
                        })
                        .await
                        .unwrap()
                })
            };
            entered.await.unwrap();
            assert_eq!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    state.run(2, 0, async { Ok(()) })
                )
                .await
                .expect("Another chat must not wait for this read receipt")
                .unwrap(),
                Some(())
            );
            let revision = state.advance(1);
            assert_eq!(state.revision(2), 0);
            let manual = {
                let state = state.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    state
                        .run(1, revision, async move {
                            events.lock().unwrap().push("unread");
                            Ok(())
                        })
                        .await
                        .unwrap()
                })
            };
            let stale = {
                let state = state.clone();
                tokio::spawn(async move {
                    state
                        .run(1, 0, async {
                            panic!("Stale history must not send a read receipt");
                            #[allow(unreachable_code)]
                            Ok(())
                        })
                        .await
                        .unwrap()
                })
            };
            release.send(()).unwrap();
            assert_eq!(old.await.unwrap(), Some(()));
            assert_eq!(manual.await.unwrap(), Some(()));
            assert_eq!(stale.await.unwrap(), None);
            assert_eq!(*events.lock().unwrap(), ["read", "unread"]);
        });
    }

    #[test]
    fn explicitly_reopening_a_chat_supersedes_an_older_manual_action() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let state = ReadState::default();
            let manual = state.advance(1);
            let reopened = state.advance(1);
            assert_eq!(
                state
                    .run(1, manual, async {
                        anyhow::bail!("Obsolete marker must not be sent")
                    })
                    .await
                    .unwrap(),
                None::<()>
            );
            assert_eq!(
                state.run(1, reopened, async { Ok("read") }).await.unwrap(),
                Some("read")
            );
        });
    }
}
