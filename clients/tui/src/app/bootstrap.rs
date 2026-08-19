//! Non-blocking startup (issue #189).
//!
//! The TUI used to run five sequential `await`s — accounts, rooms, read
//! markers, the first timeline, drafts — on the main task *before* the first
//! `terminal.draw`. Nothing was painted and no key was handled until all five
//! finished, so connecting to a server with thousands of rooms looked like a
//! hang rather than a load.
//!
//! Startup is now a chain of stages driven by [`BootstrapOutcome`]s: each
//! stage's network work is spawned, its result comes back through the main
//! loop's channel, and the loop draws between every stage. The ordering the
//! stages need is preserved by construction — a stage is only spawned from the
//! previous stage's handler — rather than by five awaits in a row:
//!
//! 1. **Accounts.** Nothing else can be account-scoped until these land.
//! 2. **Rooms.** Needs the account list to drop rooms belonging to logged-out
//!    accounts, and it is what picks the launch room.
//! 3. **Device state** (read markers *and* drafts, one concurrent batch).
//! 4. **The launch room's timeline**, awaited on the main task once markers are
//!    applied.
//!
//! Step 4 stays an await deliberately. Read markers must be in place *before*
//! the first `load_selected_timeline`, or the marker that call fabricates wins
//! the monotonic merge and permanently discards the real one (ADR 0048/0089).
//! It is one bounded request for one room's page — it does not grow with the
//! room count, and it is the same await every room switch already performs.
//!
//! The [`BootstrapOutcome::Rooms`] stage doubles as the post-startup room
//! refresh: a live frame for an unknown room asks for one, and requests are
//! coalesced so a backlog of such frames cannot stack up N full room fetches.

use std::future::Future;

use uuid::Uuid;

use crate::api::{AccountDto, ApiError, AxonClient, DeviceStateDto, RoomDto};

use super::drafts::DRAFTS_NAMESPACE;
use super::read_markers::READ_MARKERS_NAMESPACE;
use super::{App, Status};

/// One merged device-state read per account, for a single namespace.
pub(crate) type DeviceStateReads = Vec<(Uuid, Result<DeviceStateDto, ApiError>)>;

/// A completed startup stage, applied by the main loop.
pub(crate) enum BootstrapOutcome {
    Accounts(Result<Vec<AccountDto>, ApiError>),
    /// Also delivers post-startup refreshes; see the module docs.
    Rooms(Result<Vec<RoomDto>, ApiError>),
    DeviceState {
        markers: DeviceStateReads,
        drafts: DeviceStateReads,
    },
}

/// How far startup has got. Rendered in the rooms-panel title so a slow first
/// connect shows progress rather than an empty list that reads as "no rooms".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootstrapStage {
    Accounts,
    Rooms,
    DeviceState,
    Done,
}

impl BootstrapStage {
    /// The label for the rooms-panel title, or `None` once startup is complete.
    pub(crate) fn label(self) -> Option<&'static str> {
        match self {
            Self::Accounts => Some("connecting"),
            Self::Rooms => Some("loading rooms"),
            Self::DeviceState => Some("loading read state"),
            Self::Done => None,
        }
    }

    pub(crate) fn is_done(self) -> bool {
        self == Self::Done
    }
}

/// Read one device-state namespace for every account, concurrently.
///
/// Sequential would cost `accounts × RTT` on a path that used to block the
/// event loop; this is the shape `refresh_drafts` already used.
async fn read_namespace(
    client: &AxonClient,
    device_id: Uuid,
    account_ids: &[Uuid],
    namespace: &str,
) -> DeviceStateReads {
    let reads = account_ids.iter().map(|account_id| {
        let client = client.clone();
        let account_id = *account_id;
        async move {
            (
                account_id,
                client
                    .get_device_state(device_id, account_id, namespace)
                    .await,
            )
        }
    });
    futures_util::future::join_all(reads).await
}

