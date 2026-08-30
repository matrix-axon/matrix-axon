//! Account-scoped Matrix OAuth QR login grants (ADR 0097).
//!
//! The registry owns every flow and its cancellation token. A grant holds the
//! account's identity lock only while the SDK protocol is live: sync continues,
//! while logout, deletion, and supervised-client replacement cancel the grant
//! before waiting for that lock. Before every poll that can advance the SDK
//! future, Axon revalidates the current client and re-derives trust under the
//! lock. Active-account reads are cached for five seconds; lifecycle
//! invalidation cancels the same flow token before waiting for the lock, so the
//! cache cannot delay teardown. This puts the second trust derivation directly
//! in front of the SDK poll that may export or transmit the secrets bundle.

use std::{
    collections::HashMap,
    future::{Future, IntoFuture},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axon_core::SyncConfig;
use axon_store::{AccountState, Store};
use futures_util::{
    task::{waker_ref, ArcWake},
    Stream,
};
use matrix_sdk::{
    authentication::oauth::qrcode::{
        CheckCodeSender, GeneratedQrProgress, GrantLoginProgress, QRCodeGrantLoginError,
        QrCodeData, QrProgress, SecureChannelError,
    },
    Client,
};
use tokio::sync::{oneshot, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    lifecycle::{derive_verified, lock_for, IdentityLock, IdentityLocks},
    manager::ClientManager,
    matrix_oauth_acquire::{
        parse_check_code, parse_grant_qr_payload, validate_qr_url, MatrixOAuthAcquireError,
    },
    matrix_oauth_flow::{
        run_reaper, FlowLease, FLOW_TTL, MAX_CONCURRENT_FLOWS, MAX_RETAINED_FLOWS,
    },
};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);
const TRUST_TIMEOUT: Duration = Duration::from_secs(10);
const EXPORT_TIMEOUT: Duration = Duration::from_secs(15);
const ACTIVE_ACCOUNT_RECHECK_INTERVAL: Duration = Duration::from_secs(5);

struct ActiveAccountRecheck {
    next_check: Instant,
}

impl ActiveAccountRecheck {
    fn due_now(now: Instant) -> Self {
        Self { next_check: now }
    }

    fn is_due(&self, now: Instant) -> bool {
        now >= self.next_check
    }

    fn record_success(&mut self, now: Instant) {
        self.next_check = now + ACTIVE_ACCOUNT_RECHECK_INTERVAL;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixOAuthGrantPresentation {
    Display,
    Scan,
}

impl MatrixOAuthGrantPresentation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Scan => "scan",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixOAuthGrantStage {
    Starting,
    QrReady,
    CheckCodeToDisplay,
    CheckCodeRequired,
    WaitingForAuthorization,
    SyncingSecrets,
    Done,
    Failed,
    Cancelled,
}

impl MatrixOAuthGrantStage {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct MatrixOAuthGrantState {
    pub flow_id: Uuid,
    pub account_id: Uuid,
    pub presentation: MatrixOAuthGrantPresentation,
    pub stage: MatrixOAuthGrantStage,
    pub qr_code_data: Option<String>,
    pub check_code: Option<String>,
    pub verification_uri: Option<String>,
    pub error_code: Option<String>,
}

impl MatrixOAuthGrantState {
    fn new(flow_id: Uuid, account_id: Uuid, presentation: MatrixOAuthGrantPresentation) -> Self {
        Self {
            flow_id,
            account_id,
            presentation,
            stage: MatrixOAuthGrantStage::Starting,
            qr_code_data: None,
            check_code: None,
            verification_uri: None,
            error_code: None,
        }
    }

    fn set_stage(&mut self, stage: MatrixOAuthGrantStage) {
        self.stage = stage;
        self.qr_code_data = None;
        self.check_code = None;
        self.verification_uri = None;
        self.error_code = None;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MatrixOAuthGrantError {
    #[error("Matrix account not found: {0}")]
    AccountNotFound(Uuid),
    #[error("Matrix account is not active: {0}")]
    AccountNotActive(Uuid),
    #[error("Matrix account is being deleted: {0}")]
    AccountBeingDeleted(Uuid),
    #[error("Matrix account's current device is not trusted")]
    DeviceNotTrusted,
    #[error("Matrix account cannot export the required secrets bundle")]
    SecretsUnavailable,
    #[error("a Matrix OAuth QR grant is already active for this account")]
    FlowAlreadyExists,
    #[error("too many Matrix OAuth QR grant flows are active or retained")]
    Capacity,
    #[error("Matrix OAuth QR grant flow not found: {0}")]
    NotFound(Uuid),
    #[error("input does not match the flow presentation or stage")]
    WrongStage,
    #[error("invalid QR grant input: {0}")]
    InvalidInput(&'static str),
    #[error("the current Matrix client is temporarily unavailable")]
    Unavailable,
    #[error("Matrix OAuth QR grant failed internally")]
    Internal,
}

struct FlowMutable {
    state: MatrixOAuthGrantState,
    scan_tx: Option<oneshot::Sender<QrCodeData>>,
    check_code_tx: Option<oneshot::Sender<u8>>,
    lease: FlowLease,
}

struct Flow {
    mutable: Mutex<FlowMutable>,
    cancel: CancellationToken,
    identity_lock: IdentityLock,
}

impl Flow {
    fn snapshot(&self) -> MatrixOAuthGrantState {
        self.mutable
            .lock()
            .expect("Matrix OAuth grant flow poisoned")
            .state
            .clone()
    }

    fn update(&self, update: impl FnOnce(&mut MatrixOAuthGrantState)) {
        let mut mutable = self
            .mutable
            .lock()
            .expect("Matrix OAuth grant flow poisoned");
        if mutable.lease.is_live() && !mutable.state.stage.is_terminal() {
            update(&mut mutable.state);
        }
    }

    /// Consume the only live owner. Completion, timeout, lifecycle invalidation,
    /// and DELETE can race, but exactly one of them can publish a terminal state.
    fn terminal(&self, stage: MatrixOAuthGrantStage, error_code: Option<&'static str>) -> bool {
        self.terminal_at(stage, error_code, Instant::now())
    }

    fn terminal_at(
        &self,
        stage: MatrixOAuthGrantStage,
        error_code: Option<&'static str>,
        now: Instant,
    ) -> bool {
        let mut mutable = self
            .mutable
            .lock()
            .expect("Matrix OAuth grant flow poisoned");
        if !mutable.lease.finish(now) {
            return false;
        }
        mutable.state.set_stage(stage);
        mutable.state.error_code = error_code.map(str::to_owned);
        mutable.scan_tx.take();
        mutable.check_code_tx.take();
        true
    }

    fn cancel(&self) -> bool {
        if !self.terminal(MatrixOAuthGrantStage::Cancelled, None) {
            return false;
        }
        self.cancel.cancel();
        true
    }

    fn accept_scan(
        &self,
        parsed: QrCodeData,
    ) -> Result<MatrixOAuthGrantState, MatrixOAuthGrantError> {
        let mut mutable = self
            .mutable
            .lock()
            .expect("Matrix OAuth grant flow poisoned");
        if mutable.state.presentation != MatrixOAuthGrantPresentation::Scan
            || mutable.state.stage != MatrixOAuthGrantStage::Starting
            || !mutable.lease.is_live()
        {
            return Err(MatrixOAuthGrantError::WrongStage);
        }
        mutable
            .scan_tx
            .take()
            .ok_or(MatrixOAuthGrantError::WrongStage)?
            .send(parsed)
            .map_err(|_| MatrixOAuthGrantError::WrongStage)?;
        Ok(mutable.state.clone())
    }

    fn accept_check_code(&self, value: u8) -> Result<MatrixOAuthGrantState, MatrixOAuthGrantError> {
        let mut mutable = self
            .mutable
            .lock()
            .expect("Matrix OAuth grant flow poisoned");
        if mutable.state.presentation != MatrixOAuthGrantPresentation::Display
            || mutable.state.stage != MatrixOAuthGrantStage::CheckCodeRequired
            || !mutable.lease.is_live()
        {
            return Err(MatrixOAuthGrantError::WrongStage);
        }
        mutable
            .check_code_tx
            .take()
            .ok_or(MatrixOAuthGrantError::WrongStage)?
            .send(value)
            .map_err(|_| MatrixOAuthGrantError::WrongStage)?;
        Ok(mutable.state.clone())
    }
}

struct RegistryInner {
    flows: HashMap<Uuid, Arc<Flow>>,
    account_owners: HashMap<Uuid, Uuid>,
}

/// Shared ownership/cancellation registry. Lifecycle teardown and supervised
/// client eviction use this same handle, so there is no second cancellation map
/// that can drift away from the HTTP-visible flow resource.
#[derive(Clone)]
pub(crate) struct MatrixOAuthGrantRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    capacity: Arc<Semaphore>,
}

impl MatrixOAuthGrantRegistry {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                flows: HashMap::new(),
                account_owners: HashMap::new(),
            })),
            capacity: Arc::new(Semaphore::new(MAX_CONCURRENT_FLOWS)),
        }
    }

