//! Composition-root adapter: binds `axon-sync`'s [`LinkedEventReader`] to
//! `axon-api`'s linked-event port (ADR 0138 section 3).

use async_trait::async_trait;
use axon_api::{FetchedEvent, LinkedEventError, LinkedEventSource};
use axon_sync::{LinkedEvent, LinkedEventError as SyncLinkedEventError, LinkedEventReader};
use uuid::Uuid;

/// Wraps the sync engine's reader so it satisfies the API port. The orphan
/// rule requires a local newtype.
pub struct LinkedEventAdapter(pub LinkedEventReader);

fn map_err(err: SyncLinkedEventError) -> LinkedEventError {
    match err {
        SyncLinkedEventError::NotFound => LinkedEventError::NotFound("event not found".to_owned()),
        SyncLinkedEventError::UnknownAccount(id) => {
            LinkedEventError::NotFound(format!("account {id} not found"))
        }
        SyncLinkedEventError::Invalid(msg) => LinkedEventError::Invalid(msg),
        SyncLinkedEventError::NotActive(id) => LinkedEventError::NotActive(format!(
            "account {id} is not active; log in before reading linked events"
        )),
        SyncLinkedEventError::NotConnected(id) => {
            LinkedEventError::Unavailable(format!("account {id} is not connected"))
        }
        SyncLinkedEventError::Busy => {
            LinkedEventError::Unavailable("too many linked-event reads in flight".to_owned())
        }
        SyncLinkedEventError::TimedOut => {
            LinkedEventError::Timeout("the homeserver did not answer in time".to_owned())
        }
        // The detail (which can name the homeserver's URL) is logged by the
        // reader; the client gets a fixed message, as on the 500 path.
        SyncLinkedEventError::Upstream(_) => {
            LinkedEventError::Upstream("the homeserver could not serve this event".to_owned())
        }
        // Logged, with its ids, by the reader.
        SyncLinkedEventError::Store(_) => LinkedEventError::Internal,
    }
}

fn map_event(event: LinkedEvent) -> FetchedEvent {
    FetchedEvent {
        event_id: event.event_id,
        room_id: event.room_id,
        sender: event.sender,
        origin_ts: event.origin_ts,
        event_type: event.event_type,
        content: event.content,
        body: event.body,
        relates_to: event.relates_to,
        redacted: event.redacted,
        edited: event.edited,
        sender_trust: event.sender_trust,
    }
}

#[async_trait]
impl LinkedEventSource for LinkedEventAdapter {
    async fn read(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<FetchedEvent, LinkedEventError> {
        self.0
            .read(account_id, room_id, event_id)
            .await
            .map(map_event)
            .map_err(map_err)
    }
}
