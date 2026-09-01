use ruma::OwnedUserId;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::api::{
    AccountDto, AccountState, BackupAction, EnableBackupResponse, FlowDto, VerificationFrameDto,
};

use super::{
    AccountSelection, App, Mode, RecoveryOrigin, RoomKey, Status, VerificationDirection,
    VerificationFlow, VerificationStage,
};

pub(super) enum LogoutResolution {
    Match(AccountDto),
    Ambiguous(Vec<String>),
    Missing,
}

pub(super) enum DeleteResolution {
    Match(AccountDto),
    Ambiguous(Vec<String>),
    Missing,
}

pub(super) enum RecoverResolution {
    Match(AccountDto),
    Ambiguous(Vec<String>),
    Missing,
}

/// Result of a login/logout request run off the event loop. The slow network
/// call happens in a spawned task; this is what it sends back for the main loop
/// to apply (refresh + status) without ever blocking redraws.
pub(crate) enum LifecycleOutcome {
    Login {
        /// The full Matrix ID that was attempted, for failure messaging.
        username: String,
        /// `account_id`s that were already `active` before the attempt, so the
        /// handler can tell a real (re)login from a no-op on an active account.
        prior_account_ids: Vec<Uuid>,
        result: Result<AccountDto, String>,
    },
    /// Account list fetched off-loop as the first phase of logout; the main
    /// loop resolves the target and dispatches to confirm/perform from here.
    LogoutReady {
        target: Option<String>,
        result: Result<Vec<AccountDto>, String>,
    },
    Logout {
        /// The Matrix ID being logged out, for failure messaging.
        user_id: String,
        result: Result<AccountDto, String>,
    },
    RecoverReady {
        target: Option<String>,
        result: Result<Vec<AccountDto>, String>,
    },
    Recover {
        user_id: String,
        result: Result<AccountDto, String>,
    },
    BackupEnableReady {
        target: Option<String>,
        result: Result<Vec<AccountDto>, String>,
    },
    BackupEnable {
        user_id: String,
        result: Result<EnableBackupResponse, String>,
    },
    /// Account list fetched off-loop as the first phase of delete; the main
    /// loop resolves the target and dispatches to confirm/perform from here.
    DeleteReady {
        target: Option<String>,
        result: Result<Vec<AccountDto>, String>,
    },
    Delete {
        /// The Matrix ID being deleted, for failure messaging.
        user_id: String,
        result: Result<(), String>,
    },
    /// Result of an optimistic send — used to swap the temp ID for the real
    /// one on success or mark the echo as failed without a blocking await.
    MessageSent {
        key: super::RoomKey,
        temp_id: String,
        result: Result<String, String>,
    },
    /// Result of a `/send` media upload + send-media call. Unlike
    /// `MessageSent`, there is no optimistic local echo to reconcile — the
    /// real event arrives over `/v1/ws` like any other mutation.
    MediaSent {
        key: super::RoomKey,
        result: Result<String, String>,
    },
    /// Result of an outgoing `POST …/verify`: the new flow's id, or an error to
    /// surface in the modal (ADR 0028).
    VerifyStarted {
        account_id: Uuid,
        result: Result<String, String>,
    },
    /// Result of `POST …/verify/{flow_id}/confirm`. Errors (e.g. a 409 from a
    /// concurrent logout) surface verbatim in the modal.
    VerifyConfirmed {
        account_id: Uuid,
        flow_id: String,
        result: Result<(), String>,
    },
    /// Result of `POST …/verify/{flow_id}/cancel`. Best-effort: errors are shown
    /// only in the status line, since the modal is already closing.
    VerifyCancelled { result: Result<(), String> },
    /// Result of a read-on-reconnect `GET …/verify/{flow_id}` (ADR 0028 §3). A
    /// 404 is an implicit server-side cancellation.
    VerifyResynced {
        account_id: Uuid,
        flow_id: String,
        result: Result<FlowDto, VerifyResyncError>,
    },
    /// Result of a `GET …/verify` issued on reconnect to discover a request that
    /// arrived while disconnected (ADR 0028 §3). Errors are non-fatal.
    VerifyDiscovered {
        account_id: Uuid,
        result: Result<Vec<FlowDto>, String>,
    },
    /// Optional homeserver display name for `/whoami`, fetched off the event
    /// loop. Applied only while the status line still matches `provisional`, so
    /// a later command is not overwritten by a slow devices GET.
    WhoamiDevice {
        provisional: String,
        enriched: String,
    },
}

/// Failure modes of a verification resync. A 404 is treated as an implicit
/// cancellation (ADR 0028 §3); other errors are transient and leave the modal up.
pub(crate) enum VerifyResyncError {
    NotFound,
    Other(String),
}

impl App {
    /// Install a fetched account list. Split out from the fetch so startup can
    /// read the accounts off the event loop and apply them here (#189).
    pub(crate) fn apply_account_refresh(&mut self, accounts: Vec<AccountDto>) {
        self.set_accounts(accounts);
        // Apply the CLI --account-id flag once, before user interaction
        if self.accounts.selected == AccountSelection::All {
            if let Some(filter_id) = self.account_filter {
                if let Some(idx) = self
                    .accounts
                    .accounts
                    .iter()
                    .position(|a| a.account_id == filter_id)
                {
                    self.accounts.selected = AccountSelection::Account(idx);
                }
            }
        }
    }

    pub(super) async fn start_login(
        &mut self,
        username: Option<String>,
        password: Option<String>,
        homeserver: Option<String>,
    ) {
        if self.reject_if_lifecycle_busy() {
            return;
        }
        let Some(raw_username) = username else {
            self.clear_lifecycle_input();
            self.mode = Mode::LoginUsername;
            self.status = LOGIN_USERNAME_PROMPT.into();
            return;
        };
        let username = match normalize_matrix_user_id(&raw_username) {
            Ok(username) => username,
            Err(message) => {
                self.input.buffer = raw_username;
                self.move_cursor_to_end();
                self.mode = Mode::LoginUsername;
                self.status = message.into();
                return;
            }
        };
        // A homeserver only ever rides along the inline third token, which the
        // parser allows only when a password is also present, so it is always
        // `None` on the prompt-for-password path below.
        let homeserver = homeserver.map(|value| normalize_homeserver_url(&value));
        let Some(password) = password else {
            self.clear_lifecycle_input();
            self.mode = Mode::LoginPassword {
                username,
                homeserver,
            };
            self.status = "Password: input is hidden; Enter submits, Esc cancels".into();
            return;
        };
        self.perform_login(username, password, homeserver);
    }

    pub(crate) async fn submit_login_username(&mut self) {
        let raw = self.take_input_for_submit();
        // The username step accepts an optional homeserver after the Matrix ID
        // (both single tokens, so there is no ambiguity). This is how a user with
        // a space-bearing password — which must go through the hidden prompt —
        // still pins a homeserver.
        let mut tokens = raw.split_whitespace();
        let raw_username = tokens.next().unwrap_or_default().to_owned();
        let homeserver = tokens.next().map(str::to_owned);
        let extra = tokens.next().is_some();
        let restore = |app: &mut Self, message: &str| {
            app.input.buffer = raw.clone();
            app.move_cursor_to_end();
            app.status = message.into();
        };
        if extra {
            restore(
                self,
                "enter at most a Matrix ID and a homeserver, e.g. @user:example.com hs.example.com",
            );
            return;
        }
        let username = match normalize_matrix_user_id(&raw_username) {
            Ok(username) => username,
            Err(message) => {
                restore(self, &message);
                return;
            }
        };
        let homeserver = homeserver.map(|value| normalize_homeserver_url(&value));
        self.mode = Mode::LoginPassword {
            username,
            homeserver,
        };
        self.status = "Password: input is hidden; Enter submits, Esc cancels".into();
    }