    fn reserve(
        &self,
        account_id: Uuid,
    ) -> Result<(Uuid, OwnedSemaphorePermit), MatrixOAuthGrantError> {
        self.reap_expired();
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| MatrixOAuthGrantError::Capacity)?;
        let mut inner = self
            .inner
            .lock()
            .expect("Matrix OAuth grant registry poisoned");
        if inner.flows.len() >= MAX_RETAINED_FLOWS {
            return Err(MatrixOAuthGrantError::Capacity);
        }
        if inner.account_owners.contains_key(&account_id) {
            return Err(MatrixOAuthGrantError::FlowAlreadyExists);
        }
        let flow_id = Uuid::new_v4();
        inner.account_owners.insert(account_id, flow_id);
        Ok((flow_id, permit))
    }

    fn commit_reservation(&self, flow: Arc<Flow>) -> Result<(), MatrixOAuthGrantError> {
        let state = flow.snapshot();
        let mut inner = self
            .inner
            .lock()
            .expect("Matrix OAuth grant registry poisoned");
        if inner.account_owners.get(&state.account_id) != Some(&state.flow_id) {
            return Err(MatrixOAuthGrantError::Unavailable);
        }
        if inner.flows.len() >= MAX_RETAINED_FLOWS {
            return Err(MatrixOAuthGrantError::Capacity);
        }
        inner.flows.insert(state.flow_id, flow);
        Ok(())
    }

    fn abandon_reservation(&self, account_id: Uuid, flow_id: Uuid) {
        let mut inner = self
            .inner
            .lock()
            .expect("Matrix OAuth grant registry poisoned");
        if inner.account_owners.get(&account_id) == Some(&flow_id) {
            inner.account_owners.remove(&account_id);
        }
    }

    fn flow(&self, account_id: Uuid, flow_id: Uuid) -> Result<Arc<Flow>, MatrixOAuthGrantError> {
        self.reap_expired();
        self.inner
            .lock()
            .expect("Matrix OAuth grant registry poisoned")
            .flows
            .get(&flow_id)
            .filter(|flow| flow.snapshot().account_id == account_id)
            .cloned()
            .ok_or(MatrixOAuthGrantError::NotFound(flow_id))
    }

    fn release_account_owner(&self, account_id: Uuid, flow_id: Uuid) {
        let mut inner = self
            .inner
            .lock()
            .expect("Matrix OAuth grant registry poisoned");
        if inner.account_owners.get(&account_id) == Some(&flow_id) {
            inner.account_owners.remove(&account_id);
        }
    }

    pub(crate) fn cancel_account(&self, account_id: Uuid) {
        let flow = {
            let mut inner = self
                .inner
                .lock()
                .expect("Matrix OAuth grant registry poisoned");
            let Some(flow_id) = inner.account_owners.remove(&account_id) else {
                return;
            };
            inner.flows.get(&flow_id).cloned()
        };
        if let Some(flow) = flow {
            if flow.cancel() {
                tracing::info!(
                    account_id = %account_id,
                    flow_id = %flow.snapshot().flow_id,
                    role = "grant",
                    "Matrix OAuth QR grant cancelled by account lifecycle"
                );
            }
        }
    }

    /// Cancel identity-lock-owning secret flows before a lifecycle operation
    /// waits for that lock.
    ///
    /// Logout, deletion, and supervised-client replacement all use this one
    /// barrier. Future secret-bearing flows that hold the identity lock must be
    /// added here, not rediscovered at each lifecycle call site.
    pub(crate) async fn cancel_before_identity_lock(
        &self,
        account_id: Uuid,
        identity_lock: IdentityLock,
    ) -> OwnedMutexGuard<()> {
        self.cancel_account(account_id);
        identity_lock.lock_owned().await
    }

    pub(crate) async fn cancel_before_identity_lock_or_cancel(
        &self,
        account_id: Uuid,
        identity_lock: IdentityLock,
        cancel: &CancellationToken,
    ) -> Option<OwnedMutexGuard<()>> {
        tokio::select! {
            guard = self.cancel_before_identity_lock(account_id, identity_lock) => Some(guard),
            _ = cancel.cancelled() => None,
        }
    }

    fn reap_expired(&self) {
        self.reap_expired_at(Instant::now());
    }

    fn reap_expired_at(&self, now: Instant) {
        self.inner
            .lock()
            .expect("Matrix OAuth grant registry poisoned")
            .flows
            .retain(|_, flow| {
                flow.mutable
                    .lock()
                    .expect("Matrix OAuth grant flow poisoned")
                    .lease
                    .is_retained_at(now)
            });
    }
}

struct MatrixOAuthGrantInner {
    store: Store,
    config: SyncConfig,
    manager: ClientManager,
    locks: IdentityLocks,
    registry: MatrixOAuthGrantRegistry,
    tracker: TaskTracker,
    engine_cancel: CancellationToken,
}

#[derive(Clone)]
pub struct MatrixOAuthGrantEngine {
    inner: Arc<MatrixOAuthGrantInner>,
}