impl App {
    /// Spawn `work` and deliver its outcome to the main loop.
    fn spawn_bootstrap<F>(&self, work: F)
    where
        F: Future<Output = BootstrapOutcome> + Send + 'static,
    {
        let Some(tx) = self.bootstrap_tx.clone() else {
            return;
        };
        tokio::spawn(async move {
            let _ = tx.send(work.await);
        });
    }

    /// Kick off stage 1. Called once, before the event loop starts drawing.
    pub(crate) fn start_bootstrap(&mut self) {
        self.bootstrap = BootstrapStage::Accounts;
        let client = self.client.clone();
        self.spawn_bootstrap(
            async move { BootstrapOutcome::Accounts(client.list_accounts().await) },
        );
    }

    /// Ask for a fresh room list, coalescing concurrent requests.
    ///
    /// A WS backlog can produce many "room I don't know" frames at once; each
    /// used to `await` a full unpaginated `GET /v1/rooms` on the main task and
    /// re-trigger the per-room title fan-out. At most one fetch is in flight,
    /// and at most one more is remembered behind it.
    pub(crate) fn request_rooms_refresh(&mut self) {
        if self.rooms_fetch_inflight {
            self.rooms_fetch_again = true;
            return;
        }
        self.rooms_fetch_inflight = true;
        self.rooms_fetch_had_selection = self.selected_room().is_some();
        let client = self.client.clone();
        let account_filter = self.account_filter;
        self.spawn_bootstrap(async move {
            BootstrapOutcome::Rooms(client.list_rooms(account_filter).await)
        });
    }

    /// Re-read both device-state namespaces for every known account.
    ///
    /// Used by startup and by every WS (re)connect — the lossy bus may have
    /// dropped `device_state` frames while the socket was down (ADR 0048).
    pub(crate) fn request_device_state(&mut self) {
        let client = self.client.clone();
        let device_id = self.device_id;
        let account_ids: Vec<Uuid> = self
            .accounts
            .accounts
            .iter()
            .map(|a| a.account_id)
            .collect();
        self.spawn_bootstrap(async move {
            let (markers, drafts) = tokio::join!(
                read_namespace(&client, device_id, &account_ids, READ_MARKERS_NAMESPACE),
                read_namespace(&client, device_id, &account_ids, DRAFTS_NAMESPACE),
            );
            BootstrapOutcome::DeviceState { markers, drafts }
        });
    }