    pub(crate) async fn submit_login_password(
        &mut self,
        username: String,
        homeserver: Option<String>,
    ) {
        let password = self.take_input_for_submit();
        if password.is_empty() {
            self.status = "password cannot be empty".into();
            return;
        }
        self.perform_login(username, password, homeserver);
    }

    pub(crate) fn submit_recovery_key(&mut self, account: AccountDto, origin: RecoveryOrigin) {
        let recovery_key = self.take_input_for_submit();
        if origin == RecoveryOrigin::BackupEnable {
            let recovery_key = recovery_key.trim();
            let recovery_key = (!recovery_key.is_empty()).then(|| recovery_key.to_owned());
            self.perform_enable_backup(account, recovery_key);
            return;
        }
        if recovery_key.trim().is_empty() {
            self.mode = Mode::Compose;
            self.status = Status::from(match origin {
                RecoveryOrigin::PostLogin => {
                    format!("recovery skipped for {}", account.user_id)
                }
                RecoveryOrigin::Command => {
                    format!("recovery cancelled for {}", account.user_id)
                }
                RecoveryOrigin::BackupEnable => {
                    format!("backup enable cancelled for {}", account.user_id)
                }
            });
            return;
        }
        self.perform_recover(account, recovery_key);
    }

    /// Kick off a login without blocking the event loop: the login round-trip
    /// runs in a spawned task (which owns and then drops the password), and its
    /// outcome arrives via [`LifecycleOutcome`]. `homeserver` is an optional base
    /// URL override; when `None`, Axon resolves the homeserver from the Matrix ID.
    fn perform_login(&mut self, username: String, password: String, homeserver: Option<String>) {
        self.clear_lifecycle_input();
        self.mode = Mode::Compose;
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        let prior_account_ids = self.active_account_ids();
        self.status = Status::from(format!("logging in {username}…"));
        self.lifecycle_busy = true;
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = client
                .login(&username, &password, homeserver.as_deref())
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::Login {
                username,
                prior_account_ids,
                result,
            });
        });
    }

    pub(super) fn start_logout(&mut self, target: Option<String>) {
        if self.reject_if_lifecycle_busy() {
            return;
        }
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        self.status = Status::from(match &target {
            Some(t) if !t.is_empty() => format!("logging out {t}…"),
            _ => "logging out…".to_owned(),
        });
        self.lifecycle_busy = true;
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = client.list_accounts().await.map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::LogoutReady { target, result });
        });
    }

    pub(super) fn start_recover(&mut self, target: Option<String>) {
        if self.reject_if_lifecycle_busy() {
            return;
        }
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        self.status = Status::from(match &target {
            Some(t) if !t.is_empty() => format!("preparing recovery for {t}…"),
            _ => "preparing recovery…".to_owned(),
        });
        self.lifecycle_busy = true;
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = client.list_accounts().await.map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::RecoverReady { target, result });
        });
    }

    pub(super) fn start_backup_enable(&mut self, target: Option<String>) {
        if self.reject_if_lifecycle_busy() {
            return;
        }
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        self.status = Status::from(match &target {
            Some(t) if !t.is_empty() => format!("preparing backup enable for {t}…"),
            _ => "preparing backup enable…".to_owned(),
        });
        self.lifecycle_busy = true;
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = client.list_accounts().await.map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::BackupEnableReady { target, result });
        });
    }

    fn request_recovery(&mut self, account: AccountDto, origin: RecoveryOrigin) {
        self.clear_lifecycle_input();
        self.status = Status::from(match origin {
            RecoveryOrigin::PostLogin | RecoveryOrigin::Command => format!(
                "Recovery key for {}: input is hidden; Enter submits, empty Enter or Esc skips",
                account.user_id
            ),
            RecoveryOrigin::BackupEnable => format!(
                "Recovery key for {}: input is hidden; Enter submits, empty Enter kicks upload \
                 only, Esc cancels",
                account.user_id
            ),
        });
        self.mode = Mode::RecoveryKey { account, origin };
    }

    fn perform_recover(&mut self, account: AccountDto, recovery_key: String) {
        let user_id = account.user_id.clone();
        let recovery_key = Zeroizing::new(recovery_key);
        let Some(tx) = self.lifecycle_tx.clone() else {
            self.clear_lifecycle_input();
            self.mode = Mode::Compose;
            return;
        };
        self.clear_lifecycle_input();
        self.mode = Mode::Compose;
        self.status = Status::from(format!("recovering encryption keys for {user_id}…"));
        self.lifecycle_busy = true;
        let client = self.client.clone();
        let account_id = account.account_id;
        tokio::spawn(async move {
            let result = client
                .recover(account_id, &recovery_key)
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::Recover { user_id, result });
        });
    }

    fn perform_enable_backup(&mut self, account: AccountDto, recovery_key: Option<String>) {
        let user_id = account.user_id.clone();
        let recovery_key = recovery_key.map(Zeroizing::new);
        self.clear_lifecycle_input();
        self.mode = Mode::Compose;
        self.status = Status::from(format!("enabling megolm backup for {user_id}…"));
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        self.lifecycle_busy = true;
        let client = self.client.clone();
        let account_id = account.account_id;
        tokio::spawn(async move {
            let result = client
                .enable_backup(account_id, recovery_key.as_ref().map(|key| key.as_str()))
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::BackupEnable { user_id, result });
        });
    }

    /// Resolve the account a verification flow should run against, mirroring the
    /// send-side targeting: the active filter when set, otherwise the sole active
    /// account — refusing when the filter is "all" with several active accounts
    /// (ADR 0028 §1).
    fn resolve_verification_account(&self) -> Result<Uuid, String> {
        if let Some(id) = self.active_account_filter() {
            return Ok(id);
        }
        let active = self.active_account_ids();
        match active.len() {
            0 => Err("no active account to verify".to_owned()),
            1 => Ok(active[0]),
            _ => Err(
                "select an account first: the filter is \"all\" and several accounts are active"
                    .to_owned(),
            ),
        }
    }

    /// Start an outgoing SAS flow against a pasted device id (self-verification,
    /// ADR 0028 §1) or a `@user:server` (cross-user verification, ADR 0040).
    /// Verification is deliberately *not* gated by `lifecycle_busy` (ADR 0028 §4).
    pub(crate) fn start_verification(&mut self, target: Option<String>) {
        let Some(target) = target.filter(|t| !t.trim().is_empty()) else {
            self.status = Status::from(
                "usage: /verify <device_id|@user:server> (incoming requests open automatically)"
                    .to_owned(),
            );
            return;
        };
        let target = target.trim().to_owned();
        // A `@user:server` argument is a cross-user verification target; anything
        // else is one of our own device ids (self-verification).
        let is_user = target.starts_with('@');
        let account_id = match self.resolve_verification_account() {
            Ok(id) => id,
            Err(message) => {
                self.status = Status::from(message);
                return;
            }
        };
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        let (user_id, device_id) = if is_user {
            (target.clone(), String::new())
        } else {
            (String::new(), target.clone())
        };
        self.verification = Some(VerificationFlow {
            account_id,
            user_id,
            device_id,
            flow_id: None,
            direction: VerificationDirection::Outgoing,
            stage: VerificationStage::Starting,
            emoji: None,
            decimals: None,
        });
        self.mode = Mode::Verification;
        self.status = Status::from(format!("starting verification of {target}…"));
        let client = self.client.clone();
        tokio::spawn(async move {
            let (user_arg, device_arg) = if is_user {
                (Some(target.as_str()), None)
            } else {
                (None, Some(target.as_str()))
            };
            let result = client
                .start_verification(account_id, user_arg, device_arg)
                .await
                .map(|resp| resp.flow_id)
                .map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::VerifyStarted { account_id, result });
        });
    }

    /// Auto-open the modal for a peer-initiated request (ADR 0028 §2). Ignores a
    /// new request while a non-terminal flow is already on screen.
    pub(crate) fn open_incoming_verification(
        &mut self,
        account_id: Uuid,
        flow_id: String,
        user_id: String,
        device_id: String,
    ) {
        if self
            .verification
            .as_ref()
            .is_some_and(|flow| !flow.stage.is_terminal())
        {
            return;
        }
        self.verification = Some(VerificationFlow {
            account_id,
            user_id,
            device_id,
            flow_id: Some(flow_id),
            direction: VerificationDirection::Incoming,
            stage: VerificationStage::Waiting,
            emoji: None,
            decimals: None,
        });
        self.mode = Mode::Verification;
        self.status = Status::from("verification requested — compare the emoji".to_owned());
    }

    /// Whether to surface an incoming verification request, applying the
    /// client-side suppression of unsolicited cross-user requests (ADR 0040). A
    /// self-verification request (the peer is our own account user) is always
    /// shown. A cross-user request is shown only when the
    /// `accept_incoming_verification` display option is set; otherwise it is
    /// declined silently — cancelled server-side, with no modal.
    pub(crate) fn should_open_incoming_verification(
        &self,
        account_id: Uuid,
        payload: &VerificationFrameDto,
    ) -> bool {
        if self.verification.as_ref().is_some_and(|flow| {
            flow.is_pending_outgoing_target(
                account_id,
                &payload.user_id,
                payload.device_id.as_deref(),
            )
        }) {
            return false;
        }
        self.should_open_incoming_verification_target(
            account_id,
            &payload.flow_id,
            &payload.user_id,
        )
    }

    pub(crate) fn should_open_incoming_verification_target(
        &self,
        account_id: Uuid,
        flow_id: &str,
        user_id: &str,
    ) -> bool {
        let own_user = self
            .accounts
            .accounts
            .iter()
            .find(|account| account.account_id == account_id)
            .map(|account| account.user_id.as_str());
        let is_cross_user = !user_id.is_empty() && own_user.is_some_and(|own| own != user_id);
        if is_cross_user && !self.display.accept_incoming_verification {
            let client = self.client.clone();
            let flow_id = flow_id.to_owned();
            tokio::spawn(async move {
                let _ = client.cancel_verification(account_id, &flow_id).await;
            });
            return false;
        }
        true
    }

    /// Confirm the SAS values match (the user pressed `y`).
    pub(crate) fn confirm_active_verification(&mut self) {
        let Some(flow) = self.verification.as_mut() else {
            return;
        };
        let (Some(flow_id), account_id) = (flow.flow_id.clone(), flow.account_id) else {
            return;
        };
        flow.stage = VerificationStage::Confirming;
        self.status = Status::from("confirming verification…".to_owned());
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = client
                .confirm_verification(account_id, &flow_id)
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::VerifyConfirmed {
                account_id,
                flow_id,
                result,
            });
        });
    }

    /// Cancel the active flow (the user pressed `n`, or `Esc` on a live flow).
    /// The modal transitions to a terminal state immediately; the cancel call is
    /// best-effort.
    pub(crate) fn cancel_active_verification(&mut self) {
        let Some(flow) = self.verification.as_mut() else {
            return;
        };
        let target = flow
            .flow_id
            .clone()
            .map(|flow_id| (flow.account_id, flow_id));
        flow.stage = VerificationStage::Ended("Verification cancelled".to_owned());
        self.status = Status::from("verification cancelled".to_owned());
        let (Some((account_id, flow_id)), Some(tx)) = (target, self.lifecycle_tx.clone()) else {
            return;
        };
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = client
                .cancel_verification(account_id, &flow_id)
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::VerifyCancelled { result });
        });
    }

    /// On reconnect, discover a verification request that may have arrived while
    /// disconnected (ADR 0028 §3). Only runs when no flow is already on screen
    /// and a single account is unambiguously targetable.
    pub(crate) fn discover_incoming_verification(&mut self) {
        if self
            .verification
            .as_ref()
            .is_some_and(|flow| !flow.stage.is_terminal())
        {
            return;
        }
        let Ok(account_id) = self.resolve_verification_account() else {
            return;
        };
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = client
                .list_flows(account_id)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(LifecycleOutcome::VerifyDiscovered { account_id, result });
        });
    }

    /// Re-read the active flow's state on WS reconnect (ADR 0028 §3).
    pub(crate) fn resync_active_verification(&mut self) {
        let Some(flow) = self.verification.as_ref() else {
            return;
        };
        if flow.stage.is_terminal() {
            return;
        }
        let (Some(flow_id), account_id) = (flow.flow_id.clone(), flow.account_id) else {
            return;
        };
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = match client.get_flow(account_id, &flow_id).await {
                Ok(flow) => Ok(flow),
                Err(err) if err.is_not_found() => Err(VerifyResyncError::NotFound),
                Err(err) => Err(VerifyResyncError::Other(err.to_string())),
            };
            let _ = tx.send(LifecycleOutcome::VerifyResynced {
                account_id,
                flow_id,
                result,
            });
        });
    }

    /// Either prompt for confirmation or log out immediately, per the
    /// `confirm_logout` display option.
    pub(crate) fn request_logout(&mut self, account: AccountDto) {
        if self.display.confirm_logout {
            self.clear_lifecycle_input();
            self.status = Status::from(format!("Log out {}? [y/N]", account.user_id));
            self.mode = Mode::ConfirmLogout { account };
        } else {
            self.perform_logout(account);
        }
    }

    pub(crate) fn cancel_logout_confirmation(&mut self) {
        self.mode = Mode::Compose;
        self.status = Status::Info(String::new());
    }

    /// Kick off a logout without blocking the event loop; the result arrives via
    /// [`LifecycleOutcome`].
    pub(crate) fn perform_logout(&mut self, account: AccountDto) {
        let user_id = account.user_id.clone();
        self.clear_lifecycle_input();
        self.mode = Mode::Compose;
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        self.status = Status::from(format!("logging out {user_id}…"));
        self.lifecycle_busy = true;
        let client = self.client.clone();
        let account_id = account.account_id;
        tokio::spawn(async move {
            let result = client
                .logout(account_id)
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::Logout { user_id, result });
        });
    }

    pub(super) fn start_delete(&mut self, target: Option<String>) {
        if self.reject_if_lifecycle_busy() {
            return;
        }
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        self.status = Status::from(match &target {
            Some(t) if !t.is_empty() => format!("preparing to delete {t}…"),
            _ => "preparing to delete…".to_owned(),
        });
        self.lifecycle_busy = true;
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = client.list_accounts().await.map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::DeleteReady { target, result });
        });
    }

    pub(crate) fn request_delete(&mut self, account: AccountDto) {
        self.clear_lifecycle_input();
        self.status = Status::from(format!(
            "Permanently delete {}? Type YES to confirm, or Esc to cancel",
            account.user_id
        ));
        self.mode = Mode::ConfirmDelete { account };
    }

    pub(crate) fn cancel_delete_confirmation(&mut self) {
        self.clear_lifecycle_input();
        self.mode = Mode::Compose;
        self.status = Status::Info("deletion cancelled".to_owned());
    }

    pub(crate) fn perform_delete(&mut self, account: AccountDto) {
        let user_id = account.user_id.clone();
        self.clear_lifecycle_input();
        self.mode = Mode::Compose;
        let Some(tx) = self.lifecycle_tx.clone() else {
            return;
        };
        self.status = Status::from(format!("deleting {user_id}…"));
        self.lifecycle_busy = true;
        let client = self.client.clone();
        let account_id = account.account_id;
        tokio::spawn(async move {
            let result = client
                .delete_account(account_id)
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(LifecycleOutcome::Delete { user_id, result });
        });
    }

    /// Apply the result of a spawned login/logout once it lands on the event
    /// loop: refresh views and report a final status. Runs only fast, local
    /// Axon calls, so blocking here is acceptable.
    pub(crate) async fn handle_lifecycle_outcome(&mut self, outcome: LifecycleOutcome) {
        if let LifecycleOutcome::MessageSent {
            key,
            temp_id,
            result,
        } = outcome
        {
            match result {
                Ok(event_id) => {
                    if let Some(events) = self.messages.events.get_mut(&key) {
                        if let Some(e) = events.iter_mut().find(|e| e.event_id == temp_id) {
                            e.event_id = event_id.clone();
                        }
                    }
                    self.live.pending_own_event_id = Some(event_id.clone());
                    self.status = Status::EventAction {
                        debug: format!("sent: {event_id}"),
                        redacted: "sent",
                    };
                }
                Err(err) => {
                    if let Some(events) = self.messages.events.get_mut(&key) {
                        if let Some(e) = events.iter_mut().find(|e| e.event_id == temp_id) {
                            e.body = Some(format!("[send failed: {err}]"));
                        }
                    }
                    self.status = Status::Info(format!("send failed: {err}"));
                }
            }
            return;
        }
        if let LifecycleOutcome::WhoamiDevice {
            provisional,
            enriched,
        } = outcome
        {
            if self.status.text(false) == provisional {
                self.status = Status::Info(enriched.clone());
                if self.pending_command_response.as_deref() == Some(provisional.as_str()) {
                    self.pending_command_response = Some(enriched);
                }
            }
            return;
        }
        if let LifecycleOutcome::MediaSent { key, result } = outcome {
            self.media_send_busy = false;
            // The user may have switched rooms while the upload was in
            // flight; only claim the status line for the room the send
            // actually targeted (the same staleness discipline other
            // background results apply before mutating visible state).
            let same_room = self
                .selected_room()
                .is_some_and(|room| RoomKey::from(room) == key);
            match result {
                Ok(event_id) => {
                    self.live.pending_own_event_id = Some(event_id.clone());
                    self.status = if same_room {
                        Status::EventAction {
                            debug: format!("sent: {event_id}"),
                            redacted: "sent",
                        }
                    } else {
                        Status::Info(format!("sent to {}: {event_id}", key.room_id))
                    };
                }
                Err(err) => {
                    self.status = if same_room {
                        Status::Info(format!("send failed: {err}"))
                    } else {
                        Status::Info(format!("send failed in {}: {err}", key.room_id))
                    };
                }
            }
            return;
        }
        self.lifecycle_busy = false;
        if !matches!(self.mode, Mode::Popup(_)) {
            self.pending_command_response = None;
        }
        match outcome {
            LifecycleOutcome::Login {
                username,
                prior_account_ids,
                result,
            } => match result {
                Ok(account) => {
                    let warning = self.refresh_after_lifecycle_change().await;
                    if let Some(idx) = self
                        .accounts
                        .accounts
                        .iter()
                        .position(|a| a.account_id == account.account_id)
                    {
                        self.accounts.selected = AccountSelection::Account(idx);
                        self.sync_room_selection_to_account_filter();
                        self.load_selected_timeline().await;
                    }
                    let already_active = prior_account_ids.contains(&account.account_id);
                    if should_offer_post_login_recovery(already_active, account.verified) {
                        self.request_recovery(account, RecoveryOrigin::PostLogin);
                    } else {
                        self.status =
                            lifecycle_login_status(already_active, &account.user_id, warning);
                    }
                }
                Err(error) => {
                    self.status = Status::from(format!("login failed for {username}: {error}"));
                }
            },
            LifecycleOutcome::LogoutReady { target, result } => {
                self.lifecycle_busy = false;
                match result {
                    Ok(accounts) => {
                        // Filter to Active accounts (set_accounts) instead of a raw
                        // assignment: the latter briefly flashed every account —
                        // including logged-out ones — into the account pane during
                        // logout. resolve_logout_target re-filters to Active anyway.
                        self.set_accounts(accounts);
                        match self.resolve_logout_target(target.as_deref()) {
                            LogoutResolution::Match(account) => self.request_logout(account),
                            LogoutResolution::Ambiguous(options) => {
                                self.restore_logout_input(target.as_deref());
                                self.status = Status::from(format!(
                                    "logout target is ambiguous: {} - press Tab to choose",
                                    options.join(", ")
                                ));
                            }
                            LogoutResolution::Missing => {
                                self.restore_logout_input(target.as_deref());
                                self.status = if target.as_deref().is_some_and(|v| !v.is_empty()) {
                                    Status::from(format!(
                                        "no active account matches: {}",
                                        target.unwrap_or_default()
                                    ))
                                } else {
                                    Status::from("no active accounts".to_owned())
                                };
                            }
                        }
                    }
                    Err(err) => {
                        self.restore_logout_input(target.as_deref());
                        self.status = Status::from(format!("logout failed: {err}"));
                    }
                }
            }
            LifecycleOutcome::Logout { user_id, result } => match result {
                Ok(account) => {
                    let warning = self.refresh_after_lifecycle_change().await;
                    self.status = lifecycle_success_status("logged out", &account.user_id, warning);
                }
                Err(error) => {
                    self.status = Status::from(format!("logout failed for {user_id}: {error}"));
                }
            },
            LifecycleOutcome::RecoverReady { target, result } => {
                self.lifecycle_busy = false;
                match result {
                    Ok(accounts) => {
                        self.set_accounts(accounts);
                        match self.resolve_recover_target(target.as_deref()) {
                            RecoverResolution::Match(account) => {
                                self.request_recovery(account, RecoveryOrigin::Command);
                            }
                            RecoverResolution::Ambiguous(options) => {
                                self.restore_recover_input(target.as_deref());
                                self.status = Status::from(format!(
                                    "recovery target is ambiguous: {} - press Tab to choose",
                                    options.join(", ")
                                ));
                            }
                            RecoverResolution::Missing => {
                                self.restore_recover_input(target.as_deref());
                                self.status = if target.as_deref().is_some_and(|v| !v.is_empty()) {
                                    Status::from(format!(
                                        "no active account matches: {}",
                                        target.unwrap_or_default()
                                    ))
                                } else {
                                    Status::from("no active accounts".to_owned())
                                };
                            }
                        }
                    }
                    Err(err) => {
                        self.restore_recover_input(target.as_deref());
                        self.status = Status::from(format!("recovery failed: {err}"));
                    }
                }
            }
            LifecycleOutcome::Recover { user_id, result } => match result {
                Ok(account) => {
                    let warning = self.refresh_after_lifecycle_change().await;
                    self.status =
                        recovery_success_status(&account.user_id, account.verified, warning);
                }
                Err(error) => {
                    self.status = Status::from(format!("recovery failed for {user_id}: {error}"));
                }
            },
            LifecycleOutcome::BackupEnableReady { target, result } => {
                self.lifecycle_busy = false;
                match result {
                    Ok(accounts) => {
                        self.set_accounts(accounts);
                        match self.resolve_recover_target(target.as_deref()) {
                            RecoverResolution::Match(account) => {
                                if account.verified != Some(true) {
                                    self.restore_backup_enable_input(target.as_deref());
                                    self.status = Status::from(format!(
                                        "account is not verified; recover or verify first: {}",
                                        account.user_id
                                    ));
                                } else {
                                    self.request_recovery(account, RecoveryOrigin::BackupEnable);
                                }
                            }
                            RecoverResolution::Ambiguous(options) => {
                                self.restore_backup_enable_input(target.as_deref());
                                self.status = Status::from(format!(
                                    "backup enable target is ambiguous: {} - press Tab to choose",
                                    options.join(", ")
                                ));
                            }
                            RecoverResolution::Missing => {
                                self.restore_backup_enable_input(target.as_deref());
                                self.status = if target.as_deref().is_some_and(|v| !v.is_empty()) {
                                    Status::from(format!(
                                        "no active account matches: {}",
                                        target.unwrap_or_default()
                                    ))
                                } else {
                                    Status::from("no active accounts".to_owned())
                                };
                            }
                        }
                    }
                    Err(err) => {
                        self.restore_backup_enable_input(target.as_deref());
                        self.status = Status::from(format!("backup enable failed: {err}"));
                    }
                }
            }
            LifecycleOutcome::BackupEnable { user_id, result } => match result {
                Ok(response) => {
                    let warning = self.refresh_after_lifecycle_change().await;
                    self.status =
                        backup_enable_success_status(&user_id, response.backup_action, warning);
                }
                Err(error) => {
                    self.status =
                        Status::from(format!("backup enable failed for {user_id}: {error}"));
                }
            },
            LifecycleOutcome::DeleteReady { target, result } => {
                self.lifecycle_busy = false;
                match result {
                    Ok(accounts) => {
                        self.accounts.client_visible = accounts.clone();
                        self.accounts.inactive_ids = accounts
                            .iter()
                            .filter(|a| a.state != AccountState::Active)
                            .map(|a| a.account_id)
                            .collect();
                        self.accounts.accounts = accounts;
                        match self.resolve_delete_target(target.as_deref()) {
                            DeleteResolution::Match(account) => self.request_delete(account),
                            DeleteResolution::Ambiguous(options) => {
                                self.restore_delete_input(target.as_deref());
                                self.status = Status::from(format!(
                                    "delete target is ambiguous: {} - press Tab to choose",
                                    options.join(", ")
                                ));
                            }
                            DeleteResolution::Missing => {
                                self.restore_delete_input(target.as_deref());
                                self.status = if target.as_deref().is_some_and(|v| !v.is_empty()) {
                                    Status::from(format!(
                                        "no account matches: {}",
                                        target.unwrap_or_default()
                                    ))
                                } else {
                                    Status::from("no accounts".to_owned())
                                };
                            }
                        }
                    }
                    Err(err) => {
                        self.restore_delete_input(target.as_deref());
                        self.status = Status::from(format!("delete failed: {err}"));
                    }
                }
            }
            LifecycleOutcome::Delete { user_id, result } => match result {
                Ok(()) => {
                    let warning = self.refresh_after_lifecycle_change().await;
                    self.status = lifecycle_success_status("deleted", &user_id, warning);
                }
                Err(error) => {
                    self.status = Status::from(format!("delete failed for {user_id}: {error}"));
                }
            },
            LifecycleOutcome::VerifyStarted { account_id, result } => match result {
                Ok(flow_id) => {
                    if let Some(flow) = self.verification.as_mut() {
                        if flow.account_id == account_id && flow.flow_id.is_none() {
                            flow.flow_id = Some(flow_id);
                            if flow.stage == VerificationStage::Starting {
                                flow.stage = VerificationStage::Waiting;
                            }
                            self.status = Status::from(
                                "verification started — waiting for emoji…".to_owned(),
                            );
                        }
                    }
                }
                Err(error) => {
                    self.end_active_verification(
                        account_id,
                        None,
                        format!("verification failed: {error}"),
                    );
                }
            },
            LifecycleOutcome::VerifyConfirmed {
                account_id,
                flow_id,
                result,
            } => match result {
                Ok(()) => {
                    self.status =
                        Status::from("verification confirmed — awaiting peer…".to_owned());
                }
                Err(error) => {
                    self.end_active_verification(
                        account_id,
                        Some(&flow_id),
                        format!("verification failed: {error}"),
                    );
                }
            },
            LifecycleOutcome::VerifyCancelled { result } => {
                if let Err(error) = result {
                    self.status = Status::from(format!("verification cancel failed: {error}"));
                }
            }
            LifecycleOutcome::VerifyResynced {
                account_id,
                flow_id,
                result,
            } => match result {
                Ok(flow_dto) => {
                    if let Some(flow) = self.verification.as_mut() {
                        if flow.matches(account_id, &flow_id) && !flow.stage.is_terminal() {
                            flow.apply_flow(&flow_dto);
                            let stage_label = format!("{:?}", flow_dto.stage).to_ascii_lowercase();
                            self.status = Status::from(format!(
                                "verification resync: server stage is {stage_label}"
                            ));
                        }
                    }
                }
                Err(VerifyResyncError::NotFound) => {
                    self.end_active_verification(
                        account_id,
                        Some(&flow_id),
                        "Verification ended — the flow was cancelled by the server".to_owned(),
                    );
                }
                Err(VerifyResyncError::Other(error)) => {
                    self.status = Status::from(format!("verification resync failed: {error}"));
                }
            },
            LifecycleOutcome::VerifyDiscovered { account_id, result } => {
                if self
                    .verification
                    .as_ref()
                    .is_some_and(|flow| !flow.stage.is_terminal())
                {
                    // A live frame opened a modal while the list was in flight.
                } else if let Ok(flows) = result {
                    if let Some(flow) = flows.into_iter().find(|flow| {
                        !matches!(
                            flow.stage,
                            crate::api::FlowStage::Done | crate::api::FlowStage::Cancelled
                        ) && self.should_open_incoming_verification_target(
                            account_id,
                            &flow.flow_id,
                            &flow.user_id,
                        )
                    }) {
                        self.open_incoming_verification(
                            account_id,
                            flow.flow_id.clone(),
                            flow.user_id.clone(),
                            flow.device_id.clone().unwrap_or_default(),
                        );
                        if let Some(active) = self.verification.as_mut() {
                            active.apply_flow(&flow);
                        }
                    }
                }
            }
            LifecycleOutcome::MessageSent { .. } => unreachable!(),
            LifecycleOutcome::MediaSent { .. } => unreachable!(),
            LifecycleOutcome::WhoamiDevice { .. } => unreachable!(),
        }
        self.queue_completed_command_response();
    }

    /// Transition the active verification modal to a terminal error state and set
    /// the status, but only when the open flow still matches the operation that
    /// failed (guards against a stale outcome clobbering a newer flow).
    fn end_active_verification(
        &mut self,
        account_id: Uuid,
        flow_id: Option<&str>,
        message: String,
    ) {
        let matches = self.verification.as_ref().is_some_and(|flow| {
            flow.account_id == account_id
                && flow_id.is_none_or(|id| flow.flow_id.as_deref() == Some(id))
                && !flow.stage.is_terminal()
        });
        if matches {
            if let Some(flow) = self.verification.as_mut() {
                flow.stage = VerificationStage::Ended(message.clone());
            }
        }
        self.status = Status::from(message);
    }

    fn reject_if_lifecycle_busy(&mut self) -> bool {
        if self.lifecycle_busy {
            self.status = Status::from("an account operation is already in progress".to_owned());
            return true;
        }
        false
    }

    fn active_account_ids(&self) -> Vec<Uuid> {
        self.accounts
            .accounts
            .iter()
            .filter(|account| account.state == AccountState::Active)
            .map(|account| account.account_id)
            .collect()
    }

    async fn refresh_after_lifecycle_change(&mut self) -> Option<String> {
        let had_selection = self.selected_room().map(RoomKey::from);
        let mut warnings = Vec::new();
        match self.client.list_accounts().await {
            Ok(accounts) => self.set_accounts(accounts),
            Err(err) => warnings.push(format!("account refresh failed: {err}")),
        }
        match self.client.list_rooms(self.account_filter).await {
            Ok(rooms) => {
                self.apply_room_refresh(rooms);
                let new_selection = self.selected_room().map(RoomKey::from);
                if new_selection.is_some() && new_selection != had_selection {
                    self.load_selected_timeline().await;
                }
            }
            Err(err) => {
                warnings.push(format!("room refresh failed: {err}"));
            }
        }
        (!warnings.is_empty()).then(|| warnings.join("; "))
    }

    pub(super) fn resolve_logout_target(&self, target: Option<&str>) -> LogoutResolution {
        let active: Vec<_> = self
            .accounts
            .accounts
            .iter()
            .filter(|account| account.state == AccountState::Active)
            .cloned()
            .collect();
        let target = target.unwrap_or_default().trim();
        let matches = if target.is_empty() {
            active
        } else if let Ok(account_id) = Uuid::parse_str(target) {
            active
                .into_iter()
                .filter(|account| account.account_id == account_id)
                .collect()
        } else if let Some(canonical) = canonical_logout_target(target) {
            active
                .into_iter()
                .filter(|account| account.user_id == canonical)
                .collect()
        } else {
            let localpart = target.trim_start_matches('@');
            active
                .into_iter()
                .filter(|account| matrix_user_localpart(&account.user_id) == Some(localpart))
                .collect()
        };

        match matches.as_slice() {
            [account] => LogoutResolution::Match(account.clone()),
            [_, _, ..] => LogoutResolution::Ambiguous(
                matches.into_iter().map(|account| account.user_id).collect(),
            ),
            [] => LogoutResolution::Missing,
        }
    }

    pub(super) fn resolve_recover_target(&self, target: Option<&str>) -> RecoverResolution {
        let active: Vec<_> = self
            .accounts
            .accounts
            .iter()
            .filter(|account| account.state == AccountState::Active)
            .cloned()
            .collect();
        let target = target.unwrap_or_default().trim();
        let matches = resolve_account_matches(active, target);
        match matches.as_slice() {
            [account] => RecoverResolution::Match(account.clone()),
            [_, _, ..] => RecoverResolution::Ambiguous(
                matches.into_iter().map(|account| account.user_id).collect(),
            ),
            [] => RecoverResolution::Missing,
        }
    }

    fn restore_recover_input(&mut self, target: Option<&str>) {
        self.mode = Mode::Compose;
        self.input.buffer = match target.filter(|value| !value.is_empty()) {
            Some(target) => format!("/recover {target}"),
            None => "/recover".to_owned(),
        };
        self.move_cursor_to_end();
    }

    fn restore_backup_enable_input(&mut self, target: Option<&str>) {
        self.mode = Mode::Compose;
        self.input.buffer = match target.filter(|value| !value.is_empty()) {
            Some(target) => format!("/backup enable {target}"),
            None => "/backup enable".to_owned(),
        };
        self.move_cursor_to_end();
    }

    fn restore_logout_input(&mut self, target: Option<&str>) {
        self.mode = Mode::Compose;
        self.input.buffer = match target.filter(|value| !value.is_empty()) {
            Some(target) => format!("/logout {target}"),
            None => "/logout".to_owned(),
        };
        self.move_cursor_to_end();
    }

    pub(super) fn resolve_delete_target(&self, target: Option<&str>) -> DeleteResolution {
        let deletable: Vec<_> = self
            .accounts
            .accounts
            .iter()
            .filter(|account| account.state != AccountState::Deleting)
            .cloned()
            .collect();
        let target = target.unwrap_or_default().trim();
        let matches = if target.is_empty() {
            deletable
        } else if let Ok(account_id) = Uuid::parse_str(target) {
            deletable
                .into_iter()
                .filter(|account| account.account_id == account_id)
                .collect()
        } else if let Some(canonical) = canonical_logout_target(target) {
            deletable
                .into_iter()
                .filter(|account| account.user_id == canonical)
                .collect()
        } else {
            let localpart = target.trim_start_matches('@');
            deletable
                .into_iter()
                .filter(|account| matrix_user_localpart(&account.user_id) == Some(localpart))
                .collect()
        };

        match matches.as_slice() {
            [account] => DeleteResolution::Match(account.clone()),
            [_, _, ..] => DeleteResolution::Ambiguous(
                matches.into_iter().map(|account| account.user_id).collect(),
            ),
            [] => DeleteResolution::Missing,
        }
    }

    fn restore_delete_input(&mut self, target: Option<&str>) {
        self.mode = Mode::Compose;
        self.input.buffer = match target.filter(|value| !value.is_empty()) {
            Some(target) => format!("/delete {target}"),
            None => "/delete".to_owned(),
        };
        self.move_cursor_to_end();
    }

    pub(crate) fn delete_candidates(&self, target: &str) -> Vec<String> {
        let target = target.trim();
        let matches: Vec<_> = self
            .accounts
            .accounts
            .iter()
            .filter(|account| account.state != AccountState::Deleting)
            .filter(|account| {
                if target.is_empty() {
                    true
                } else if let Some(canonical) = canonical_logout_target(target) {
                    account.user_id.starts_with(&canonical)
                } else {
                    matrix_user_localpart(&account.user_id).is_some_and(|localpart| {
                        localpart.starts_with(target.trim_start_matches('@'))
                    })
                }
            })
            .collect();
        matches
            .iter()
            .map(|account| {
                if matches
                    .iter()
                    .filter(|candidate| candidate.user_id == account.user_id)
                    .count()
                    > 1
                {
                    account.account_id.to_string()
                } else {
                    account.user_id.clone()
                }
            })
            .collect()
    }

    pub(crate) fn active_logout_candidates(&self, target: &str) -> Vec<String> {
        let target = target.trim();
        let matches: Vec<_> = self
            .accounts
            .accounts
            .iter()
            .filter(|account| account.state == AccountState::Active)
            .filter(|account| {
                if target.is_empty() {
                    true
                } else if let Some(canonical) = canonical_logout_target(target) {
                    account.user_id.starts_with(&canonical)
                } else {
                    matrix_user_localpart(&account.user_id).is_some_and(|localpart| {
                        localpart.starts_with(target.trim_start_matches('@'))
                    })
                }
            })
            .collect();
        matches
            .iter()
            .map(|account| {
                if matches
                    .iter()
                    .filter(|candidate| candidate.user_id == account.user_id)
                    .count()
                    > 1
                {
                    account.account_id.to_string()
                } else {
                    account.user_id.clone()
                }
            })
            .collect()
    }

    pub(crate) fn active_recover_candidates(&self, target: &str) -> Vec<String> {
        self.active_account_candidates(target)
    }

    fn active_account_candidates(&self, target: &str) -> Vec<String> {
        let target = target.trim();
        let matches: Vec<_> = self
            .accounts
            .accounts
            .iter()
            .filter(|account| account.state == AccountState::Active)
            .filter(|account| {
                if target.is_empty() {
                    true
                } else if let Some(canonical) = canonical_logout_target(target) {
                    account.user_id.starts_with(&canonical)
                } else {
                    matrix_user_localpart(&account.user_id).is_some_and(|localpart| {
                        localpart.starts_with(target.trim_start_matches('@'))
                    })
                }
            })
            .collect();
        account_completion_values(&matches)
    }

    pub(crate) fn cancel_lifecycle_input(&mut self) {
        self.clear_lifecycle_input();
        self.mode = Mode::Compose;
        self.status = Status::Info(String::new());
    }

    pub(crate) fn cancel_recovery_input(&mut self, account: AccountDto, origin: RecoveryOrigin) {
        self.clear_lifecycle_input();
        self.mode = Mode::Compose;
        self.status = Status::from(match origin {
            RecoveryOrigin::PostLogin => format!("recovery skipped for {}", account.user_id),
            RecoveryOrigin::Command => format!("recovery cancelled for {}", account.user_id),
            RecoveryOrigin::BackupEnable => {
                format!("backup enable cancelled for {}", account.user_id)
            }
        });
    }

    fn clear_lifecycle_input(&mut self) {
        self.clear_input_buffer();
        self.input.logout_command_completion = None;
        self.input.recover_command_completion = None;
        self.input.backup_command_completion = None;
        self.input.delete_command_completion = None;
    }
}

