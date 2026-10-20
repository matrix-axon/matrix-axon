//! Turn a sliding-sync room update into a push observation.
//!
//! Push actions are read here, from `TimelineEvent::push_actions`, because the
//! event-handler path collapses `None` into an empty slice. The subscription
//! is installed before sync starts. A lagged broadcast unseeds the account so
//! the next observation does not invent a rise.
//!
//! This task does not read unread counts. The unread watcher is the only
//! writer of that pair, and it emits the count observation when the pair
//! changes. A message observation leaves `after` unset. On a seeded room that
//! is the equals path, so a held increase cannot look like a fall.

use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::ruma::push::Action;
use matrix_sdk_base::sync::RoomUpdates;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::message::{consider_event, PushClass, PushMessage};
use super::router::{Observation, PushIngress};

/// What one account's room-update reader needs.
pub(crate) struct RoomWatch {
    pub account_id: Uuid,
    pub own_user_id: String,
    pub push: PushIngress,
    pub cancel: CancellationToken,
}

pub(crate) async fn watch_room_updates(
    mut updates: broadcast::Receiver<RoomUpdates>,
    watch: RoomWatch,
) {
    loop {
        let batch = tokio::select! {
            _ = watch.cancel.cancelled() => return,
            batch = updates.recv() => batch,
        };
        match batch {
            Ok(batch) => {
                for (room_id, update) in batch.joined {
                    let messages = messages_of(&update.timeline.events, &watch.own_user_id);
                    if messages.is_empty() {
                        continue;
                    }
                    watch.push.observe(Observation {
                        account_id: watch.account_id,
                        room_id: room_id.to_string(),
                        after: None,
                        messages,
                    });
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                tracing::warn!(
                    account_id = %watch.account_id,
                    "push room-update subscription lagged; the next observation seeds"
                );
                watch.push.unseed_account(watch.account_id);
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn messages_of(events: &[TimelineEvent], own_user_id: &str) -> Vec<PushMessage> {
    events
        .iter()
        .filter_map(|event| {
            let class = match event.push_actions() {
                None => PushClass::Unknown,
                Some(actions)
                    if actions
                        .iter()
                        .any(|action| matches!(action, Action::Notify)) =>
                {
                    PushClass::Notify
                }
                Some(_) => PushClass::Silent,
            };
            let value: Value = serde_json::from_str(event.raw().json().get()).ok()?;
            consider_event(&value, own_user_id, class)
        })
        .collect()
}

pub(crate) fn count_observation(account_id: Uuid, room_id: &str, pair: (u64, u64)) -> Observation {
    Observation {
        account_id,
        room_id: room_id.to_owned(),
        after: Some(pair),
        messages: Vec::new(),
    }
}