impl MatrixOAuthGrantEngine {
    pub(crate) fn new(
        store: Store,
        config: SyncConfig,
        manager: ClientManager,
        locks: IdentityLocks,
        registry: MatrixOAuthGrantRegistry,
        tracker: TaskTracker,
        engine_cancel: CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(MatrixOAuthGrantInner {
                store,
                config,
                manager,
                locks,
                registry,
                tracker,
                engine_cancel,
            }),
        }
    }

    pub(crate) fn registry(&self) -> MatrixOAuthGrantRegistry {
        self.inner.registry.clone()
    }

    pub async fn create(
        &self,
        account_id: Uuid,
        presentation: MatrixOAuthGrantPresentation,
    ) -> Result<MatrixOAuthGrantState, MatrixOAuthGrantError> {
        let (flow_id, permit) = self.inner.registry.reserve(account_id)?;
        let result = self
            .create_under_reservation(flow_id, account_id, presentation, permit)
            .await;
        if result.is_err() {
            self.inner.registry.abandon_reservation(account_id, flow_id);
        }
        result
    }

    async fn create_under_reservation(
        &self,
        flow_id: Uuid,
        account_id: Uuid,
        presentation: MatrixOAuthGrantPresentation,
        permit: OwnedSemaphorePermit,
    ) -> Result<MatrixOAuthGrantState, MatrixOAuthGrantError> {
        let account = self
            .inner
            .store
            .get_account(account_id)
            .await
            .map_err(|error| {
                log_store_failure(flow_id, account_id, "create_lookup", &error);
                MatrixOAuthGrantError::Internal
            })?
            .ok_or(MatrixOAuthGrantError::AccountNotFound(account_id))?;
        let identity_lock = lock_for(&self.locks(), &account.user_id, &account.homeserver_url);
        let _guard = identity_lock.clone().lock_owned().await;
        let (account, client) = self.validate_authority(account_id, None).await?;
        self.validate_exportable(flow_id, account_id, &client)
            .await?;
        let expected_user_id = account.user_id;

        let (scan_tx, scan_rx) = oneshot::channel();
        let (check_code_tx, check_code_rx) = oneshot::channel();
        let flow = Arc::new(Flow {
            mutable: Mutex::new(FlowMutable {
                state: MatrixOAuthGrantState::new(flow_id, account_id, presentation),
                scan_tx: Some(scan_tx),
                check_code_tx: Some(check_code_tx),
                lease: FlowLease::new(permit),
            }),
            cancel: self.inner.engine_cancel.child_token(),
            identity_lock,
        });
        self.inner.registry.commit_reservation(flow.clone())?;

        let driver = self.clone();
        let task_flow = flow.clone();
        let span = tracing::info_span!(
            "matrix_oauth_qr_grant",
            %flow_id,
            %account_id,
            %expected_user_id,
            role = "grant",
            presentation = presentation.as_str()
        );
        let task_expected_user_id = expected_user_id.clone();
        self.inner.tracker.spawn(
            async move {
                driver
                    .drive(task_flow, scan_rx, check_code_rx, task_expected_user_id)
                    .await;
            }
            .instrument(span),
        );
        tracing::info!(
            %flow_id,
            %account_id,
            %expected_user_id,
            role = "grant",
            presentation = presentation.as_str(),
            "Matrix OAuth QR grant flow created"
        );
        Ok(flow.snapshot())
    }

    fn locks(&self) -> IdentityLocks {
        self.inner.locks.clone()
    }

    async fn validate_authority(
        &self,
        account_id: Uuid,
        expected_client: Option<&Client>,
    ) -> Result<(axon_store::Account, Client), MatrixOAuthGrantError> {
        let account = self.load_active_account(account_id).await?;
        let client = self
            .validate_current_trusted_client(account_id, expected_client)
            .await?;
        Ok((account, client))
    }

    async fn load_active_account(
        &self,
        account_id: Uuid,
    ) -> Result<axon_store::Account, MatrixOAuthGrantError> {
        let account = self
            .inner
            .store
            .get_account(account_id)
            .await
            .map_err(|error| {
                log_store_failure(Uuid::nil(), account_id, "authority_lookup", &error);
                MatrixOAuthGrantError::Internal
            })?
            .ok_or(MatrixOAuthGrantError::AccountNotFound(account_id))?;
        require_active_account(account_id, account.state)?;
        Ok(account)
    }

    async fn validate_current_trusted_client(
        &self,
        account_id: Uuid,
        expected_client: Option<&Client>,
    ) -> Result<Client, MatrixOAuthGrantError> {
        let client = tokio::time::timeout(
            CLIENT_TIMEOUT,
            self.inner.manager.get_or_connect(account_id),
        )
        .await
        .map_err(|_| MatrixOAuthGrantError::Unavailable)?
        .map_err(|_| MatrixOAuthGrantError::Unavailable)?;
        if let Some(expected) = expected_client {
            if !same_client_run(
                expected.user_id().map(|id| id.as_str()),
                expected.device_id().map(|id| id.as_str()),
                client.user_id().map(|id| id.as_str()),
                client.device_id().map(|id| id.as_str()),
            ) {
                return Err(MatrixOAuthGrantError::AccountNotActive(account_id));
            }
        }
        let trusted = tokio::time::timeout(TRUST_TIMEOUT, derive_verified(&client))
            .await
            .unwrap_or(false);
        require_trusted_device(trusted)?;
        Ok(client)
    }

    async fn validate_poll_authority(
        &self,
        account_id: Uuid,
        expected_client: &Client,
        active_account: &mut ActiveAccountRecheck,
    ) -> Result<(), MatrixOAuthGrantError> {
        if active_account.is_due(Instant::now()) {
            self.load_active_account(account_id).await?;
            active_account.record_success(Instant::now());
        }
        self.validate_current_trusted_client(account_id, Some(expected_client))
            .await?;
        Ok(())
    }

    async fn validate_exportable(
        &self,
        flow_id: Uuid,
        account_id: Uuid,
        client: &Client,
    ) -> Result<(), MatrixOAuthGrantError> {
        let database_path = self.inner.config.data_dir.join(account_id.to_string());
        let passphrase = self.inner.config.store_key.as_deref();
        let export = async {
            matrix_sdk::encryption::export_secrets_bundle_from_store(database_path, passphrase)
                .await
                .map_err(|_| ())?
                .filter(|(user_id, _)| Some(user_id.as_ref()) == client.user_id())
                .map(|_| ())
                .ok_or(())
        };
        match tokio::time::timeout(EXPORT_TIMEOUT, export).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(())) => {
                tracing::warn!(
                    %flow_id,
                    %account_id,
                    role = "grant",
                    error_class = "secrets_unavailable",
                    "Matrix OAuth QR grant preflight could not export secrets"
                );
                Err(MatrixOAuthGrantError::SecretsUnavailable)
            }
            Err(_) => {
                tracing::warn!(
                    %flow_id,
                    %account_id,
                    role = "grant",
                    error_class = "timeout",
                    "Matrix OAuth QR grant secrets preflight timed out"
                );
                Err(MatrixOAuthGrantError::Unavailable)
            }
        }
    }

    pub fn get(
        &self,
        account_id: Uuid,
        flow_id: Uuid,
    ) -> Result<MatrixOAuthGrantState, MatrixOAuthGrantError> {
        Ok(self.inner.registry.flow(account_id, flow_id)?.snapshot())
    }

    pub fn submit_scan(
        &self,
        account_id: Uuid,
        flow_id: Uuid,
        qr_code_data: &str,
    ) -> Result<MatrixOAuthGrantState, MatrixOAuthGrantError> {
        let parsed = parse_grant_qr_payload(qr_code_data).map_err(map_acquire_input)?;
        let flow = self.inner.registry.flow(account_id, flow_id)?;
        flow.accept_scan(parsed)
    }

    pub fn submit_check_code(
        &self,
        account_id: Uuid,
        flow_id: Uuid,
        check_code: &str,
    ) -> Result<MatrixOAuthGrantState, MatrixOAuthGrantError> {
        let value = parse_check_code(check_code).map_err(map_acquire_input)?;
        let flow = self.inner.registry.flow(account_id, flow_id)?;
        flow.accept_check_code(value)
    }

    pub fn cancel(&self, account_id: Uuid, flow_id: Uuid) {
        let Ok(flow) = self.inner.registry.flow(account_id, flow_id) else {
            return;
        };
        if flow.cancel() {
            self.inner
                .registry
                .release_account_owner(account_id, flow_id);
            tracing::info!(
                %flow_id,
                %account_id,
                role = "grant",
                presentation = flow.snapshot().presentation.as_str(),
                "Matrix OAuth QR grant flow cancelled"
            );
        }
    }

    pub(crate) async fn reap_loop(self, cancel: CancellationToken) {
        run_reaper(cancel, || self.inner.registry.reap_expired()).await;
    }

    async fn drive(
        &self,
        flow: Arc<Flow>,
        scan_rx: oneshot::Receiver<QrCodeData>,
        check_code_rx: oneshot::Receiver<u8>,
        expected_user_id: String,
    ) {
        let state = flow.snapshot();
        let deadline = tokio::time::Instant::now() + FLOW_TTL;
        let result = self
            .drive_under_identity_lock(flow.clone(), scan_rx, check_code_rx, deadline)
            .await;
        let (stage, failure) = match result {
            Ok(()) => (MatrixOAuthGrantStage::Done, None),
            Err(DriverFailure::Cancelled) => (MatrixOAuthGrantStage::Cancelled, None),
            Err(error) => (MatrixOAuthGrantStage::Failed, Some(error)),
        };
        let code = failure.as_ref().map(DriverFailure::code);
        if flow.terminal(stage, code) {
            self.inner
                .registry
                .release_account_owner(state.account_id, state.flow_id);
            let final_state = flow.snapshot();
            match final_state.stage {
                MatrixOAuthGrantStage::Done => tracing::info!(
                    flow_id = %state.flow_id,
                    account_id = %state.account_id,
                    %expected_user_id,
                    role = "grant",
                    presentation = state.presentation.as_str(),
                    stage = "done",
                    "Matrix OAuth QR grant completed"
                ),
                MatrixOAuthGrantStage::Failed => log_terminal_failure(
                    &state,
                    &expected_user_id,
                    failure.as_ref().expect("failed grant has a failure"),
                ),
                _ => {}
            }
        }
    }

    async fn drive_under_identity_lock(
        &self,
        flow: Arc<Flow>,
        scan_rx: oneshot::Receiver<QrCodeData>,
        check_code_rx: oneshot::Receiver<u8>,
        deadline: tokio::time::Instant,
    ) -> Result<(), DriverFailure> {
        let _guard = tokio::select! {
            _ = flow.cancel.cancelled() => return Err(DriverFailure::Cancelled),
            _ = tokio::time::sleep_until(deadline) => return Err(DriverFailure::Timeout),
            guard = flow.identity_lock.clone().lock_owned() => guard,
        };
        let state = flow.snapshot();
        let (_, client) = self
            .validate_authority(state.account_id, None)
            .await
            .map_err(classify_authority_error)?;

        match state.presentation {
            MatrixOAuthGrantPresentation::Scan => {
                let qr = tokio::select! {
                    _ = flow.cancel.cancelled() => return Err(DriverFailure::Cancelled),
                    _ = tokio::time::sleep_until(deadline) => return Err(DriverFailure::Timeout),
                    qr = scan_rx => qr.map_err(|_| DriverFailure::Cancelled)?,
                };
                let oauth = client.oauth();
                let grant = oauth.grant_login_with_qr_code().scan(&qr);
                let progress = grant.subscribe_to_progress();
                self.drive_sdk_future(
                    &flow,
                    &client,
                    grant.into_future(),
                    progress,
                    check_code_rx,
                    deadline,
                )
                .await
            }
            MatrixOAuthGrantPresentation::Display => {
                let oauth = client.oauth();
                let grant = oauth.grant_login_with_qr_code().generate();
                let progress = grant.subscribe_to_progress();
                self.drive_sdk_future(
                    &flow,
                    &client,
                    grant.into_future(),
                    progress,
                    check_code_rx,
                    deadline,
                )
                .await
            }
        }
    }

    async fn drive_sdk_future<F, S, Q>(
        &self,
        flow: &Arc<Flow>,
        expected_client: &Client,
        mut grant: Pin<Box<F>>,
        progress: S,
        mut check_code_rx: oneshot::Receiver<u8>,
        deadline: tokio::time::Instant,
    ) -> Result<(), DriverFailure>
    where
        F: Future<Output = Result<(), QRCodeGrantLoginError>> + ?Sized,
        S: Stream<Item = GrantLoginProgress<Q>>,
        Q: GrantProgress,
    {
        let signal = Arc::new(PollSignal::default());
        let mut progress = Box::pin(progress);
        let mut check_sender: Option<CheckCodeSender> = None;
        let account_id = flow.snapshot().account_id;
        // The first SDK poll can follow a long wait for a scanned QR payload,
        // so it always gets an active-account read. Subsequent wakeups reuse
        // that result briefly while still revalidating client identity and
        // re-deriving trust before every poll.
        let mut active_account = ActiveAccountRecheck::due_now(Instant::now());

        loop {
            let notified = signal.notify.notified();

            // This is the second authorization derivation. The identity lock is
            // still held, and every poll capable of exporting/sending secrets is
            // preceded by this active/current/trusted check.
            let authority = async {
                self.validate_poll_authority(account_id, expected_client, &mut active_account)
                    .await
                    .map_err(classify_authority_error)
            };
            if let Poll::Ready(result) =
                authorize_and_poll_once(flow, deadline, grant.as_mut(), &signal, authority).await?
            {
                return result.map_err(classify_grant_error);
            }

            {
                let waker = waker_ref(&signal);
                let mut context = Context::from_waker(&waker);
                while let Poll::Ready(Some(update)) = progress.as_mut().poll_next(&mut context) {
                    check_sender = update.apply(flow, check_sender)?;
                }
            }

            tokio::select! {
                _ = flow.cancel.cancelled() => return Err(DriverFailure::Cancelled),
                _ = tokio::time::sleep_until(deadline) => return Err(DriverFailure::Timeout),
                _ = notified => {}
                code = &mut check_code_rx, if check_sender.is_some() => {
                    let code = code.map_err(|_| DriverFailure::Cancelled)?;
                    check_sender.take().expect("guarded check-code sender")
                        .send(code).await.map_err(|_| DriverFailure::InvalidCheckCode)?;
                }
            }
        }
    }
}

