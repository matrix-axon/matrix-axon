//! Persist matrix-rust-sdk's calculated room display name (ADR 0118).
//!
//! The SDK recomputes a room's display name on every sync response that
//! touches the room and caches it on `RoomInfo`. Axon copies that value onto
//! `room_summaries.display_name` rather than reimplementing the spec's naming
//! algorithm. The ADR 0111 member-count reconciler drives this per room, so
//! the hints and the bounded sweep that already visit every room cover it too.
use axon_core::{LiveFrame, RoomDisplayNameFrame};
use axon_store::{DisplayNameWrite, Store};
use matrix_sdk::{ruma::RoomId, Room, RoomState};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Hero display names are user-controlled; bound what one room can store.
const MAX_DISPLAY_NAME_CHARS: usize = 512;

/// Persist one joined room's display name. Returns `true` when the caller
/// should retry: the room has no summary row yet, or the read or write failed.
pub(crate) async fn reconcile(
    room: &Room,
    store: &Store,
    account_id: Uuid,
    live_tx: &broadcast::Sender<LiveFrame>,
) -> bool {
    // Left and invited rooms have no listed summary to name.
    if room.state() != RoomState::Joined {
        return false;
    }
    let room_id = room.room_id();
    let display_name = match room.display_name().await {
        Ok(display_name) => display_name.to_string(),
        Err(error) => {
            tracing::warn!(%account_id, %room_id, source = "sdk_display_name", %error, "room display name read failed; will retry");
            return true;
        }
    };
    persist(&display_name, store, account_id, room_id, live_tx).await
}

async fn persist(
    display_name: &str,
    store: &Store,
    account_id: Uuid,
    room_id: &RoomId,
    live_tx: &broadcast::Sender<LiveFrame>,
) -> bool {
    let display_name = bounded(display_name);
    match store
        .set_room_display_name(account_id, room_id.as_str(), display_name)
        .await
    {
        Ok(DisplayNameWrite::Applied) => {
            tracing::trace!(%account_id, %room_id, source = "sdk_display_name", "stored room display name");
            // No subscriber is not an error; the next list load reads the row.
            let _ = live_tx.send(LiveFrame::RoomDisplayNameChanged(RoomDisplayNameFrame {
                account_id,
                room_id: room_id.to_string(),
                display_name: display_name.to_owned(),
            }));
            false
        }
        Ok(DisplayNameWrite::Unchanged) => false,
        Ok(DisplayNameWrite::Missing) => {
            tracing::trace!(%account_id, %room_id, source = "sdk_display_name", "room has no summary yet; will retry");
            true
        }
        Err(error) => {
            tracing::warn!(%account_id, %room_id, source = "sdk_display_name", reason = error.diagnostic_reason(), "room display name persistence failed; will retry");
            true
        }
    }
}

/// `name` cut to at most [`MAX_DISPLAY_NAME_CHARS`] characters, on a
/// character boundary.
fn bounded(name: &str) -> &str {
    match name.char_indices().nth(MAX_DISPLAY_NAME_CHARS) {
        Some((end, _)) => &name[..end],
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::room_id;

    #[test]
    fn bounded_cuts_on_a_character_boundary() {
        assert_eq!(bounded("Alice, Bob"), "Alice, Bob");
        let long = "é".repeat(MAX_DISPLAY_NAME_CHARS + 10);
        let cut = bounded(&long);
        assert_eq!(cut.chars().count(), MAX_DISPLAY_NAME_CHARS);
        assert!(long.starts_with(cut));
        let exact = "a".repeat(MAX_DISPLAY_NAME_CHARS);
        assert_eq!(bounded(&exact), exact);
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn persist_writes_changes_once_and_retries_a_missing_summary() {
        let store = Store::connect(&std::env::var("DATABASE_URL").unwrap(), 5)
            .await
            .unwrap();
        let account = store
            .upsert_account(
                &format!("@names-{}:localhost", Uuid::new_v4()),
                "https://hs.example.org",
            )
            .await
            .unwrap();
        let (live_tx, mut live_rx) = broadcast::channel(8);
        let room = room_id!("!names:localhost");

        assert!(
            persist("Alice, Bob", &store, account.account_id, room, &live_tx).await,
            "no summary row yet must ask for a retry"
        );
        assert!(live_rx.try_recv().is_err());

        sqlx_core::query::query("INSERT INTO room_summaries (account_id, room_id, last_activity_ts, last_event_id, last_event_row_id, last_activity_is_content) VALUES ($1, $2, 1, '$fixture', 1, false)")
            .bind(account.account_id).bind(room.as_str()).execute(store.pool()).await.unwrap();
        assert!(!persist("Alice, Bob", &store, account.account_id, room, &live_tx).await);
        match live_rx.try_recv().unwrap() {
            LiveFrame::RoomDisplayNameChanged(frame) => {
                assert_eq!(frame.account_id, account.account_id);
                assert_eq!(frame.room_id, room.as_str());
                assert_eq!(frame.display_name, "Alice, Bob");
            }
            other => panic!("expected RoomDisplayNameChanged, got {other:?}"),
        }

        // An unchanged value is neither rewritten nor re-announced.
        assert!(!persist("Alice, Bob", &store, account.account_id, room, &live_tx).await);
        assert!(live_rx.try_recv().is_err());

        assert!(!persist("Alice, Carol", &store, account.account_id, room, &live_tx).await);
        assert!(matches!(
            live_rx.try_recv().unwrap(),
            LiveFrame::RoomDisplayNameChanged(frame) if frame.display_name == "Alice, Carol"
        ));
        let listed = store.list_rooms(Some(account.account_id)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].display_name.as_deref(), Some("Alice, Carol"));
        store.delete_account_row(account.account_id).await.unwrap();
    }
}