fn resolve_account_matches(accounts: Vec<AccountDto>, target: &str) -> Vec<AccountDto> {
    if target.is_empty() {
        accounts
    } else if let Ok(account_id) = Uuid::parse_str(target) {
        accounts
            .into_iter()
            .filter(|account| account.account_id == account_id)
            .collect()
    } else if let Some(canonical) = canonical_logout_target(target) {
        accounts
            .into_iter()
            .filter(|account| account.user_id == canonical)
            .collect()
    } else {
        let localpart = target.trim_start_matches('@');
        accounts
            .into_iter()
            .filter(|account| matrix_user_localpart(&account.user_id) == Some(localpart))
            .collect()
    }
}

fn account_completion_values(accounts: &[&AccountDto]) -> Vec<String> {
    accounts
        .iter()
        .map(|account| {
            if accounts
                .iter()
                .filter(|candidate| candidate.user_id == account.user_id)
                .count()
                > 1
            {
                account.account_id.to_string()
            } else {
                account.user_id.clone()
            }
        })
        .collect()
}

/// Username-step prompt. Mentions the optional homeserver so users who must use
/// the hidden password prompt (e.g. a password with spaces) can still pin one.
const LOGIN_USERNAME_PROMPT: &str =
    "Matrix ID (optionally a homeserver after it): @user:example.com [hs.example.com]";