async fn authorize_and_poll_once<F, A>(
    flow: &Flow,
    deadline: tokio::time::Instant,
    grant: Pin<&mut F>,
    signal: &Arc<PollSignal>,
    authority: A,
) -> Result<Poll<F::Output>, DriverFailure>
where
    F: Future + ?Sized,
    A: Future<Output = Result<(), DriverFailure>>,
{
    authority.await?;
    // Cancellation may have arrived while the authority recheck was awaiting
    // the store or SDK. Never give the grant future one more poll after DELETE,
    // logout, deletion, eviction, shutdown, or expiry.
    let waker = waker_ref(signal);
    let mut context = Context::from_waker(&waker);
    poll_sdk_once(flow, deadline, grant, &mut context)
}

fn poll_sdk_once<F: Future + ?Sized>(
    flow: &Flow,
    deadline: tokio::time::Instant,
    grant: Pin<&mut F>,
    context: &mut Context<'_>,
) -> Result<Poll<F::Output>, DriverFailure> {
    if flow.cancel.is_cancelled() {
        return Err(DriverFailure::Cancelled);
    }
    if tokio::time::Instant::now() >= deadline {
        return Err(DriverFailure::Timeout);
    }
    Ok(grant.poll(context))
}

trait GrantProgress: Sized {
    fn apply(
        self,
        flow: &Arc<Flow>,
        check_sender: Option<CheckCodeSender>,
    ) -> Result<Option<CheckCodeSender>, DriverFailure>;
}

impl GrantProgress for QrProgress {
    fn apply(
        self,
        flow: &Arc<Flow>,
        check_sender: Option<CheckCodeSender>,
    ) -> Result<Option<CheckCodeSender>, DriverFailure> {
        publish_stable_update(
            flow,
            StableGrantUpdate::CheckCodeToDisplay(format!("{:02}", self.check_code.to_digit())),
        );
        Ok(check_sender)
    }
}

impl GrantProgress for GeneratedQrProgress {
    fn apply(
        self,
        flow: &Arc<Flow>,
        _check_sender: Option<CheckCodeSender>,
    ) -> Result<Option<CheckCodeSender>, DriverFailure> {
        match self {
            GeneratedQrProgress::QrReady(qr) => {
                publish_stable_update(flow, StableGrantUpdate::QrReady(qr.to_base64()));
                Ok(None)
            }
            GeneratedQrProgress::QrScanned(sender) => {
                publish_stable_update(flow, StableGrantUpdate::CheckCodeRequired);
                Ok(Some(sender))
            }
        }
    }
}

trait ApplyGrantProgress<Q> {
    fn apply(
        self,
        flow: &Arc<Flow>,
        check_sender: Option<CheckCodeSender>,
    ) -> Result<Option<CheckCodeSender>, DriverFailure>;
}

impl<Q: GrantProgress> ApplyGrantProgress<Q> for GrantLoginProgress<Q> {
    fn apply(
        self,
        flow: &Arc<Flow>,
        check_sender: Option<CheckCodeSender>,
    ) -> Result<Option<CheckCodeSender>, DriverFailure> {
        match self {
            GrantLoginProgress::Starting => Ok(check_sender),
            GrantLoginProgress::EstablishingSecureChannel(progress) => {
                progress.apply(flow, check_sender)
            }
            GrantLoginProgress::WaitingForAuth { verification_uri } => {
                validate_qr_url(&verification_uri).map_err(|_| DriverFailure::Upstream)?;
                publish_stable_update(
                    flow,
                    StableGrantUpdate::WaitingForAuthorization(verification_uri.to_string()),
                );
                Ok(check_sender)
            }
            GrantLoginProgress::SyncingSecrets => {
                publish_stable_update(flow, StableGrantUpdate::SyncingSecrets);
                Ok(check_sender)
            }
            GrantLoginProgress::Done => Ok(check_sender),
        }
    }
}

enum StableGrantUpdate {
    QrReady(String),
    CheckCodeToDisplay(String),
    CheckCodeRequired,
    WaitingForAuthorization(String),
    SyncingSecrets,
}

fn publish_stable_update(flow: &Flow, update: StableGrantUpdate) {
    flow.update(|state| match update {
        StableGrantUpdate::QrReady(qr_code_data) => {
            state.set_stage(MatrixOAuthGrantStage::QrReady);
            state.qr_code_data = Some(qr_code_data);
        }
        StableGrantUpdate::CheckCodeToDisplay(check_code) => {
            state.set_stage(MatrixOAuthGrantStage::CheckCodeToDisplay);
            state.check_code = Some(check_code);
        }
        StableGrantUpdate::CheckCodeRequired => {
            state.set_stage(MatrixOAuthGrantStage::CheckCodeRequired);
        }
        StableGrantUpdate::WaitingForAuthorization(verification_uri) => {
            state.set_stage(MatrixOAuthGrantStage::WaitingForAuthorization);
            state.verification_uri = Some(verification_uri);
        }
        StableGrantUpdate::SyncingSecrets => {
            state.set_stage(MatrixOAuthGrantStage::SyncingSecrets);
        }
    });
}

#[derive(Default)]
struct PollSignal {
    notify: tokio::sync::Notify,
}

impl ArcWake for PollSignal {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.notify.notify_one();
    }
}

#[derive(Debug)]
enum DriverFailure {
    Cancelled,
    Timeout,
    Unsupported,
    InvalidCheckCode,
    RendezvousExpired,
    DeviceConflict,
    DeviceNotFound,
    TrustLost,
    SecretsUnavailable,
    Upstream,
    Internal,
}

