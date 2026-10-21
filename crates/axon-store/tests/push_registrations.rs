//! Push registrations, and the transactions that delete them (ADR 0113).
//!
//! Needs a disposable database. These tests insert credentials and
//! registrations, then delete the rows they created. `reap_push_registrations`
//! deletes every dead registration in the database, so point it at a
//! throwaway database.
//!
//! ```sh
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_push_registrations \
//!   cargo test -p axon-store --test push_registrations -- --ignored --test-threads=1
//! ```

mod common;

use axon_store::{
    NewPushRegistration, OAuthSession, PushRegistrationOutcome, Store, TokenRevocation,
};
use chrono::{Duration, Utc};
use common::{migrated_store, raw_pool};
use sqlx_core::row::Row;
use sqlx_postgres::PgPool;
use uuid::Uuid;

async fn register(store: &Store, session_id: Uuid, token_id: Uuid, token: &str) -> Uuid {
    match store
        .upsert_push_registration(&NewPushRegistration {
            session_id,
            token_id,
            provider: "fcm",
            token,
            environment: None,
            disclosure: "activity",
        })
        .await
        .expect("upsert")
    {
        PushRegistrationOutcome::Stored(row) => row.id,
        other => panic!("expected a stored registration, got {other:?}"),
    }
}

/// A row that the upsert path would now refuse, such as one written before
/// its credential ended.
async fn insert_registration(pool: &PgPool, session_id: Uuid, token_id: Uuid, token: &str) -> Uuid {
    sqlx_core::query::query(
        "INSERT INTO push_registrations \
             (session_id, token_id, provider, token, disclosure) \
         VALUES ($1, $2, 'fcm', $3, 'activity') RETURNING id",
    )
    .bind(session_id)
    .bind(token_id)
    .bind(token)
    .fetch_one(pool)
    .await
    .expect("insert registration")
    .try_get("id")
    .expect("id")
}