/// Make a user-supplied homeserver acceptable as Axon's `homeserver_url`: a bare
/// host gets `https://`, while an explicit scheme is left intact (so a loopback
/// dev server can be reached with `http://localhost:8008`).
fn normalize_homeserver_url(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    }
}

fn normalize_matrix_user_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    let candidate = if value.starts_with('@') {
        value.to_owned()
    } else if value.contains(':') {
        format!("@{value}")
    } else if let Some((localpart, server_name)) = value.split_once('@') {
        if localpart.is_empty() || server_name.is_empty() || server_name.contains('@') {
            return Err(matrix_user_id_error());
        }
        format!("@{localpart}:{server_name}")
    } else {
        return Err(matrix_user_id_error());
    };

    OwnedUserId::try_from(candidate.as_str())
        .map(|_| candidate)
        .map_err(|_| matrix_user_id_error())
}

fn canonical_logout_target(value: &str) -> Option<String> {
    let value = value.trim();
    (value.contains(':') || (!value.starts_with('@') && value.contains('@')))
        .then(|| normalize_matrix_user_id(value).ok())
        .flatten()
}

fn matrix_user_id_error() -> String {
    "enter a Matrix ID as @name:domain, name:domain, or name@domain".to_owned()
}

fn matrix_user_localpart(user_id: &str) -> Option<&str> {
    user_id
        .strip_prefix('@')?
        .split_once(':')
        .map(|(local, _)| local)
}