impl DriverFailure {
    fn code(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::Unsupported => "unsupported",
            Self::InvalidCheckCode => "invalid_check_code",
            Self::RendezvousExpired => "rendezvous_expired",
            Self::DeviceConflict => "device_conflict",
            Self::DeviceNotFound => "device_not_found",
            Self::TrustLost => "trust_lost",
            Self::SecretsUnavailable => "secrets_unavailable",
            Self::Upstream => "upstream",
            Self::Internal => "internal",
        }
    }
}

fn log_terminal_failure(
    state: &MatrixOAuthGrantState,
    expected_user_id: &str,
    failure: &DriverFailure,
) {
    if matches!(failure, DriverFailure::DeviceNotFound) {
        tracing::warn!(
            flow_id = %state.flow_id,
            account_id = %state.account_id,
            expected_user_id,
            role = "grant",
            presentation = state.presentation.as_str(),
            stage = "failed",
            error_code = failure.code(),
            possible_cause = "wrong_account_or_device_provisioning",
            "Matrix OAuth QR grant device was not registered for the expected account"
        );
    } else {
        tracing::warn!(
            flow_id = %state.flow_id,
            account_id = %state.account_id,
            expected_user_id,
            role = "grant",
            presentation = state.presentation.as_str(),
            stage = "failed",
            error_code = failure.code(),
            "Matrix OAuth QR grant failed"
        );
    }
}

fn require_active_account(
    account_id: Uuid,
    state: AccountState,
) -> Result<(), MatrixOAuthGrantError> {
    match state {
        AccountState::Active => Ok(()),
        AccountState::Deactivated => Err(MatrixOAuthGrantError::AccountNotActive(account_id)),
        AccountState::Deleting => Err(MatrixOAuthGrantError::AccountBeingDeleted(account_id)),
    }
}

fn require_trusted_device(trusted: bool) -> Result<(), MatrixOAuthGrantError> {
    if trusted {
        Ok(())
    } else {
        Err(MatrixOAuthGrantError::DeviceNotTrusted)
    }
}

fn same_client_run(
    expected_user_id: Option<&str>,
    expected_device_id: Option<&str>,
    current_user_id: Option<&str>,
    current_device_id: Option<&str>,
) -> bool {
    expected_user_id == current_user_id && expected_device_id == current_device_id
}

fn map_acquire_input(error: MatrixOAuthAcquireError) -> MatrixOAuthGrantError {
    match error {
        MatrixOAuthAcquireError::InvalidInput(message) => {
            MatrixOAuthGrantError::InvalidInput(message)
        }
        _ => MatrixOAuthGrantError::InvalidInput("QR grant input is invalid"),
    }
}

fn classify_authority_error(error: MatrixOAuthGrantError) -> DriverFailure {
    match error {
        MatrixOAuthGrantError::DeviceNotTrusted => DriverFailure::TrustLost,
        MatrixOAuthGrantError::SecretsUnavailable => DriverFailure::SecretsUnavailable,
        MatrixOAuthGrantError::Unavailable => DriverFailure::Timeout,
        MatrixOAuthGrantError::AccountNotFound(_)
        | MatrixOAuthGrantError::AccountNotActive(_)
        | MatrixOAuthGrantError::AccountBeingDeleted(_) => DriverFailure::Cancelled,
        MatrixOAuthGrantError::Internal => DriverFailure::Internal,
        _ => DriverFailure::Internal,
    }
}

fn classify_grant_error(error: QRCodeGrantLoginError) -> DriverFailure {
    let failure = match &error {
        QRCodeGrantLoginError::MissingSecretsBackup(_) => DriverFailure::SecretsUnavailable,
        QRCodeGrantLoginError::InvalidCheckCode => DriverFailure::InvalidCheckCode,
        QRCodeGrantLoginError::NotFound => DriverFailure::RendezvousExpired,
        QRCodeGrantLoginError::UnsupportedProtocol(_) => DriverFailure::Unsupported,
        QRCodeGrantLoginError::DeviceIDAlreadyInUse => DriverFailure::DeviceConflict,
        QRCodeGrantLoginError::DeviceNotFound => DriverFailure::DeviceNotFound,
        QRCodeGrantLoginError::SecureChannel(SecureChannelError::RendezvousChannel(error))
            if error
                .as_client_api_error()
                .is_some_and(|e| e.is_endpoint_not_implemented()) =>
        {
            DriverFailure::Unsupported
        }
        QRCodeGrantLoginError::SecureChannel(_)
        | QRCodeGrantLoginError::UnexpectedMessage { .. }
        | QRCodeGrantLoginError::LoginFailure { .. }
        | QRCodeGrantLoginError::Unknown(_) => DriverFailure::Upstream,
    };
    tracing::warn!(
        role = "grant",
        error_class = failure.code(),
        "Matrix OAuth QR grant protocol step failed"
    );
    failure
}

