//! The process-wide push router (ADR 0113, ADR 0129).
//!
//! Account tasks `try_send` observations. This task applies the case list and
//! fans out. It does not await Postgres or a provider. At most four sends run
//! at once, and each provider call has its own timeout.
//!
//! A fall in a room's `notification_count` (ADR 0129) queues a badge-only push
//! for each badge-enabled APNs device, and a fall to zero queues a room-read
//! message for each FCM device. Neither reads Postgres here: the total is read
//! inside the spawned send.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axon_core::PushMaterial;
use axon_store::{PushTarget, Store};
use tokio::sync::{mpsc, Notify};

use super::deliver::SharedBanner;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use super::decision::{self, RoomState};
use super::deliver::LiveTransport;
use super::message::PushMessage;

/// How long a slot or an unpaired rise lives. Matches the unread resweep.
const SLOT_TTL: Duration = Duration::from_secs(300);
const OBSERVATION_CAPACITY: usize = 64;
/// How many provider calls may run at once. The scheduler is the only cap.
const SEND_PERMITS: usize = 4;

/// One in-flight or waiting send.
///
/// At most one send per key runs, and at most one waits. A newer job replaces
/// the waiting one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FlightKey {
    /// Alerts and room-read messages for one room on one device. They share a
    /// key so they never run at once, and so a read replaces an alert that is
    /// still waiting. The account is part of the key: two accounts in the same
    /// room must not share a slot on one device.
    Room {
        account_id: Uuid,
        registration_id: Uuid,
        room_id: String,
    },
    /// Badge-only pushes for one device. A burst of reads coalesces into one
    /// running push and one waiting push, and the waiting one reads the total
    /// when it starts.
    Badge { registration_id: Uuid },
}

/// One room's counts and eligible messages, handed off the sync loop.
#[derive(Debug, Clone)]
pub(crate) struct Observation {
    pub account_id: Uuid,
    pub room_id: String,
    pub after: Option<(u64, u64)>,
    pub messages: Vec<PushMessage>,
}

struct Control {
    notify: Notify,
    drop_accounts: Mutex<HashSet<Uuid>>,
    drop_rooms: Mutex<HashSet<(Uuid, String)>>,
    unseed_accounts: Mutex<HashSet<Uuid>>,
    finished: Mutex<Vec<FlightKey>>,
}

impl Control {
    fn new() -> Self {
        Self {
            notify: Notify::new(),
            drop_accounts: Mutex::new(HashSet::new()),
            drop_rooms: Mutex::new(HashSet::new()),
            unseed_accounts: Mutex::new(HashSet::new()),
            finished: Mutex::new(Vec::new()),
        }
    }
}

#[derive(Clone)]
struct Live {
    tx: mpsc::Sender<Observation>,
    control: Arc<Control>,
    snapshot: Arc<RwLock<Arc<Vec<PushTarget>>>>,
}

/// How an account task hands the router a wake. `Off` drops everything.
#[derive(Clone)]
pub(crate) struct PushIngress {
    live: Option<Live>,
}

impl PushIngress {
    /// No router. Test lifecycles use this so a deleted account does not need a task.
    #[cfg(test)]
    pub(crate) fn off() -> Self {
        Self { live: None }
    }

    pub(crate) fn observe(&self, observation: Observation) {
        let Some(live) = &self.live else {
            return;
        };
        if let Err(err) = live.tx.try_send(observation) {
            let observation = match err {
                mpsc::error::TrySendError::Full(observation) => observation,
                mpsc::error::TrySendError::Closed(_) => return,
            };
            let ids: Vec<Uuid> = live
                .snapshot
                .read()
                .expect("push snapshot")
                .iter()
                .map(|target| target.id)
                .collect();
            let event_id = observation
                .messages
                .last()
                .map(|message| message.event_id.as_str())
                .unwrap_or("");
            tracing::warn!(
                account_id = %observation.account_id,
                room_id = %observation.room_id,
                event_id,
                ?ids,
                "push observation dropped; channel full"
            );
        }
    }