fn lifecycle_success_status(action: &str, user_id: &str, warning: Option<String>) -> Status {
    Status::from(match warning {
        Some(warning) => format!("{action}: {user_id}; {warning}"),
        None => format!("{action}: {user_id}"),
    })
}

/// Status for a completed login. An `already_active` account is the server's
/// idempotent no-op: nothing changed and the password was never consulted, so
/// say so rather than implying a fresh authentication succeeded.
fn lifecycle_login_status(already_active: bool, user_id: &str, warning: Option<String>) -> Status {
    let summary = if already_active {
        format!("already logged in: {user_id} (no changes)")
    } else {
        format!("logged in: {user_id}")
    };
    Status::from(match warning {
        Some(warning) => format!("{summary}; {warning}"),
        None => summary,
    })
}

fn backup_enable_success_status(
    user_id: &str,
    action: BackupAction,
    warning: Option<String>,
) -> Status {
    let summary = match action {
        BackupAction::Joined => format!("joined existing megolm backup: {user_id}"),
        BackupAction::Enabled => format!("enabled megolm backup: {user_id}"),
        BackupAction::ExportPending => {
            format!("megolm backup export pending for {user_id}; retry /backup enable")
        }
        BackupAction::Failed => {
            format!("megolm backup enable failed for {user_id}; retry /backup enable")
        }
        BackupAction::AlreadyUploading => {
            format!("already uploading megolm keys to backup: {user_id}")
        }
    };
    Status::from(match warning {
        Some(warning) => format!("{summary}; {warning}"),
        None => summary,
    })
}