fn log_store_failure(
    flow_id: Uuid,
    account_id: Uuid,
    phase: &'static str,
    error: &axon_store::StoreError,
) {
    let error_class = match error {
        axon_store::StoreError::Sqlx(_) => "database",
        axon_store::StoreError::Migrate(_) | axon_store::StoreError::EmbeddedMigration(_) => {
            "migration"
        }
        _ => "store",
    };
    tracing::error!(
        %flow_id,
        %account_id,
        role = "grant",
        phase,
        error_class,
        "Matrix OAuth QR grant storage operation failed"
    );
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Barrier,
        },
    };

    use tracing_subscriber::fmt::MakeWriter;

    use super::*;
    use crate::matrix_oauth_flow::TERMINAL_RETENTION;
    use matrix_sdk::authentication::oauth::qrcode::LoginProtocolType;

    const VALID_LOGIN_QR: &str = concat!(
        "TUFUUklYAgPYhmhqshl7eA4wCp1KIUdIBwDXkp85qzG55RQ3AkjtawBHaHR0",
        "cHM6Ly9yZW5kZXp2b3VzLmxhYi5lbGVtZW50LmRldi9lOGRhNjM1NS01NTBi",
        "LTRhMzItYTE5My0xNjE5ZDk4MzA2Njg=",
    );
    const VALID_RECIPROCATE_QR: &str = concat!(
        "TUFUUklYAgS0yzZ1QVpQ1jlnoxWX3d5jrWRFfELxjS2gN7pz9y+3PABaaHR0",
        "cHM6Ly9zeW5hcHNlLW9pZGMubGFiLmVsZW1lbnQuZGV2L19zeW5hcHNlL2Ns",
        "aWVudC9yZW5kZXp2b3VzLzAxSFg5SzAwUTFINktQRDQ3RUc0RzFUM1hHACVo",
        "dHRwczovL3N5bmFwc2Utb2lkYy5sYWIuZWxlbWVudC5kZXYv",
    );

    fn test_flow(
        account_id: Uuid,
        flow_id: Uuid,
        presentation: MatrixOAuthGrantPresentation,
    ) -> (
        Arc<Flow>,
        oneshot::Receiver<QrCodeData>,
        oneshot::Receiver<u8>,
        Arc<Semaphore>,
    ) {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().try_acquire_owned().unwrap();
        let (flow, scan_rx, check_code_rx) =
            test_flow_with_permit(account_id, flow_id, presentation, permit);
        (flow, scan_rx, check_code_rx, semaphore)
    }

    fn test_flow_with_permit(
        account_id: Uuid,
        flow_id: Uuid,
        presentation: MatrixOAuthGrantPresentation,
        permit: OwnedSemaphorePermit,
    ) -> (
        Arc<Flow>,
        oneshot::Receiver<QrCodeData>,
        oneshot::Receiver<u8>,
    ) {
        let (scan_tx, scan_rx) = oneshot::channel();
        let (check_code_tx, check_code_rx) = oneshot::channel();
        let flow = Arc::new(Flow {
            mutable: Mutex::new(FlowMutable {
                state: MatrixOAuthGrantState::new(flow_id, account_id, presentation),
                scan_tx: Some(scan_tx),
                check_code_tx: Some(check_code_tx),
                lease: FlowLease::new(permit),
            }),
            cancel: CancellationToken::new(),
            identity_lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        (flow, scan_rx, check_code_rx)
    }

    fn retain_terminal_flows(
        registry: &MatrixOAuthGrantRegistry,
        count: usize,
        terminal_at: Instant,
    ) {
        let mut inner = registry
            .inner
            .lock()
            .expect("Matrix OAuth grant registry poisoned");
        for _ in 0..count {
            let account_id = Uuid::new_v4();
            let flow_id = Uuid::new_v4();
            let (flow, _, _, _) =
                test_flow(account_id, flow_id, MatrixOAuthGrantPresentation::Display);
            assert!(flow.terminal_at(MatrixOAuthGrantStage::Done, None, terminal_at));
            inner.flows.insert(flow_id, flow);
        }
    }

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CapturedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("captured log poisoned").extend(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CapturedWriter(self.0.clone())
        }
    }

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("captured log poisoned").clone())
                .expect("logs are UTF-8")
        }
    }

    #[test]
    fn stages_clear_data_from_the_previous_stage() {
        let mut state = MatrixOAuthGrantState::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Display,
        );
        state.set_stage(MatrixOAuthGrantStage::QrReady);
        state.qr_code_data = Some("opaque".to_owned());
        state.set_stage(MatrixOAuthGrantStage::CheckCodeRequired);
        assert!(state.qr_code_data.is_none());
        assert!(state.check_code.is_none());
        assert!(state.verification_uri.is_none());
        assert!(state.error_code.is_none());
    }

    #[test]
    fn active_and_trusted_authority_is_required_before_flow_creation() {
        let account_id = Uuid::new_v4();
        assert!(require_active_account(account_id, AccountState::Active).is_ok());
        assert!(matches!(
            require_active_account(account_id, AccountState::Deactivated),
            Err(MatrixOAuthGrantError::AccountNotActive(id)) if id == account_id
        ));
        assert!(matches!(
            require_active_account(account_id, AccountState::Deleting),
            Err(MatrixOAuthGrantError::AccountBeingDeleted(id)) if id == account_id
        ));
        assert!(require_trusted_device(true).is_ok());
        assert!(matches!(
            require_trusted_device(false),
            Err(MatrixOAuthGrantError::DeviceNotTrusted)
        ));
    }

    #[test]
    fn active_account_reads_are_cached_for_a_bounded_interval() {
        let start = Instant::now();
        let mut recheck = ActiveAccountRecheck::due_now(start);
        assert!(recheck.is_due(start));

        recheck.record_success(start);
        assert!(!recheck.is_due(start));
        assert!(!recheck.is_due(start + ACTIVE_ACCOUNT_RECHECK_INTERVAL - Duration::from_millis(1)));
        assert!(recheck.is_due(start + ACTIVE_ACCOUNT_RECHECK_INTERVAL));

        let second_check = start + ACTIVE_ACCOUNT_RECHECK_INTERVAL;
        recheck.record_success(second_check);
        assert!(!recheck.is_due(second_check));
    }

    #[test]
    fn client_replacement_invalidates_the_current_grant_run() {
        assert!(same_client_run(
            Some("@alice:example.org"),
            Some("AXON"),
            Some("@alice:example.org"),
            Some("AXON"),
        ));
        assert!(!same_client_run(
            Some("@alice:example.org"),
            Some("AXON"),
            Some("@alice:example.org"),
            Some("REPLACEMENT"),
        ));
        assert!(!same_client_run(
            Some("@alice:example.org"),
            Some("AXON"),
            Some("@mallory:example.org"),
            Some("AXON"),
        ));
        assert!(!same_client_run(
            Some("@alice:example.org"),
            Some("AXON"),
            None,
            None,
        ));
    }

    #[test]
    fn both_presentations_publish_only_the_current_stable_stage_data() {
        let account_id = Uuid::new_v4();
        let (display, _, _, _) = test_flow(
            account_id,
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Display,
        );
        publish_stable_update(&display, StableGrantUpdate::QrReady("qr".to_owned()));
        let state = display.snapshot();
        assert_eq!(state.stage, MatrixOAuthGrantStage::QrReady);
        assert_eq!(state.qr_code_data.as_deref(), Some("qr"));
        assert!(state.check_code.is_none());

        publish_stable_update(&display, StableGrantUpdate::CheckCodeRequired);
        let state = display.snapshot();
        assert_eq!(state.stage, MatrixOAuthGrantStage::CheckCodeRequired);
        assert!(state.qr_code_data.is_none());

        publish_stable_update(
            &display,
            StableGrantUpdate::WaitingForAuthorization(
                "https://auth.example.org/device".to_owned(),
            ),
        );
        let state = display.snapshot();
        assert_eq!(state.stage, MatrixOAuthGrantStage::WaitingForAuthorization);
        assert_eq!(
            state.verification_uri.as_deref(),
            Some("https://auth.example.org/device")
        );

        publish_stable_update(&display, StableGrantUpdate::SyncingSecrets);
        let state = display.snapshot();
        assert_eq!(state.stage, MatrixOAuthGrantStage::SyncingSecrets);
        assert!(state.verification_uri.is_none());
        assert!(display.terminal(MatrixOAuthGrantStage::Done, None));
        assert_eq!(display.snapshot().stage, MatrixOAuthGrantStage::Done);

        let (scan, _, _, _) = test_flow(
            account_id,
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Scan,
        );
        publish_stable_update(
            &scan,
            StableGrantUpdate::CheckCodeToDisplay("07".to_owned()),
        );
        let state = scan.snapshot();
        assert_eq!(state.stage, MatrixOAuthGrantStage::CheckCodeToDisplay);
        assert_eq!(state.check_code.as_deref(), Some("07"));
        assert!(state.qr_code_data.is_none());
        publish_stable_update(
            &scan,
            StableGrantUpdate::WaitingForAuthorization(
                "https://auth.example.org/device".to_owned(),
            ),
        );
        assert!(scan.snapshot().check_code.is_none());
    }

    #[tokio::test]
    async fn qr_and_check_code_inputs_are_single_use_and_stage_scoped() {
        let account_id = Uuid::new_v4();
        let (scan, mut scan_rx, _, _) = test_flow(
            account_id,
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Scan,
        );
        let qr = parse_grant_qr_payload(VALID_LOGIN_QR).expect("valid login QR");
        scan.accept_scan(qr).expect("first scan accepted");
        scan_rx.try_recv().expect("driver receives parsed QR");
        let replay = parse_grant_qr_payload(VALID_LOGIN_QR).expect("valid login QR");
        assert!(matches!(
            scan.accept_scan(replay),
            Err(MatrixOAuthGrantError::WrongStage)
        ));

        let (display, _, mut check_code_rx, _) = test_flow(
            account_id,
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Display,
        );
        publish_stable_update(&display, StableGrantUpdate::CheckCodeRequired);
        display.accept_check_code(42).expect("first code accepted");
        assert_eq!(check_code_rx.try_recv().expect("driver receives code"), 42);
        assert!(matches!(
            display.accept_check_code(42),
            Err(MatrixOAuthGrantError::WrongStage)
        ));

        let wrong_presentation = parse_grant_qr_payload(VALID_LOGIN_QR).unwrap();
        assert!(matches!(
            display.accept_scan(wrong_presentation),
            Err(MatrixOAuthGrantError::WrongStage)
        ));
        assert!(matches!(
            scan.accept_check_code(42),
            Err(MatrixOAuthGrantError::WrongStage)
        ));
    }

    #[test]
    fn scanned_grant_qr_requires_the_new_devices_login_intent() {
        let qr = parse_grant_qr_payload(VALID_LOGIN_QR).expect("valid login-intent QR");
        assert_eq!(
            qr.intent(),
            matrix_sdk::authentication::oauth::qrcode::QrCodeIntent::Login
        );
        assert!(matches!(
            parse_grant_qr_payload(VALID_RECIPROCATE_QR),
            Err(MatrixOAuthAcquireError::InvalidInput(_))
        ));
    }

    #[test]
    fn malformed_and_oversized_grant_inputs_fail_before_flow_use() {
        for invalid_qr in [
            "not base64 or QR data".to_owned(),
            "A".repeat(crate::matrix_oauth_acquire::MAX_QR_BASE64_BYTES + 1),
        ] {
            assert!(matches!(
                parse_grant_qr_payload(&invalid_qr).map_err(map_acquire_input),
                Err(MatrixOAuthGrantError::InvalidInput(_))
            ));
        }
        for invalid_code in ["", "1", "100", "a1", "１２"] {
            assert!(matches!(
                parse_check_code(invalid_code).map_err(map_acquire_input),
                Err(MatrixOAuthGrantError::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn one_terminal_transition_consumes_the_flow_owner() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().try_acquire_owned().unwrap();
        let (scan_tx, _) = oneshot::channel();
        let (check_code_tx, _) = oneshot::channel();
        let flow = Flow {
            mutable: Mutex::new(FlowMutable {
                state: MatrixOAuthGrantState::new(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    MatrixOAuthGrantPresentation::Scan,
                ),
                scan_tx: Some(scan_tx),
                check_code_tx: Some(check_code_tx),
                lease: FlowLease::new(permit),
            }),
            cancel: CancellationToken::new(),
            identity_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        assert!(flow.cancel());
        assert!(!flow.terminal(MatrixOAuthGrantStage::Done, None));
        assert_eq!(flow.snapshot().stage, MatrixOAuthGrantStage::Cancelled);
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[test]
    fn stable_failure_codes_cover_security_boundaries() {
        let cases = [
            (DriverFailure::Cancelled, "cancelled"),
            (DriverFailure::Timeout, "timeout"),
            (DriverFailure::Unsupported, "unsupported"),
            (DriverFailure::InvalidCheckCode, "invalid_check_code"),
            (DriverFailure::RendezvousExpired, "rendezvous_expired"),
            (DriverFailure::TrustLost, "trust_lost"),
            (DriverFailure::SecretsUnavailable, "secrets_unavailable"),
            (DriverFailure::DeviceConflict, "device_conflict"),
            (DriverFailure::DeviceNotFound, "device_not_found"),
            (DriverFailure::Upstream, "upstream"),
            (DriverFailure::Internal, "internal"),
        ];
        for (failure, expected) in cases {
            assert_eq!(failure.code(), expected);
        }
    }

    #[test]
    fn sdk_failures_map_to_stable_non_secret_classifications() {
        assert!(matches!(
            classify_grant_error(QRCodeGrantLoginError::UnsupportedProtocol(
                LoginProtocolType::DeviceAuthorizationGrant,
            )),
            DriverFailure::Unsupported
        ));
        assert!(matches!(
            classify_grant_error(QRCodeGrantLoginError::InvalidCheckCode),
            DriverFailure::InvalidCheckCode
        ));
        assert!(matches!(
            classify_grant_error(QRCodeGrantLoginError::NotFound),
            DriverFailure::RendezvousExpired
        ));
        assert!(matches!(
            classify_grant_error(QRCodeGrantLoginError::DeviceIDAlreadyInUse),
            DriverFailure::DeviceConflict
        ));
        assert!(matches!(
            classify_grant_error(QRCodeGrantLoginError::DeviceNotFound),
            DriverFailure::DeviceNotFound
        ));
    }

    #[test]
    fn one_account_can_have_only_one_live_owner() {
        let registry = MatrixOAuthGrantRegistry::new();
        let account_id = Uuid::new_v4();
        let (flow_id, permit) = registry.reserve(account_id).unwrap();
        assert!(matches!(
            registry.reserve(account_id),
            Err(MatrixOAuthGrantError::FlowAlreadyExists)
        ));
        drop(permit);
        registry.abandon_reservation(account_id, flow_id);
        assert!(registry.reserve(account_id).is_ok());
    }

    #[test]
    fn global_active_capacity_is_bounded_and_recovers_when_a_reservation_is_released() {
        let registry = MatrixOAuthGrantRegistry::new();
        let mut reservations = Vec::new();
        for _ in 0..MAX_CONCURRENT_FLOWS {
            let account_id = Uuid::new_v4();
            let (flow_id, permit) = registry.reserve(account_id).unwrap();
            reservations.push((account_id, flow_id, permit));
        }
        assert!(matches!(
            registry.reserve(Uuid::new_v4()),
            Err(MatrixOAuthGrantError::Capacity)
        ));

        let (account_id, flow_id, permit) = reservations.pop().unwrap();
        registry.abandon_reservation(account_id, flow_id);
        drop(permit);
        assert!(registry.reserve(Uuid::new_v4()).is_ok());
    }

    #[test]
    fn retained_capacity_and_terminal_grace_period_are_bounded() {
        let registry = MatrixOAuthGrantRegistry::new();
        let now = Instant::now();
        retain_terminal_flows(&registry, MAX_RETAINED_FLOWS, now);

        assert!(matches!(
            registry.reserve(Uuid::new_v4()),
            Err(MatrixOAuthGrantError::Capacity)
        ));
        registry.reap_expired_at(now + TERMINAL_RETENTION - Duration::from_millis(1));
        assert_eq!(
            registry
                .inner
                .lock()
                .expect("Matrix OAuth grant registry poisoned")
                .flows
                .len(),
            MAX_RETAINED_FLOWS
        );
        registry.reap_expired_at(now + TERMINAL_RETENTION);
        assert!(registry
            .inner
            .lock()
            .expect("Matrix OAuth grant registry poisoned")
            .flows
            .is_empty());
        assert!(registry.reserve(Uuid::new_v4()).is_ok());
    }

    #[test]
    fn retained_capacity_race_is_reported_as_capacity() {
        let registry = MatrixOAuthGrantRegistry::new();
        let account_id = Uuid::new_v4();
        let (flow_id, permit) = registry.reserve(account_id).unwrap();
        let (flow, _, _) = test_flow_with_permit(
            account_id,
            flow_id,
            MatrixOAuthGrantPresentation::Display,
            permit,
        );
        retain_terminal_flows(&registry, MAX_RETAINED_FLOWS, Instant::now());

        assert!(matches!(
            registry.commit_reservation(flow),
            Err(MatrixOAuthGrantError::Capacity)
        ));
    }

    #[test]
    fn lifecycle_invalidated_reservation_is_temporarily_unavailable() {
        let registry = MatrixOAuthGrantRegistry::new();
        let account_id = Uuid::new_v4();
        let (flow_id, permit) = registry.reserve(account_id).unwrap();
        let (flow, _, _) = test_flow_with_permit(
            account_id,
            flow_id,
            MatrixOAuthGrantPresentation::Display,
            permit,
        );

        registry.cancel_account(account_id);

        assert!(matches!(
            registry.commit_reservation(flow),
            Err(MatrixOAuthGrantError::Unavailable)
        ));
    }

    #[test]
    fn a_fresh_registry_has_no_durable_or_wedged_account_owner() {
        let account_id = Uuid::new_v4();
        let old_registry = MatrixOAuthGrantRegistry::new();
        let _reservation = old_registry.reserve(account_id).unwrap();

        let restarted_registry = MatrixOAuthGrantRegistry::new();
        assert!(restarted_registry.reserve(account_id).is_ok());
    }

    #[test]
    fn lifecycle_cancellation_uses_the_http_visible_flow_owner() {
        let registry = MatrixOAuthGrantRegistry::new();
        let account_id = Uuid::new_v4();
        let (flow_id, permit) = registry.reserve(account_id).unwrap();
        let (scan_tx, _) = oneshot::channel();
        let (check_code_tx, _) = oneshot::channel();
        let flow = Arc::new(Flow {
            mutable: Mutex::new(FlowMutable {
                state: MatrixOAuthGrantState::new(
                    flow_id,
                    account_id,
                    MatrixOAuthGrantPresentation::Display,
                ),
                scan_tx: Some(scan_tx),
                check_code_tx: Some(check_code_tx),
                lease: FlowLease::new(permit),
            }),
            cancel: CancellationToken::new(),
            identity_lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        registry.commit_reservation(flow.clone()).unwrap();

        registry.cancel_account(account_id);

        assert_eq!(flow.snapshot().stage, MatrixOAuthGrantStage::Cancelled);
        assert!(flow.cancel.is_cancelled());
        assert!(registry.reserve(account_id).is_ok());
    }

    #[test]
    fn flow_lookup_never_crosses_account_scope() {
        let registry = MatrixOAuthGrantRegistry::new();
        let account_id = Uuid::new_v4();
        let (flow_id, permit) = registry.reserve(account_id).unwrap();
        let (scan_tx, _) = oneshot::channel();
        let (check_code_tx, _) = oneshot::channel();
        let flow = Arc::new(Flow {
            mutable: Mutex::new(FlowMutable {
                state: MatrixOAuthGrantState::new(
                    flow_id,
                    account_id,
                    MatrixOAuthGrantPresentation::Display,
                ),
                scan_tx: Some(scan_tx),
                check_code_tx: Some(check_code_tx),
                lease: FlowLease::new(permit),
            }),
            cancel: CancellationToken::new(),
            identity_lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        registry.commit_reservation(flow).unwrap();

        assert!(registry.flow(account_id, flow_id).is_ok());
        assert!(matches!(
            registry.flow(Uuid::new_v4(), flow_id),
            Err(MatrixOAuthGrantError::NotFound(id)) if id == flow_id
        ));
    }

    #[tokio::test]
    async fn lifecycle_cancellation_unblocks_the_identity_lock_before_eviction() {
        let registry = MatrixOAuthGrantRegistry::new();
        let account_id = Uuid::new_v4();
        let (flow_id, permit) = registry.reserve(account_id).unwrap();
        let (scan_tx, _) = oneshot::channel();
        let (check_code_tx, _) = oneshot::channel();
        let identity_lock = Arc::new(tokio::sync::Mutex::new(()));
        let flow = Arc::new(Flow {
            mutable: Mutex::new(FlowMutable {
                state: MatrixOAuthGrantState::new(
                    flow_id,
                    account_id,
                    MatrixOAuthGrantPresentation::Display,
                ),
                scan_tx: Some(scan_tx),
                check_code_tx: Some(check_code_tx),
                lease: FlowLease::new(permit),
            }),
            cancel: CancellationToken::new(),
            identity_lock: identity_lock.clone(),
        });
        registry.commit_reservation(flow.clone()).unwrap();

        let (locked_tx, locked_rx) = oneshot::channel();
        let driver_flow = flow.clone();
        let driver = tokio::spawn(async move {
            let _guard = driver_flow.identity_lock.clone().lock_owned().await;
            locked_tx.send(()).unwrap();
            driver_flow.cancel.cancelled().await;
        });
        locked_rx.await.unwrap();

        let _eviction_guard = tokio::time::timeout(
            Duration::from_secs(1),
            registry.cancel_before_identity_lock(account_id, identity_lock),
        )
        .await
        .expect("lifecycle eviction acquires the released identity lock");
        driver.await.unwrap();
        assert_eq!(flow.snapshot().stage, MatrixOAuthGrantStage::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_completion_and_cancellation_have_exactly_one_winner() {
        let (flow, _, _, semaphore) = test_flow(
            Uuid::new_v4(),
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Display,
        );
        let barrier = Arc::new(Barrier::new(3));
        let complete_flow = flow.clone();
        let complete_barrier = barrier.clone();
        let complete = tokio::task::spawn_blocking(move || {
            complete_barrier.wait();
            complete_flow.terminal(MatrixOAuthGrantStage::Done, None)
        });
        let cancel_flow = flow.clone();
        let cancel_barrier = barrier.clone();
        let cancel = tokio::task::spawn_blocking(move || {
            cancel_barrier.wait();
            cancel_flow.cancel()
        });
        barrier.wait();

        let complete_won = complete.await.unwrap();
        let cancel_won = cancel.await.unwrap();
        assert_ne!(complete_won, cancel_won);
        assert!(matches!(
            flow.snapshot().stage,
            MatrixOAuthGrantStage::Done | MatrixOAuthGrantStage::Cancelled
        ));
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[test]
    fn cancellation_is_checked_before_the_sdk_gets_another_poll() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.try_acquire_owned().unwrap();
        let (scan_tx, _) = oneshot::channel();
        let (check_code_tx, _) = oneshot::channel();
        let flow = Flow {
            mutable: Mutex::new(FlowMutable {
                state: MatrixOAuthGrantState::new(
                    Uuid::new_v4(),
                    Uuid::new_v4(),
                    MatrixOAuthGrantPresentation::Scan,
                ),
                scan_tx: Some(scan_tx),
                check_code_tx: Some(check_code_tx),
                lease: FlowLease::new(permit),
            }),
            cancel: CancellationToken::new(),
            identity_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        assert!(flow.cancel());

        let polls = Arc::new(AtomicUsize::new(0));
        let counted = polls.clone();
        let mut future = Box::pin(std::future::poll_fn(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Poll::<()>::Pending
        }));
        let signal = Arc::new(PollSignal::default());
        let waker = waker_ref(&signal);
        let mut context = Context::from_waker(&waker);

        assert!(matches!(
            poll_sdk_once(
                &flow,
                tokio::time::Instant::now() + Duration::from_secs(1),
                future.as_mut(),
                &mut context,
            ),
            Err(DriverFailure::Cancelled)
        ));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn trust_loss_prevents_the_sdk_future_from_reaching_the_secrets_boundary() {
        let (flow, _, _, _) = test_flow(
            Uuid::new_v4(),
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Display,
        );
        let polls = Arc::new(AtomicUsize::new(0));
        let counted = polls.clone();
        let mut future = Box::pin(std::future::poll_fn(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Poll::<()>::Pending
        }));
        let signal = Arc::new(PollSignal::default());

        let result = authorize_and_poll_once(
            &flow,
            tokio::time::Instant::now() + Duration::from_secs(1),
            future.as_mut(),
            &signal,
            async { Err(DriverFailure::TrustLost) },
        )
        .await;

        assert!(matches!(result, Err(DriverFailure::TrustLost)));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancellation_during_authority_recheck_prevents_the_next_sdk_poll() {
        let (flow, _, _, _) = test_flow(
            Uuid::new_v4(),
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Display,
        );
        let polls = Arc::new(AtomicUsize::new(0));
        let counted = polls.clone();
        let mut future = Box::pin(std::future::poll_fn(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Poll::<()>::Pending
        }));
        let signal = Arc::new(PollSignal::default());
        let authority_flow = flow.clone();

        let result = authorize_and_poll_once(
            &flow,
            tokio::time::Instant::now() + Duration::from_secs(1),
            future.as_mut(),
            &signal,
            async move {
                assert!(authority_flow.cancel());
                Ok(())
            },
        )
        .await;

        assert!(matches!(result, Err(DriverFailure::Cancelled)));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn every_sdk_poll_requires_a_fresh_authority_derivation() {
        let (flow, _, _, _) = test_flow(
            Uuid::new_v4(),
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Display,
        );
        let polls = Arc::new(AtomicUsize::new(0));
        let counted_polls = polls.clone();
        let mut future = Box::pin(std::future::poll_fn(move |_| {
            counted_polls.fetch_add(1, Ordering::SeqCst);
            Poll::<()>::Pending
        }));
        let authority_checks = Arc::new(AtomicUsize::new(0));
        let signal = Arc::new(PollSignal::default());

        for _ in 0..2 {
            let counted_authority = authority_checks.clone();
            assert!(matches!(
                authorize_and_poll_once(
                    &flow,
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    future.as_mut(),
                    &signal,
                    async move {
                        counted_authority.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                )
                .await,
                Ok(Poll::Pending)
            ));
        }

        assert_eq!(authority_checks.load(Ordering::SeqCst), 2);
        assert_eq!(polls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn expired_flow_does_not_poll_the_sdk_future() {
        let (flow, _, _, _) = test_flow(
            Uuid::new_v4(),
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Display,
        );
        let polls = Arc::new(AtomicUsize::new(0));
        let counted = polls.clone();
        let mut future = Box::pin(std::future::poll_fn(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Poll::<()>::Pending
        }));
        let signal = Arc::new(PollSignal::default());

        let result = authorize_and_poll_once(
            &flow,
            tokio::time::Instant::now(),
            future.as_mut(),
            &signal,
            async { Ok(()) },
        )
        .await;

        assert!(matches!(result, Err(DriverFailure::Timeout)));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn grant_diagnostics_are_actionable_and_secret_safe() {
        const SENTINEL: &str = "SECRET-access-token-and-recovery-key";
        const EXPECTED_USER_ID: &str = "@alice:example.org";
        let captured = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_writer(captured.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let failure = classify_grant_error(QRCodeGrantLoginError::Unknown(SENTINEL.to_owned()));
        assert!(matches!(failure, DriverFailure::Upstream));
        log_store_failure(
            Uuid::new_v4(),
            Uuid::new_v4(),
            "test",
            &axon_store::StoreError::InvalidAccountSession(SENTINEL.to_owned()),
        );
        let mut state = MatrixOAuthGrantState::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            MatrixOAuthGrantPresentation::Display,
        );
        state.qr_code_data = Some(SENTINEL.to_owned());
        state.check_code = Some(SENTINEL.to_owned());
        state.verification_uri = Some(format!("https://auth.example.org/{SENTINEL}"));
        log_terminal_failure(&state, EXPECTED_USER_ID, &DriverFailure::DeviceNotFound);

        let logs = captured.text();
        assert!(!logs.contains(SENTINEL));
        assert!(logs.contains("error_class=\"upstream\""));
        assert!(logs.contains("error_class=\"store\""));
        assert!(logs.contains("expected_user_id=\"@alice:example.org\""));
        assert!(logs.contains("error_code=\"device_not_found\""));
        assert!(logs.contains("possible_cause=\"wrong_account_or_device_provisioning\""));
    }
}