    pub(crate) fn account_removed(&self, account_id: Uuid) {
        let Some(live) = &self.live else {
            return;
        };
        live.control
            .drop_accounts
            .lock()
            .expect("push control")
            .insert(account_id);
        live.control.notify.notify_one();
    }

    pub(crate) fn room_dropped(&self, account_id: Uuid, room_id: &str) {
        let Some(live) = &self.live else {
            return;
        };
        live.control
            .drop_rooms
            .lock()
            .expect("push control")
            .insert((account_id, room_id.to_owned()));
        live.control.notify.notify_one();
    }

    pub(crate) fn unseed_account(&self, account_id: Uuid) {
        let Some(live) = &self.live else {
            return;
        };
        live.control
            .unseed_accounts
            .lock()
            .expect("push control")
            .insert(account_id);
        live.control.notify.notify_one();
    }
}

struct Stamped<T> {
    value: T,
    at: Instant,
}

struct Memory {
    seeded: bool,
    pair: (u64, u64),
    slot: Option<Stamped<PushMessage>>,
    unpaired: Option<Stamped<(u64, u64)>>,
}

impl Memory {
    fn unseeded(pair: (u64, u64)) -> Self {
        Self {
            seeded: false,
            pair,
            slot: None,
            unpaired: None,
        }
    }

    fn expire(&mut self, now: Instant) {
        if self
            .slot
            .as_ref()
            .is_some_and(|slot| now.saturating_duration_since(slot.at) >= SLOT_TTL)
        {
            self.slot = None;
        }
        if self
            .unpaired
            .as_ref()
            .is_some_and(|rise| now.saturating_duration_since(rise.at) >= SLOT_TTL)
        {
            self.unpaired = None;
        }
    }
}

/// What a job sends.
enum Work {
    /// An ADR 0113 alert for one message.
    Alert {
        account_id: Uuid,
        room_id: String,
        message: PushMessage,
        banner: Arc<SharedBanner>,
    },
    /// An ADR 0129 room-read message: the room's count fell to zero.
    RoomRead { account_id: Uuid, room_id: String },
    /// An ADR 0129 badge-only push: some room's count fell.
    Badge,
}

struct Job {
    target: PushTarget,
    work: Work,
}

impl Job {
    fn key(&self) -> FlightKey {
        match self.room() {
            Some((account_id, room_id)) => FlightKey::Room {
                account_id,
                registration_id: self.target.id,
                room_id: room_id.to_owned(),
            },
            None => FlightKey::Badge {
                registration_id: self.target.id,
            },
        }
    }

    /// The account and room a job belongs to. A badge-only push has neither,
    /// so dropping an account or a room never drops it.
    fn room(&self) -> Option<(Uuid, &str)> {
        match &self.work {
            Work::Alert {
                account_id,
                room_id,
                ..
            }
            | Work::RoomRead {
                account_id,
                room_id,
            } => Some((*account_id, room_id.as_str())),
            Work::Badge => None,
        }
    }
}

struct Scheduler {
    in_flight: HashSet<FlightKey>,
    waiting: HashMap<FlightKey, Job>,
    order: VecDeque<FlightKey>,
}