fn recovery_success_status(
    user_id: &str,
    verified: Option<bool>,
    warning: Option<String>,
) -> Status {
    let verification = match verified {
        Some(true) => "device verified",
        Some(false) => "device remains unverified",
        None => "device verification unavailable",
    };
    lifecycle_success_status(
        "recovered encryption keys",
        &format!("{user_id} ({verification})"),
        warning,
    )
}

fn should_offer_post_login_recovery(already_active: bool, verified: Option<bool>) -> bool {
    !already_active && verified != Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AxonClient;
    use crate::app::{Mode, RecoveryOrigin};
    use crate::config::TuiConfig;

    #[test]
    fn normalizes_supported_matrix_username_forms() {
        assert_eq!(
            normalize_matrix_user_id("@alice:example.com").unwrap(),
            "@alice:example.com"
        );
        assert_eq!(
            normalize_matrix_user_id("alice:example.com").unwrap(),
            "@alice:example.com"
        );
        assert_eq!(
            normalize_matrix_user_id("alice@example.com").unwrap(),
            "@alice:example.com"
        );
    }

    #[test]
    fn rejects_login_localpart_without_server() {
        assert!(normalize_matrix_user_id("alice").is_err());
        assert!(normalize_matrix_user_id("@alice").is_err());
    }

    #[tokio::test]
    async fn recovery_failure_is_queued_for_overflow_handling() {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );

        app.handle_lifecycle_outcome(LifecycleOutcome::Recover {
            user_id: "@alice:example.com".to_owned(),
            result: Err("the recovery key was rejected by Axon".to_owned()),
        })
        .await;

        assert_eq!(
            app.pending_command_response.as_deref(),
            Some("recovery failed for @alice:example.com: the recovery key was rejected by Axon")
        );
    }

    #[tokio::test]
    async fn logout_ready_keeps_only_active_accounts_in_pane() {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );

        let mk = |n: u128, user: &str, state| AccountDto {
            account_id: Uuid::from_u128(n),
            user_id: user.to_owned(),
            state,
            device_id: None,
            verified: Some(false),
            backup: Default::default(),
        };
        // Two active and one deactivated account come back from list_accounts.
        // Empty target resolves to Ambiguous (two active), so the handler stops
        // after populating the pane — exactly the window where the flash occurred.
        app.handle_lifecycle_outcome(LifecycleOutcome::LogoutReady {
            target: None,
            result: Ok(vec![
                mk(1, "@alice:example.com", AccountState::Active),
                mk(2, "@bob:example.com", AccountState::Deactivated),
                mk(3, "@carol:example.com", AccountState::Active),
            ]),
        })
        .await;

        // The deactivated account must not appear in the account pane.
        let shown: Vec<&str> = app
            .accounts
            .accounts
            .iter()
            .map(|a| a.user_id.as_str())
            .collect();
        assert_eq!(shown, ["@alice:example.com", "@carol:example.com"]);
        assert!(app
            .accounts
            .accounts
            .iter()
            .all(|a| a.state == AccountState::Active));
    }

    #[test]
    fn login_status_distinguishes_no_op_from_fresh_login() {
        assert_eq!(
            lifecycle_login_status(false, "@alice:example.com", None).text(false),
            "logged in: @alice:example.com"
        );
        assert_eq!(
            lifecycle_login_status(true, "@alice:example.com", None).text(false),
            "already logged in: @alice:example.com (no changes)"
        );
        assert_eq!(
            lifecycle_login_status(
                true,
                "@alice:example.com",
                Some("room refresh failed".to_owned())
            )
            .text(false),
            "already logged in: @alice:example.com (no changes); room refresh failed"
        );
    }

    #[test]
    fn post_login_recovery_is_offered_only_after_changed_unverified_login() {
        assert!(should_offer_post_login_recovery(false, Some(false)));
        assert!(should_offer_post_login_recovery(false, None));
        assert!(!should_offer_post_login_recovery(false, Some(true)));
        assert!(!should_offer_post_login_recovery(true, Some(false)));
    }

    #[test]
    fn recovery_success_reports_derived_verification_state() {
        assert_eq!(
            recovery_success_status("@alice:example.com", Some(true), None).text(false),
            "recovered encryption keys: @alice:example.com (device verified)"
        );
        assert_eq!(
            recovery_success_status("@alice:example.com", Some(false), None).text(false),
            "recovered encryption keys: @alice:example.com (device remains unverified)"
        );
    }

    #[test]
    fn backup_enable_success_reports_action() {
        assert_eq!(
            backup_enable_success_status("@alice:example.com", BackupAction::Enabled, None)
                .text(false),
            "enabled megolm backup: @alice:example.com"
        );
        assert_eq!(
            backup_enable_success_status("@alice:example.com", BackupAction::Joined, None)
                .text(false),
            "joined existing megolm backup: @alice:example.com"
        );
        assert_eq!(
            backup_enable_success_status("@alice:example.com", BackupAction::ExportPending, None)
                .text(false),
            "megolm backup export pending for @alice:example.com; retry /backup enable"
        );
        assert_eq!(
            backup_enable_success_status(
                "@alice:example.com",
                BackupAction::AlreadyUploading,
                None
            )
            .text(false),
            "already uploading megolm keys to backup: @alice:example.com"
        );
    }

    #[tokio::test]
    async fn backup_enable_ready_refuses_unverified_account() {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        let mut account = AccountDto {
            account_id: Uuid::from_u128(1),
            user_id: "@alice:example.com".to_owned(),
            state: AccountState::Active,
            device_id: None,
            verified: Some(false),
            backup: Default::default(),
        };

        app.handle_lifecycle_outcome(LifecycleOutcome::BackupEnableReady {
            target: None,
            result: Ok(vec![account.clone()]),
        })
        .await;

        assert_eq!(app.mode, Mode::Compose);
        assert!(app
            .status
            .text(false)
            .contains("account is not verified; recover or verify first"));

        account.verified = Some(true);
        app.handle_lifecycle_outcome(LifecycleOutcome::BackupEnableReady {
            target: None,
            result: Ok(vec![account.clone()]),
        })
        .await;

        assert!(matches!(
            app.mode,
            Mode::RecoveryKey {
                origin: RecoveryOrigin::BackupEnable,
                ..
            }
        ));
        assert!(app.status.text(false).contains("kicks upload only"));
    }

    #[tokio::test]
    async fn backup_enable_failure_is_queued_for_overflow_handling() {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );

        app.handle_lifecycle_outcome(LifecycleOutcome::BackupEnable {
            user_id: "@alice:example.com".to_owned(),
            result: Err("conflict: account is not verified; recover or verify first".to_owned()),
        })
        .await;

        assert_eq!(
            app.pending_command_response.as_deref(),
            Some(
                "backup enable failed for @alice:example.com: conflict: account is not verified; recover or verify first"
            )
        );
    }

    #[test]
    fn normalizes_homeserver_url_scheme() {
        // Bare host gains https://; an explicit scheme is preserved so loopback
        // dev servers can stay on http://.
        assert_eq!(
            normalize_homeserver_url("homeserver.example.org"),
            "https://homeserver.example.org"
        );
        assert_eq!(
            normalize_homeserver_url("  matrix.example.org  "),
            "https://matrix.example.org"
        );
        assert_eq!(
            normalize_homeserver_url("https://matrix.example.org"),
            "https://matrix.example.org"
        );
        assert_eq!(
            normalize_homeserver_url("http://localhost:8008"),
            "http://localhost:8008"
        );
    }

    #[test]
    fn canonicalizes_logout_targets_with_server_information() {
        assert_eq!(
            canonical_logout_target("@alice:example.com").as_deref(),
            Some("@alice:example.com")
        );
        assert_eq!(
            canonical_logout_target("alice:example.com").as_deref(),
            Some("@alice:example.com")
        );
        assert_eq!(
            canonical_logout_target("alice@example.com").as_deref(),
            Some("@alice:example.com")
        );
        assert_eq!(canonical_logout_target("alice"), None);
        assert_eq!(canonical_logout_target("@alice"), None);
    }
}