    /// Apply one completed stage and start the next.
    pub(crate) async fn handle_bootstrap_outcome(&mut self, outcome: BootstrapOutcome) {
        match outcome {
            BootstrapOutcome::Accounts(result) => {
                match result {
                    Ok(accounts) => self.apply_account_refresh(accounts),
                    Err(err) => {
                        self.status = Status::from(format!("account refresh failed: {err}"));
                    }
                }
                // Advance even on failure: a server that cannot list accounts
                // still has rooms worth trying, and a stuck stage would leave
                // the panel saying "connecting" forever.
                self.bootstrap = BootstrapStage::Rooms;
                self.request_rooms_refresh();
            }
            BootstrapOutcome::Rooms(result) => {
                self.rooms_fetch_inflight = false;
                match result {
                    Ok(rooms) => self.apply_room_refresh(rooms),
                    Err(err) => {
                        if !self.is_mid_command() {
                            self.status = Status::from(format!("room refresh failed: {err}"));
                        }
                    }
                }
                if self.rooms_fetch_again {
                    self.rooms_fetch_again = false;
                    self.request_rooms_refresh();
                }
                if self.bootstrap == BootstrapStage::Rooms {
                    self.bootstrap = BootstrapStage::DeviceState;
                    self.request_device_state();
                } else if self.bootstrap.is_done()
                    && !self.rooms_fetch_had_selection
                    && self.selected_room().is_some()
                {
                    // A refresh revealed the first room this session: open it,
                    // as the inline refresh path used to.
                    self.load_selected_timeline().await;
                }
            }
            BootstrapOutcome::DeviceState { markers, drafts } => {
                // Markers before the first timeline load: see the module docs.
                self.apply_read_marker_reads(markers);
                self.apply_draft_reads(drafts);
                if self.bootstrap == BootstrapStage::DeviceState {
                    self.bootstrap = BootstrapStage::Done;
                    self.load_selected_timeline().await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TuiConfig;
    use ratatui_image::picker::Picker;
    use tokio::sync::mpsc;

    fn app() -> (App, mpsc::UnboundedReceiver<BootstrapOutcome>) {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:1".to_owned(), None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );
        let (tx, rx) = mpsc::unbounded_channel();
        app.set_bootstrap_sender(tx);
        (app, rx)
    }

    /// The stages advance in order, each one starting the next. The ordering is
    /// what protects the read-marker invariant: device state must be applied
    /// before the launch room's timeline load fabricates a marker of its own.
    #[tokio::test]
    async fn stages_advance_in_order() {
        let (mut app, _rx) = app();
        assert_eq!(app.bootstrap, BootstrapStage::Accounts);

        app.handle_bootstrap_outcome(BootstrapOutcome::Accounts(Ok(Vec::new())))
            .await;
        assert_eq!(app.bootstrap, BootstrapStage::Rooms);
        assert!(
            app.rooms_fetch_inflight,
            "the accounts stage should start the room fetch"
        );

        app.handle_bootstrap_outcome(BootstrapOutcome::Rooms(Ok(Vec::new())))
            .await;
        assert_eq!(app.bootstrap, BootstrapStage::DeviceState);

        app.handle_bootstrap_outcome(BootstrapOutcome::DeviceState {
            markers: Vec::new(),
            drafts: Vec::new(),
        })
        .await;
        assert!(app.bootstrap.is_done());
        assert!(app.bootstrap.label().is_none());
    }

    /// A failed stage still advances. Leaving it parked would strand the panel
    /// on "connecting…" and never fetch the rooms, which are worth trying even
    /// when the account list read failed.
    #[tokio::test]
    async fn a_failed_stage_still_advances() {
        let (mut app, _rx) = app();
        app.handle_bootstrap_outcome(BootstrapOutcome::Accounts(Err(ApiError::Request(
            "boom".to_owned(),
        ))))
        .await;
        assert_eq!(app.bootstrap, BootstrapStage::Rooms);
        assert!(app.rooms_fetch_inflight);
    }

    /// N room refreshes while one is in flight collapse to one follow-up, not N.
    ///
    /// This is the WS-backlog case: every live frame for a room the client does
    /// not know asks for a refresh, and each used to run a full unpaginated
    /// `GET /v1/rooms` on the main task plus its per-room title fan-out (#189).
    #[tokio::test]
    async fn concurrent_room_refresh_requests_coalesce() {
        let (mut app, _rx) = app();
        app.bootstrap = BootstrapStage::Done;

        app.request_rooms_refresh();
        assert!(app.rooms_fetch_inflight);
        assert!(!app.rooms_fetch_again);

        for _ in 0..5 {
            app.request_rooms_refresh();
        }
        assert!(
            app.rooms_fetch_again,
            "further requests are remembered, not spawned"
        );

        // The in-flight fetch lands: exactly one more goes out.
        app.handle_bootstrap_outcome(BootstrapOutcome::Rooms(Ok(Vec::new())))
            .await;
        assert!(app.rooms_fetch_inflight);
        assert!(!app.rooms_fetch_again);

        // And that one settles with nothing queued behind it.
        app.handle_bootstrap_outcome(BootstrapOutcome::Rooms(Ok(Vec::new())))
            .await;
        assert!(!app.rooms_fetch_inflight);
        assert!(!app.rooms_fetch_again);
    }

    /// Every stage before `Done` names itself for the rooms-panel title.
    #[test]
    fn every_pending_stage_has_a_label() {
        for stage in [
            BootstrapStage::Accounts,
            BootstrapStage::Rooms,
            BootstrapStage::DeviceState,
        ] {
            assert!(stage.label().is_some(), "{stage:?} needs a label");
        }
        assert!(BootstrapStage::Done.label().is_none());
    }
}
