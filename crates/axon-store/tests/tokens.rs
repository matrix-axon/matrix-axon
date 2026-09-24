//! DB-gated tests for the bearer-token store (M7b).
//!
//! Like the other store tests these need Postgres and are `#[ignore]`d by
//! default:
//!
//! ```sh
//! docker compose up -d postgres
//! DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon cargo test -p axon-store --test tokens -- --ignored
//! ```

mod common;

use chrono::{Duration, Utc};
use common::{migrated_store, raw_pool, test_account};
use sqlx_postgres::PgPool;

async fn bootstrap_request(store: &axon_store::Store) -> axon_store::AuthorizationRequest {
    let state = uuid::Uuid::new_v4().to_string();
    store
        .create_authorization_request(&axon_store::NewAuthorizationRequest {
            client_id: "bootstrap-web",
            redirect_uri: "urn:axon:bootstrap",
            code_challenge: "bootstrap-web",
            code_challenge_method: "S256",
            client_state: None,
            provider: "google",
            upstream_state: &state,
            upstream_nonce: "nonce",
            expires_at: Utc::now() + Duration::minutes(10),
        })
        .await
        .unwrap();
    store
        .find_authorization_request_by_upstream_state("google", &state)
        .await
        .unwrap()
        .unwrap()
}

async fn reset_bootstrap_tables(pool: &PgPool) {
    for sql in [
        "DELETE FROM oauth_authorization_requests",
        "DELETE FROM oauth_bind_requests",
        "DELETE FROM oauth_refresh_tokens",
        "DELETE FROM tokens",
        "DELETE FROM oauth_identities",
        "DELETE FROM accounts",
    ] {
        sqlx_core::query::query(sql).execute(pool).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn issue_then_verify_round_trips_and_touches_last_used() {
    let store = migrated_store().await;

    let issued = store.issue_token("laptop").await.expect("issue");
    assert!(issued.token.starts_with("axon_"), "raw token is prefixed");
    assert_eq!(issued.label, "laptop");

    // The raw token verifies and resolves to its row id.
    let id = store
        .verify_token(&issued.token)
        .await
        .expect("verify")
        .expect("token is accepted");
    assert_eq!(id, issued.id);

    // Verification stamped last_used_at.
    let listed = store.list_tokens().await.expect("list");
    let row = listed
        .iter()
        .find(|t| t.id == issued.id)
        .expect("issued token listed");
    assert_eq!(row.label, "laptop");
    assert!(row.last_used_at.is_some(), "verify touched last_used_at");
    assert!(!row.is_revoked());

    store.revoke_token(issued.id).await.expect("cleanup revoke");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unknown_token_is_rejected() {
    let store = migrated_store().await;
    let got = store
        .verify_token("axon_definitely-not-a-real-token")
        .await
        .expect("verify");
    assert!(got.is_none(), "an unknown token must not verify");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn revoked_token_no_longer_verifies() {
    let store = migrated_store().await;
    let issued = store.issue_token("revoke-me").await.expect("issue");

    // First revoke reports it acted; verification then fails.
    assert!(store.revoke_token(issued.id).await.expect("revoke"));
    assert!(
        store
            .verify_token(&issued.token)
            .await
            .expect("verify")
            .is_none(),
        "a revoked token must not verify"
    );

    // The row survives as a tombstone (revocation is not a delete).
    let listed = store.list_tokens().await.expect("list");
    let row = listed
        .iter()
        .find(|t| t.id == issued.id)
        .expect("revoked token still listed");
    assert!(row.is_revoked());

    // Re-revoking an already-revoked token is a no-op.
    assert!(
        !store.revoke_token(issued.id).await.expect("re-revoke"),
        "re-revoking reports no change"
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn first_credential_bootstrap_mints_only_once() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    reset_bootstrap_tables(&pool).await;

    assert!(
        store
            .first_credential_bootstrap_available()
            .await
            .expect("bootstrap state"),
        "empty DB should allow first-credential bootstrap"
    );
    let issued = store
        .issue_first_bootstrap_token("first")
        .await
        .expect("issue first")
        .expect("first token should mint");
    assert!(issued.token.starts_with("axon_"));
    assert!(
        !store
            .first_credential_bootstrap_available()
            .await
            .expect("bootstrap state"),
        "any token row closes bootstrap"
    );
    assert!(
        store
            .issue_first_bootstrap_token("second")
            .await
            .expect("issue second")
            .is_none(),
        "second bootstrap token should be refused"
    );

    reset_bootstrap_tables(&pool).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn revoked_token_and_account_rows_block_bootstrap() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    reset_bootstrap_tables(&pool).await;

    let issued = store.issue_token("old").await.expect("issue");
    store.revoke_token(issued.id).await.expect("revoke");
    assert!(
        !store
            .first_credential_bootstrap_available()
            .await
            .expect("bootstrap state"),
        "revoked historical tokens still close bootstrap"
    );

    reset_bootstrap_tables(&pool).await;
    let _account_id = test_account(&store, "bootstrap-block").await;
    assert!(
        !store
            .first_credential_bootstrap_available()
            .await
            .expect("bootstrap state"),
        "any Matrix account row closes bootstrap"
    );

    reset_bootstrap_tables(&pool).await;
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn first_oauth_bootstrap_binds_identity_and_mints_tokens_once() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    reset_bootstrap_tables(&pool).await;

    let request = bootstrap_request(&store).await;
    let pair = store
        .issue_first_oauth_token_pair(
            &request,
            "subject-1",
            Some("owner@example.com"),
            Utc::now() + Duration::hours(1),
            Utc::now() + Duration::days(30),
        )
        .await
        .expect("issue oauth")
        .expect("first oauth bootstrap should mint");
    assert!(pair.access_token.starts_with("axon_"));
    assert!(pair.refresh_token.starts_with("axon_rt_"));
    assert!(
        store
            .verify_token(&pair.access_token)
            .await
            .expect("verify")
            .is_some(),
        "bootstrap OAuth access token should verify"
    );
    assert!(
        store
            .find_identity("google", "subject-1")
            .await
            .expect("find identity")
            .is_some(),
        "bootstrap OAuth should bind the first identity"
    );
    assert!(
        store
            .issue_first_oauth_token_pair(
                &request,
                "subject-2",
                None,
                Utc::now() + Duration::hours(1),
                Utc::now() + Duration::days(30),
            )
            .await
            .expect("issue second oauth")
            .is_none(),
        "second bootstrap OAuth credential should be refused"
    );

    reset_bootstrap_tables(&pool).await;
}

#[tokio::test]
#[ignore = "requires empty Postgres"]
async fn canceled_or_expired_bootstrap_flow_cannot_mint_even_when_bootstrap_is_available() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    reset_bootstrap_tables(&pool).await;
    for canceled in [true, false] {
        let request = bootstrap_request(&store).await;
        if canceled {
            assert!(store.cancel_authorization(request.id).await.unwrap());
        } else {
            sqlx_core::query::query("UPDATE oauth_authorization_requests SET expires_at = now() - interval '1 second' WHERE id = $1")
                .bind(request.id).execute(&pool).await.unwrap();
        }
        assert!(store
            .issue_first_oauth_token_pair(
                &request,
                "must-not-bind",
                None,
                Utc::now() + Duration::hours(1),
                Utc::now() + Duration::days(30)
            )
            .await
            .unwrap()
            .is_none());
        assert!(store.first_credential_bootstrap_available().await.unwrap());
        assert!(store
            .find_identity("google", "must-not-bind")
            .await
            .unwrap()
            .is_none());
    }
    reset_bootstrap_tables(&pool).await;
}
