//! Regression coverage for unbinding identities after real login activity.
mod common;

use axon_store::NewAuthorizationRequest;
use chrono::{Duration, Utc};
use common::{migrated_store, raw_pool};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unbind_invalidates_login_credentials_without_affecting_other_identities() {
    let store = migrated_store().await;
    let other = store
        .bind_identity("google", &Uuid::new_v4().to_string(), None)
        .await
        .unwrap();
    let expires = Utc::now() + Duration::hours(1);
    let other_access = store
        .issue_oauth_token("other", expires, "google", other.id, "web")
        .await
        .unwrap();
    for provider in ["apple", "google", "microsoft"] {
        let subject = Uuid::new_v4().to_string();
        let bind = store
            .create_bind_request(provider, &subject, expires)
            .await
            .unwrap();
        let id = store
            .complete_bind_request(bind.device_code, provider, &subject, None)
            .await
            .unwrap()
            .unwrap();
        let access = store
            .issue_oauth_token("unbind", expires, provider, id, "web")
            .await
            .unwrap();
        let old_hash = Uuid::new_v4().to_string();
        let new_hash = Uuid::new_v4().to_string();
        store
            .issue_refresh_token(&old_hash, id, "web", expires)
            .await
            .unwrap();
        store
            .redeem_refresh_token(&old_hash, Uuid::new_v4(), &new_hash, expires)
            .await
            .unwrap()
            .unwrap();
        let mut codes = Vec::new();
        for redeemed in [false, true] {
            let state = Uuid::new_v4().to_string();
            let request = store
                .create_authorization_request(&NewAuthorizationRequest {
                    client_id: "web",
                    redirect_uri: "https://example.test/callback",
                    code_challenge: "test",
                    code_challenge_method: "S256",
                    client_state: None,
                    provider,
                    upstream_state: &state,
                    upstream_nonce: "test",
                    expires_at: expires,
                })
                .await
                .unwrap();
            let hash = Uuid::new_v4().to_string();
            assert!(store
                .complete_authorization(request, id, &hash)
                .await
                .unwrap());
            if redeemed {
                assert!(store
                    .redeem_authorization_code(&hash)
                    .await
                    .unwrap()
                    .is_some());
            }
            codes.push(hash);
        }

        assert!(store.delete_identity(id).await.unwrap());
        assert!(!store.delete_identity(id).await.unwrap());
        assert!(store.find_identity_by_id(id).await.unwrap().is_none());
        assert!(store.verify_token(&access.token).await.unwrap().is_none());
        let audit = store
            .list_tokens()
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == access.id)
            .unwrap();
        assert!(audit.is_revoked());
        assert!(audit.oauth_identity_id.is_none());
        assert_eq!(audit.provider.as_deref(), Some(provider));
        for hash in [old_hash, new_hash] {
            assert!(store
                .redeem_refresh_token(&hash, Uuid::new_v4(), "unused", expires)
                .await
                .unwrap()
                .is_err());
        }
        for hash in codes {
            assert!(store
                .redeem_authorization_code(&hash)
                .await
                .unwrap()
                .is_none());
        }
        assert!(store
            .find_bind_request(bind.device_code)
            .await
            .unwrap()
            .unwrap()
            .oauth_identity_id
            .is_none());
        assert!(store
            .issue_oauth_token("late", expires, provider, id, "web")
            .await
            .is_err());
        let rebound = store.bind_identity(provider, &subject, None).await.unwrap();
        assert_ne!(rebound.id, id);
        assert!(store.verify_token(&access.token).await.unwrap().is_none());
        store.delete_identity(rebound.id).await.unwrap();
    }
    assert!(store.find_identity_by_id(other.id).await.unwrap().is_some());
    assert!(store
        .verify_token(&other_access.token)
        .await
        .unwrap()
        .is_some());
    store.delete_identity(other.id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unbind_failure_rolls_back_credential_revocation() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let identity = store
        .bind_identity("apple", &Uuid::new_v4().to_string(), None)
        .await
        .unwrap();
    let expires = Utc::now() + Duration::hours(1);
    let access = store
        .issue_oauth_token("rollback", expires, "apple", identity.id, "web")
        .await
        .unwrap();
    let hash = Uuid::new_v4().to_string();
    let refresh = store
        .issue_refresh_token(&hash, identity.id, "web", expires)
        .await
        .unwrap();
    // Hold a child row lock so unbind fails after updating the access token.
    // The bounded lock wait must roll back that update, not partially sign out.
    let mut blocker = pool.begin().await.unwrap();
    sqlx_core::query::query("SELECT id FROM oauth_refresh_tokens WHERE id = $1 FOR UPDATE")
        .bind(refresh)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    assert!(store.delete_identity(identity.id).await.is_err());
    blocker.rollback().await.unwrap();
    assert!(store
        .find_identity_by_id(identity.id)
        .await
        .unwrap()
        .is_some());
    assert!(store.verify_token(&access.token).await.unwrap().is_some());
    assert!(store
        .redeem_refresh_token(&hash, Uuid::new_v4(), &Uuid::new_v4().to_string(), expires)
        .await
        .unwrap()
        .is_ok());
    assert!(store.delete_identity(identity.id).await.unwrap());
}