impl Scheduler {
    fn new() -> Self {
        Self {
            in_flight: HashSet::new(),
            waiting: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn enqueue(&mut self, job: Job) -> Option<Job> {
        let key = job.key();
        if self.in_flight.contains(&key) {
            self.waiting.insert(key, job);
            return None;
        }
        if self.in_flight.len() >= SEND_PERMITS {
            if !self.order.iter().any(|queued| queued == &key) {
                self.order.push_back(key.clone());
            }
            self.waiting.insert(key, job);
            return None;
        }
        self.in_flight.insert(key);
        Some(job)
    }

    fn finished(&mut self, key: FlightKey) -> Vec<Job> {
        self.in_flight.remove(&key);
        let mut start = Vec::new();
        if let Some(job) = self.waiting.remove(&key) {
            self.order.retain(|queued| queued != &key);
            if let Some(job) = self.enqueue(job) {
                start.push(job);
            }
        }
        start.extend(self.pump());
        start
    }

    fn pump(&mut self) -> Vec<Job> {
        let mut start = Vec::new();
        while self.in_flight.len() < SEND_PERMITS {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            let Some(job) = self.waiting.remove(&key) else {
                continue;
            };
            if let Some(job) = self.enqueue(job) {
                start.push(job);
            }
        }
        start
    }

    fn drop_account(&mut self, account_id: Uuid) {
        self.waiting
            .retain(|_, job| job.room().map(|(id, _)| id) != Some(account_id));
        self.order
            .retain(|key| self.waiting.contains_key(key) || self.in_flight.contains(key));
    }

    fn drop_room(&mut self, account_id: Uuid, room_id: &str) {
        self.waiting
            .retain(|_, job| job.room() != Some((account_id, room_id)));
        self.order
            .retain(|key| self.waiting.contains_key(key) || self.in_flight.contains(key));
    }
}

/// Start the router, the registration reload, and the session reap.
pub(crate) fn spawn(
    store: Store,
    material: PushMaterial,
    changed: Arc<Notify>,
    cancel: CancellationToken,
    tracker: &TaskTracker,
) -> PushIngress {
    let (tx, rx) = mpsc::channel(OBSERVATION_CAPACITY);
    let control = Arc::new(Control::new());
    let snapshot = Arc::new(RwLock::new(Arc::new(Vec::new())));
    let ready = Arc::new(Notify::new());
    let transport = Arc::new(LiveTransport::new(store.clone(), material));

    let reload_cancel = cancel.clone();
    let reload_store = store.clone();
    let reload_snapshot = Arc::clone(&snapshot);
    let reload_changed = Arc::clone(&changed);
    let reload_ready = Arc::clone(&ready);
    tracker.spawn(async move {
        reload_loop(
            reload_store,
            reload_snapshot,
            reload_changed,
            reload_ready,
            reload_cancel,
        )
        .await;
    });

    let reap_cancel = cancel.clone();
    let reap_store = store.clone();
    let reap_changed = Arc::clone(&changed);
    tracker.spawn(async move {
        reap_loop(reap_store, reap_changed, reap_cancel).await;
    });

    let decision_snapshot = Arc::clone(&snapshot);
    let decision_control = Arc::clone(&control);
    tracker.spawn(async move {
        decide_loop(
            rx,
            RouterLoop {
                snapshot: decision_snapshot,
                control: decision_control,
                transport,
                shutdown: cancel,
            },
            ready,
        )
        .await;
    });

    PushIngress {
        live: Some(Live {
            tx,
            control,
            snapshot,
        }),
    }
}

async fn reload_loop(
    store: Store,
    snapshot: Arc<RwLock<Arc<Vec<PushTarget>>>>,
    changed: Arc<Notify>,
    ready: Arc<Notify>,
    cancel: CancellationToken,
) {
    load_snapshot(&store, &snapshot).await;
    ready.notify_one();
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + SLOT_TTL, SLOT_TTL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = changed.notified() => load_snapshot(&store, &snapshot).await,
            _ = tick.tick() => load_snapshot(&store, &snapshot).await,
        }
    }
}

async fn load_snapshot(store: &Store, snapshot: &RwLock<Arc<Vec<PushTarget>>>) {
    match store.list_push_targets().await {
        Ok(targets) => {
            *snapshot.write().expect("push snapshot") = Arc::new(targets);
        }
        Err(err) => {
            tracing::warn!(error = %err, "push registration snapshot was not reloaded");
        }
    }
}

async fn reap_loop(store: Store, changed: Arc<Notify>, cancel: CancellationToken) {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + SLOT_TTL, SLOT_TTL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {
                match store.reap_push_registrations().await {
                    Ok(0) => {}
                    Ok(reaped) => {
                        tracing::info!(
                            reaped,
                            "removed push registrations whose sessions are not live"
                        );
                        changed.notify_one();
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "push registration reap failed");
                    }
                }
            }
        }
    }
}

struct RouterLoop {
    snapshot: Arc<RwLock<Arc<Vec<PushTarget>>>>,
    control: Arc<Control>,
    transport: Arc<LiveTransport>,
    shutdown: CancellationToken,
}