async fn session_of(pool: &PgPool, id: Uuid) -> Option<Uuid> {
    sqlx_core::query::query("SELECT session_id FROM push_registrations WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .expect("select registration")
        .map(|row| row.try_get("session_id").expect("session_id"))
}

async fn delete_registration(pool: &PgPool, id: Uuid) {
    sqlx_core::query::query("DELETE FROM push_registrations WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("delete registration");
}

async fn delete_identity(pool: &PgPool, id: Uuid) {
    for sql in [
        "DELETE FROM push_registrations WHERE session_id IN ( \
            SELECT session_id FROM tokens WHERE oauth_identity_id = $1 \
            UNION \
            SELECT session_id FROM oauth_refresh_tokens WHERE oauth_identity_id = $1 \
         )",
        "DELETE FROM oauth_refresh_tokens WHERE oauth_identity_id = $1",
        "DELETE FROM tokens WHERE oauth_identity_id = $1 OR created_by_token_id IN ( \
            SELECT id FROM tokens WHERE oauth_identity_id = $1 \
         )",
        "DELETE FROM oauth_identities WHERE id = $1",
    ] {
        sqlx_core::query::query(sql)
            .bind(id)
            .execute(pool)
            .await
            .expect("cleanup identity");
    }
}

fn session(id: Uuid) -> OAuthSession {
    OAuthSession {
        session_id: id,
        authenticated_at: Some(Utc::now()),
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn rotation_keeps_a_registration_and_reuse_deletes_that_clients_sessions() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let identity = store
        .bind_identity("google", &format!("sub-{}", Uuid::new_v4()), None)
        .await
        .expect("bind");
    let kept = store
        .bind_identity("google", &format!("sub-{}", Uuid::new_v4()), None)
        .await
        .expect("bind other");

    let session_a = Uuid::new_v4();
    let session_b = Uuid::new_v4();
    let session_other_client = Uuid::new_v4();
    let future = Utc::now() + Duration::days(30);
    let refresh_a = format!("hash-a-{}", Uuid::new_v4());
    store
        .issue_refresh_token(
            &refresh_a,
            identity.id,
            "client-a",
            future,
            session(session_a),
        )
        .await
        .expect("refresh a");
    store
        .issue_refresh_token(
            &format!("hash-b-{}", Uuid::new_v4()),
            identity.id,
            "client-a",
            future,
            session(session_b),
        )
        .await
        .expect("refresh b");
    store
        .issue_refresh_token(
            &format!("hash-c-{}", Uuid::new_v4()),
            identity.id,
            "client-b",
            future,
            session(session_other_client),
        )
        .await
        .expect("refresh other client");
    store
        .issue_refresh_token(
            &format!("hash-kept-{}", Uuid::new_v4()),
            kept.id,
            "client-a",
            future,
            session(Uuid::new_v4()),
        )
        .await
        .expect("refresh kept identity");

    let access = store
        .issue_oauth_token(
            "oauth",
            future,
            "google",
            identity.id,
            "client-a",
            session(session_a),
        )
        .await
        .expect("access");
    let reg_a = register(
        &store,
        session_a,
        access.id,
        &format!("tok-a-{}", Uuid::new_v4()),
    )
    .await;
    let access_b = store
        .issue_oauth_token(
            "oauth-b",
            future,
            "google",
            identity.id,
            "client-a",
            session(session_b),
        )
        .await
        .expect("access b");
    let reg_b = register(
        &store,
        session_b,
        access_b.id,
        &format!("tok-b-{}", Uuid::new_v4()),
    )
    .await;
    let access_other = store
        .issue_oauth_token(
            "oauth-c",
            future,
            "google",
            identity.id,
            "client-b",
            session(session_other_client),
        )
        .await
        .expect("access other client");
    let reg_other = register(
        &store,
        session_other_client,
        access_other.id,
        &format!("tok-c-{}", Uuid::new_v4()),
    )
    .await;
    // The kept identity's refresh was minted with its own session. Register
    // against that row's session so unbind of `identity` must leave it.
    let kept_refresh_session: Uuid = sqlx_core::query::query(
        "SELECT session_id FROM oauth_refresh_tokens WHERE oauth_identity_id = $1",
    )
    .bind(kept.id)
    .fetch_one(&pool)
    .await
    .expect("kept refresh")
    .try_get("session_id")
    .expect("session");
    let access_kept = store
        .issue_oauth_token(
            "oauth-kept",
            future,
            "google",
            kept.id,
            "client-a",
            session(kept_refresh_session),
        )
        .await
        .expect("access kept");
    let reg_kept = register(
        &store,
        kept_refresh_session,
        access_kept.id,
        &format!("tok-kept-{}", Uuid::new_v4()),
    )
    .await;

    let rotated = store
        .redeem_refresh_token(
            &refresh_a,
            Uuid::new_v4(),
            &format!("hash-a2-{}", Uuid::new_v4()),
            future,
        )
        .await
        .expect("redeem")
        .expect("rotated");
    assert_eq!(rotated.session_id, session_a);
    assert_eq!(session_of(&pool, reg_a).await, Some(session_a));

    let revoked = store
        .revoke_refresh_tokens_for_identity_client(identity.id, "client-a")
        .await
        .expect("reuse");
    assert!(revoked >= 2, "session A after rotation and session B");
    assert_eq!(session_of(&pool, reg_a).await, None);
    assert_eq!(session_of(&pool, reg_b).await, None);
    assert_eq!(
        session_of(&pool, reg_other).await,
        Some(session_other_client)
    );
    assert_eq!(
        session_of(&pool, reg_kept).await,
        Some(kept_refresh_session)
    );

    delete_registration(&pool, reg_other).await;
    delete_registration(&pool, reg_kept).await;
    delete_identity(&pool, identity.id).await;
    delete_identity(&pool, kept.id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn revoking_an_access_token_deletes_its_session_including_when_already_revoked() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let identity = store
        .bind_identity("google", &format!("sub-{}", Uuid::new_v4()), None)
        .await
        .expect("bind");
    let session_id = Uuid::new_v4();
    let future = Utc::now() + Duration::hours(1);
    store
        .issue_refresh_token(
            &format!("hash-{}", Uuid::new_v4()),
            identity.id,
            "client",
            future,
            session(session_id),
        )
        .await
        .expect("refresh");
    let access = store
        .issue_oauth_token(
            "oauth",
            future,
            "google",
            identity.id,
            "client",
            session(session_id),
        )
        .await
        .expect("access");
    let registration = register(
        &store,
        session_id,
        access.id,
        &format!("tok-{}", Uuid::new_v4()),
    )
    .await;

    sqlx_core::query::query("UPDATE tokens SET revoked_at = now() WHERE id = $1")
        .bind(access.id)
        .execute(&pool)
        .await
        .expect("mark revoked");
    assert_eq!(
        store
            .revoke_token_allowing_lockout(access.id)
            .await
            .expect("second revoke"),
        TokenRevocation::AlreadyRevoked
    );
    assert_eq!(session_of(&pool, registration).await, None);
    let still_live: bool = sqlx_core::query::query(
        "SELECT EXISTS ( \
            SELECT 1 FROM oauth_refresh_tokens \
             WHERE session_id = $1 AND revoked_at IS NULL \
         ) AS live",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .expect("refresh liveness")
    .try_get("live")
    .expect("live");
    assert!(
        !still_live,
        "the second revoke still ends the refresh session"
    );

    delete_identity(&pool, identity.id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unbind_deletes_the_identity_sessions_and_minted_child_sessions() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let identity = store
        .bind_identity("google", &format!("sub-{}", Uuid::new_v4()), None)
        .await
        .expect("bind");
    let other = store
        .bind_identity("google", &format!("sub-{}", Uuid::new_v4()), None)
        .await
        .expect("bind other");
    let session_id = Uuid::new_v4();
    let other_session = Uuid::new_v4();
    let future = Utc::now() + Duration::hours(1);
    store
        .issue_refresh_token(
            &format!("hash-{}", Uuid::new_v4()),
            identity.id,
            "client",
            future,
            session(session_id),
        )
        .await
        .expect("refresh");
    let access = store
        .issue_oauth_token(
            "oauth",
            future,
            "google",
            identity.id,
            "client",
            session(session_id),
        )
        .await
        .expect("access");
    let child = store
        .issue_token_created_by("minted", access.id)
        .await
        .expect("child");
    let legacy_refresh_session = Uuid::new_v4();
    store
        .issue_refresh_token(
            &format!("hash-legacy-{}", Uuid::new_v4()),
            identity.id,
            "client",
            future,
            session(legacy_refresh_session),
        )
        .await
        .expect("legacy refresh");
    let reg_session = register(
        &store,
        session_id,
        access.id,
        &format!("tok-session-{}", Uuid::new_v4()),
    )
    .await;
    let reg_child = register(
        &store,
        child.id,
        child.id,
        &format!("tok-child-{}", Uuid::new_v4()),
    )
    .await;
    let legacy_access = store
        .issue_oauth_token(
            "oauth-legacy",
            future,
            "google",
            identity.id,
            "client",
            session(legacy_refresh_session),
        )
        .await
        .expect("legacy access");
    let reg_legacy = register(
        &store,
        legacy_refresh_session,
        legacy_access.id,
        &format!("tok-legacy-{}", Uuid::new_v4()),
    )
    .await;
    store
        .issue_refresh_token(
            &format!("hash-other-{}", Uuid::new_v4()),
            other.id,
            "client",
            future,
            session(other_session),
        )
        .await
        .expect("other refresh");
    let other_access = store
        .issue_oauth_token(
            "oauth-other",
            future,
            "google",
            other.id,
            "client",
            session(other_session),
        )
        .await
        .expect("other access");
    let reg_other = register(
        &store,
        other_session,
        other_access.id,
        &format!("tok-other-{}", Uuid::new_v4()),
    )
    .await;

    assert!(store.delete_identity(identity.id).await.expect("unbind"));
    assert_eq!(session_of(&pool, reg_session).await, None);
    assert_eq!(session_of(&pool, reg_child).await, None);
    assert_eq!(session_of(&pool, reg_legacy).await, None);
    assert_eq!(session_of(&pool, reg_other).await, Some(other_session));

    delete_registration(&pool, reg_other).await;
    sqlx_core::query::query("DELETE FROM tokens WHERE id = $1 OR id = $2 OR id = $3")
        .bind(child.id)
        .bind(access.id)
        .bind(legacy_access.id)
        .execute(&pool)
        .await
        .expect("delete leftover tokens");
    delete_identity(&pool, other.id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn reap_drops_a_session_only_when_no_live_credential_remains() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let identity = store
        .bind_identity("google", &format!("sub-{}", Uuid::new_v4()), None)
        .await
        .expect("bind");
    let future = Utc::now() + Duration::hours(1);
    let past = Utc::now() - Duration::minutes(1);

    let live_cli = store.issue_token("live-cli").await.expect("cli");
    let revoked_cli = store.issue_token("revoked-cli").await.expect("revoked cli");
    assert!(store
        .revoke_token(revoked_cli.id)
        .await
        .expect("revoke cli"));

    let live_refresh = Uuid::new_v4();
    let dead_refresh = Uuid::new_v4();
    store
        .issue_refresh_token(
            &format!("hash-live-{}", Uuid::new_v4()),
            identity.id,
            "client",
            future,
            session(live_refresh),
        )
        .await
        .expect("live refresh");
    store
        .issue_refresh_token(
            &format!("hash-dead-{}", Uuid::new_v4()),
            identity.id,
            "client",
            past,
            session(dead_refresh),
        )
        .await
        .expect("dead refresh");
    // An expired access token that shares the live refresh session. Reap
    // must keep the registration, because the refresh token is still live.
    let _expired_access = store
        .issue_oauth_token(
            "expired",
            past,
            "google",
            identity.id,
            "client",
            session(live_refresh),
        )
        .await
        .expect("expired access sharing the live refresh session");
    let dead_access = store
        .issue_oauth_token(
            "dead",
            past,
            "google",
            identity.id,
            "client",
            session(dead_refresh),
        )
        .await
        .expect("dead access");

    let keep_cli = register(
        &store,
        live_cli.id,
        live_cli.id,
        &format!("keep-cli-{}", Uuid::new_v4()),
    )
    .await;
    let drop_cli = insert_registration(
        &pool,
        revoked_cli.id,
        revoked_cli.id,
        &format!("drop-cli-{}", Uuid::new_v4()),
    )
    .await;
    let live_access = store
        .issue_oauth_token(
            "live",
            future,
            "google",
            identity.id,
            "client",
            session(live_refresh),
        )
        .await
        .expect("live access");
    let keep_refresh = register(
        &store,
        live_refresh,
        live_access.id,
        &format!("keep-refresh-{}", Uuid::new_v4()),
    )
    .await;
    let drop_dead = insert_registration(
        &pool,
        dead_refresh,
        dead_access.id,
        &format!("drop-dead-{}", Uuid::new_v4()),
    )
    .await;

    assert!(store
        .session_has_live_refresh_token(live_refresh)
        .await
        .expect("live"));
    assert!(!store
        .session_has_live_refresh_token(dead_refresh)
        .await
        .expect("expired"));
    store.reap_push_registrations().await.expect("reap");

    assert_eq!(session_of(&pool, keep_cli).await, Some(live_cli.id));
    assert_eq!(session_of(&pool, drop_cli).await, None);
    assert_eq!(session_of(&pool, keep_refresh).await, Some(live_refresh));
    assert_eq!(session_of(&pool, drop_dead).await, None);

    let ended = store
        .upsert_push_registration(&NewPushRegistration {
            session_id: revoked_cli.id,
            token_id: revoked_cli.id,
            provider: "fcm",
            token: &format!("after-revoke-{}", Uuid::new_v4()),
            environment: None,
            disclosure: "activity",
        })
        .await
        .expect("upsert after revoke");
    assert_eq!(ended, PushRegistrationOutcome::CredentialEnded);

    delete_registration(&pool, keep_cli).await;
    delete_registration(&pool, keep_refresh).await;
    sqlx_core::query::query("DELETE FROM tokens WHERE id = $1 OR id = $2")
        .bind(live_cli.id)
        .bind(revoked_cli.id)
        .execute(&pool)
        .await
        .expect("delete cli tokens");
    delete_identity(&pool, identity.id).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn send_check_reports_liveness_and_reap_drops_only_a_dead_session() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let live = store.issue_token("send-live").await.expect("live token");
    let dead = store.issue_token("send-dead").await.expect("dead token");
    assert!(store.revoke_token(dead.id).await.expect("revoke"));

    let live_id = register(
        &store,
        live.id,
        live.id,
        &format!("send-live-{}", Uuid::new_v4()),
    )
    .await;
    let dead_id = insert_registration(
        &pool,
        dead.id,
        dead.id,
        &format!("send-dead-{}", Uuid::new_v4()),
    )
    .await;

    let live_check = store
        .push_send_check(live_id)
        .await
        .expect("check")
        .expect("row");
    assert!(live_check.session_live);
    assert_eq!(live_check.target.id, live_id);
    assert_eq!(live_check.target.provider, "fcm");

    let dead_check = store
        .push_send_check(dead_id)
        .await
        .expect("check")
        .expect("row");
    assert!(!dead_check.session_live);

    store.reap_push_registrations().await.expect("reap");
    assert!(store
        .push_send_check(live_id)
        .await
        .expect("check")
        .is_some());
    assert!(store
        .push_send_check(dead_id)
        .await
        .expect("check")
        .is_none());

    delete_registration(&pool, live_id).await;
    sqlx_core::query::query("DELETE FROM tokens WHERE id = $1 OR id = $2")
        .bind(live.id)
        .bind(dead.id)
        .execute(&pool)
        .await
        .expect("delete tokens");
}