async fn decide_loop(mut rx: mpsc::Receiver<Observation>, router: RouterLoop, ready: Arc<Notify>) {
    // No preload of `room_unread_counts`. The first observation of a room
    // stores `after` and does not send, so a startup scan would only delay
    // that and drop observations past the channel cap.
    let mut rooms: HashMap<(Uuid, String), Memory> = HashMap::new();
    tokio::select! {
        _ = router.shutdown.cancelled() => return,
        _ = ready.notified() => {}
    }

    let mut scheduler = Scheduler::new();
    let mut account_cancels: HashMap<Uuid, CancellationToken> = HashMap::new();
    let mut expiry = tokio::time::interval(Duration::from_secs(30));
    expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    expiry.tick().await;

    loop {
        tokio::select! {
            biased;
            _ = router.shutdown.cancelled() => return,
            _ = router.control.notify.notified() => {
                apply_control(&router, &mut rooms, &mut scheduler, &mut account_cancels);
            }
            _ = expiry.tick() => {
                let now = Instant::now();
                for memory in rooms.values_mut() {
                    memory.expire(now);
                }
            }
            observation = rx.recv() => {
                let Some(observation) = observation else { return };
                apply_observation(
                    observation,
                    &mut rooms,
                    &mut scheduler,
                    &mut account_cancels,
                    &router,
                );
            }
        }
    }
}

fn apply_control(
    router: &RouterLoop,
    rooms: &mut HashMap<(Uuid, String), Memory>,
    scheduler: &mut Scheduler,
    account_cancels: &mut HashMap<Uuid, CancellationToken>,
) {
    let control = &router.control;
    let finished: Vec<_> = control
        .finished
        .lock()
        .expect("push control")
        .drain(..)
        .collect();
    let dropped_accounts: Vec<_> = control
        .drop_accounts
        .lock()
        .expect("push control")
        .drain()
        .collect();
    let dropped_rooms: Vec<_> = control
        .drop_rooms
        .lock()
        .expect("push control")
        .drain()
        .collect();
    let unseeded: Vec<_> = control
        .unseed_accounts
        .lock()
        .expect("push control")
        .drain()
        .collect();

    for account_id in dropped_accounts {
        rooms.retain(|(id, _), _| *id != account_id);
        scheduler.drop_account(account_id);
        if let Some(token) = account_cancels.remove(&account_id) {
            token.cancel();
        }
    }
    for (account_id, room_id) in dropped_rooms {
        rooms.remove(&(account_id, room_id.clone()));
        scheduler.drop_room(account_id, &room_id);
    }
    for account_id in unseeded {
        for ((id, _), memory) in rooms.iter_mut() {
            if *id == account_id {
                memory.seeded = false;
                memory.unpaired = None;
            }
        }
    }
    for key in finished {
        for job in scheduler.finished(key) {
            start_job(job, account_cancels, router);
        }
    }
}

fn apply_observation(
    observation: Observation,
    rooms: &mut HashMap<(Uuid, String), Memory>,
    scheduler: &mut Scheduler,
    account_cancels: &mut HashMap<Uuid, CancellationToken>,
    router: &RouterLoop,
) {
    let key = (observation.account_id, observation.room_id.clone());
    let memory = rooms.entry(key).or_insert_with(|| Memory::unseeded((0, 0)));
    // Zero is only a placeholder. The room stays unseeded until an observation
    // carries `after`, and that first observation stores the pair instead of
    // comparing it with zero.
    let now = Instant::now();
    memory.expire(now);
    let previous_slot = memory
        .slot
        .as_ref()
        .map(|slot| (slot.value.clone(), slot.at));
    let previous_unpaired = memory.unpaired.as_ref().map(|rise| (rise.value, rise.at));
    let state = RoomState {
        seeded: memory.seeded,
        pair: memory.pair,
        slot: memory.slot.as_ref().map(|slot| slot.value.clone()),
        unpaired: memory.unpaired.as_ref().map(|rise| rise.value),
    };
    let decision = decision::decide(&state, observation.after, &observation.messages);
    for event_id in &decision.unknown_ids {
        tracing::warn!(
            account_id = %observation.account_id,
            room_id = %observation.room_id,
            event_id,
            "live push actions were missing"
        );
    }
    memory.seeded = decision.state.seeded;
    memory.pair = decision.state.pair;
    memory.slot = decision.state.slot.map(|message| {
        let at = previous_slot
            .as_ref()
            .filter(|(previous, _)| previous == &message)
            .map(|(_, at)| *at)
            .unwrap_or(now);
        Stamped { value: message, at }
    });
    memory.unpaired = decision.state.unpaired.map(|pair| {
        let at = previous_unpaired
            .filter(|(previous, _)| *previous == pair)
            .map(|(_, at)| at)
            .unwrap_or(now);
        Stamped { value: pair, at }
    });

    let targets = router.snapshot.read().expect("push snapshot").clone();
    let jobs = jobs_for(
        &observation,
        decision.send,
        decision.notification_fell_to,
        &targets,
    );
    if decision.notification_fell_to.is_some() && !jobs.is_empty() {
        tracing::debug!(
            account_id = %observation.account_id,
            room_id = %observation.room_id,
            notification_count = decision.notification_fell_to,
            jobs = jobs.len(),
            "unread count fell; queueing push clears"
        );
    }
    for job in jobs {
        if let Some(job) = scheduler.enqueue(job) {
            start_job(job, account_cancels, router);
        }
    }
}

/// The sends one decided observation produces, one per device that gets one.
///
/// An alert goes to every device. Without an alert, a fall queues a badge-only
/// push for each badge-enabled APNs device, and a fall to zero queues a
/// room-read message for each FCM device (ADR 0129). An alert in the same
/// observation already carries the total, so it suppresses the badge-only
/// push. ADR 0113 never sends an alert on a fall to zero, so the room-read
/// message cannot be suppressed by one.
fn jobs_for(
    observation: &Observation,
    send: Option<PushMessage>,
    notification_fell_to: Option<u64>,
    targets: &[PushTarget],
) -> Vec<Job> {
    if let Some(message) = send {
        let banner = Arc::new(SharedBanner::new(
            observation.account_id,
            &observation.room_id,
            &message.sender,
        ));
        return targets
            .iter()
            .map(|target| Job {
                target: target.clone(),
                work: Work::Alert {
                    account_id: observation.account_id,
                    room_id: observation.room_id.clone(),
                    message: message.clone(),
                    banner: Arc::clone(&banner),
                },
            })
            .collect();
    }
    let Some(count) = notification_fell_to else {
        return Vec::new();
    };
    targets
        .iter()
        .filter_map(|target| {
            let work = match target.provider.as_str() {
                "apns" if target.badge => Work::Badge,
                "fcm" if count == 0 => Work::RoomRead {
                    account_id: observation.account_id,
                    room_id: observation.room_id.clone(),
                },
                _ => return None,
            };
            Some(Job {
                target: target.clone(),
                work,
            })
        })
        .collect()
}

fn start_job(
    job: Job,
    account_cancels: &mut HashMap<Uuid, CancellationToken>,
    router: &RouterLoop,
) {
    // A room's sends stop when its account is removed. A badge-only push
    // belongs to the device and stops only at shutdown.
    let cancel = match job.room() {
        Some((account_id, _)) => account_cancels
            .entry(account_id)
            .or_insert_with(|| router.shutdown.child_token())
            .clone(),
        None => router.shutdown.child_token(),
    };
    let finish = Finish {
        key: job.key(),
        control: Arc::clone(&router.control),
    };
    let transport = Arc::clone(&router.transport);
    tokio::spawn(run_send(job, transport, cancel, finish));
}

/// The scheduler marks a job in flight before this runs. Dropping the guard
/// releases that key even on cancel.
struct Finish {
    key: FlightKey,
    control: Arc<Control>,
}

impl Drop for Finish {
    fn drop(&mut self) {
        self.control
            .finished
            .lock()
            .expect("push control")
            .push(self.key.clone());
        self.control.notify.notify_one();
    }
}

async fn run_send(
    job: Job,
    transport: Arc<LiveTransport>,
    cancel: CancellationToken,
    finish: Finish,
) {
    let send = async {
        match &job.work {
            Work::Alert {
                account_id,
                room_id,
                message,
                banner,
            } => {
                transport
                    .deliver(&job.target, *account_id, room_id, message, banner, &cancel)
                    .await;
            }
            Work::RoomRead {
                account_id,
                room_id,
            } => {
                transport
                    .deliver_room_read(&job.target, *account_id, room_id, &cancel)
                    .await;
            }
            Work::Badge => transport.deliver_badge(&job.target, &cancel).await,
        }
    };
    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = send => {}
    }
    drop(finish);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::message::PushClass;
    use chrono::{TimeZone, Utc};

    fn target(registration_id: Uuid, provider: &str, badge: bool) -> PushTarget {
        PushTarget {
            id: registration_id,
            provider: provider.to_owned(),
            token: "device".to_owned(),
            environment: (provider == "apns").then(|| "sandbox".to_owned()),
            disclosure: "activity".to_owned(),
            badge,
            updated_at: Utc.timestamp_millis_opt(0).single().unwrap(),
        }
    }

    fn message() -> PushMessage {
        PushMessage {
            event_id: "$e".to_owned(),
            sender: "@bob:example".to_owned(),
            body: "hi".to_owned(),
            thread_root_id: None,
            class: PushClass::Notify,
        }
    }

    fn job(account_id: Uuid, registration_id: Uuid, room_id: &str) -> Job {
        Job {
            target: target(registration_id, "fcm", false),
            work: Work::Alert {
                account_id,
                room_id: room_id.to_owned(),
                message: message(),
                banner: Arc::new(SharedBanner::new(account_id, room_id, "@bob:example")),
            },
        }
    }

    fn room_read(account_id: Uuid, registration_id: Uuid, room_id: &str) -> Job {
        Job {
            target: target(registration_id, "fcm", false),
            work: Work::RoomRead {
                account_id,
                room_id: room_id.to_owned(),
            },
        }
    }

    fn badge(registration_id: Uuid) -> Job {
        Job {
            target: target(registration_id, "apns", true),
            work: Work::Badge,
        }
    }

    fn observation(account_id: Uuid) -> Observation {
        Observation {
            account_id,
            room_id: "!r".to_owned(),
            after: None,
            messages: Vec::new(),
        }
    }

    fn kinds(jobs: &[Job]) -> Vec<(&'static str, Uuid)> {
        jobs.iter()
            .map(|job| {
                let kind = match job.work {
                    Work::Alert { .. } => "alert",
                    Work::RoomRead { .. } => "room_read",
                    Work::Badge => "badge",
                };
                (kind, job.target.id)
            })
            .collect()
    }

    #[test]
    fn two_accounts_in_one_room_keep_separate_send_slots() {
        let mut scheduler = Scheduler::new();
        let device = Uuid::new_v4();
        let first = scheduler
            .enqueue(job(Uuid::new_v4(), device, "!r"))
            .is_some();
        let second = scheduler
            .enqueue(job(Uuid::new_v4(), device, "!r"))
            .is_some();
        assert!(first);
        assert!(second);
    }

    #[test]
    fn a_second_wake_for_the_same_account_waits() {
        let mut scheduler = Scheduler::new();
        let account = Uuid::new_v4();
        let device = Uuid::new_v4();
        assert!(scheduler.enqueue(job(account, device, "!r")).is_some());
        assert!(scheduler.enqueue(job(account, device, "!r")).is_none());
    }

    #[test]
    fn an_alert_goes_to_every_device_and_suppresses_the_clears() {
        let account = Uuid::new_v4();
        let (phone, android, plain) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let targets = [
            target(phone, "apns", true),
            target(android, "fcm", false),
            target(plain, "apns", false),
        ];
        let jobs = jobs_for(&observation(account), Some(message()), Some(3), &targets);
        assert_eq!(
            kinds(&jobs),
            vec![("alert", phone), ("alert", android), ("alert", plain)]
        );
    }

    #[test]
    fn a_partial_read_badges_opted_in_iphones_and_leaves_android_alone() {
        let account = Uuid::new_v4();
        let (phone, android, plain) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let targets = [
            target(phone, "apns", true),
            target(android, "fcm", true),
            target(plain, "apns", false),
        ];
        let jobs = jobs_for(&observation(account), None, Some(2), &targets);
        assert_eq!(kinds(&jobs), vec![("badge", phone)]);
    }

    #[test]
    fn a_fall_to_zero_also_tells_every_android_device_the_room_was_read() {
        let account = Uuid::new_v4();
        let (phone, android, android_off) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let targets = [
            target(phone, "apns", true),
            target(android, "fcm", true),
            // The badge flag is a badge preference, not a notification one.
            target(android_off, "fcm", false),
        ];
        let jobs = jobs_for(&observation(account), None, Some(0), &targets);
        assert_eq!(
            kinds(&jobs),
            vec![
                ("badge", phone),
                ("room_read", android),
                ("room_read", android_off)
            ]
        );
    }

    #[test]
    fn no_fall_and_no_alert_sends_nothing() {
        let targets = [
            target(Uuid::new_v4(), "apns", true),
            target(Uuid::new_v4(), "fcm", true),
        ];
        assert!(jobs_for(&observation(Uuid::new_v4()), None, None, &targets).is_empty());
    }

    #[test]
    fn a_room_read_shares_the_room_key_and_replaces_a_waiting_alert() {
        let mut scheduler = Scheduler::new();
        let account = Uuid::new_v4();
        let device = Uuid::new_v4();
        assert!(scheduler.enqueue(job(account, device, "!r")).is_some());
        // The read cannot run beside the alert in flight for the same room.
        assert!(scheduler.enqueue(job(account, device, "!r")).is_none());
        assert!(scheduler
            .enqueue(room_read(account, device, "!r"))
            .is_none());

        let key = job(account, device, "!r").key();
        let started = scheduler.finished(key);
        assert_eq!(started.len(), 1);
        assert!(matches!(started[0].work, Work::RoomRead { .. }));
    }

    #[test]
    fn a_burst_of_reads_holds_one_running_and_one_waiting_badge() {
        let mut scheduler = Scheduler::new();
        let device = Uuid::new_v4();
        assert!(scheduler.enqueue(badge(device)).is_some());
        for _ in 0..20 {
            assert!(scheduler.enqueue(badge(device)).is_none());
        }
        assert_eq!(scheduler.waiting.len(), 1);

        let started = scheduler.finished(FlightKey::Badge {
            registration_id: device,
        });
        assert_eq!(started.len(), 1);
        assert!(scheduler
            .finished(FlightKey::Badge {
                registration_id: device
            })
            .is_empty());
    }

    #[test]
    fn a_badge_does_not_wait_behind_a_room_send_on_the_same_device() {
        let mut scheduler = Scheduler::new();
        let device = Uuid::new_v4();
        assert!(scheduler
            .enqueue(job(Uuid::new_v4(), device, "!r"))
            .is_some());
        assert!(scheduler.enqueue(badge(device)).is_some());
    }

    #[test]
    fn dropping_an_account_or_a_room_keeps_a_waiting_badge() {
        let mut scheduler = Scheduler::new();
        let account = Uuid::new_v4();
        let device = Uuid::new_v4();
        // Fill the permits so every later job waits.
        for _ in 0..SEND_PERMITS {
            assert!(scheduler
                .enqueue(job(Uuid::new_v4(), Uuid::new_v4(), "!busy"))
                .is_some());
        }
        assert!(scheduler.enqueue(badge(device)).is_none());
        assert!(scheduler
            .enqueue(room_read(account, device, "!r"))
            .is_none());
        assert!(scheduler
            .enqueue(room_read(account, device, "!other"))
            .is_none());

        scheduler.drop_room(account, "!r");
        assert_eq!(scheduler.waiting.len(), 2);
        scheduler.drop_account(account);
        assert_eq!(scheduler.waiting.len(), 1);
        assert!(scheduler.waiting.contains_key(&FlightKey::Badge {
            registration_id: device
        }));
    }
}
