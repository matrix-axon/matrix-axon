//! Run against a disposable Postgres database, serially.
mod common;
use axon_store::{IdentityRedemption, NativeChallenge};
use chrono::{Duration, Utc};
use common::{migrated_store, raw_pool};
use uuid::Uuid;

fn challenge(purpose: &str, authority_hash: Option<String>) -> NativeChallenge {
    NativeChallenge {
        hash: Uuid::new_v4().to_string(),
        purpose: purpose.into(),
        client_id: "native-test".into(),
        instance: "https://axon.example".into(),
        nonce: Uuid::new_v4().to_string(),
        authority_hash,
    }
}

fn redemption<'a>(subject: &'a str, replay: &'a str) -> IdentityRedemption<'a> {
    IdentityRedemption {
        provider: "apple",
        subject,
        email: None,
        replay_key: replay,
        client_id: "native-test",
        access_expires_at: Utc::now() + Duration::hours(1),
        refresh_expires_at: Utc::now() + Duration::days(1),
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn native_failed_mint_rolls_back_challenge_binding_replay_and_access_token() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let owner = store.issue_token("rollback-owner").await.unwrap();
    let c = challenge("bind", Some(axon_core::hash_secret(&owner.token)));
    assert!(store.create_native_challenge(&c).await.unwrap());
    let remaining: f64 = sqlx_core::query_scalar::query_scalar(
        "SELECT EXTRACT(EPOCH FROM expires_at-clock_timestamp())::float8 FROM oauth_native_challenges WHERE hash=$1",
    ).bind(&c.hash).fetch_one(&pool).await.unwrap();
    let ttl = f64::from(axon_store::NATIVE_CHALLENGE_TTL_SECS);
    assert!(remaining > ttl - 10.0 && remaining <= ttl);
    let subject = Uuid::new_v4().to_string();
    let replay = Uuid::new_v4().to_string();
    let r = redemption(&subject, &replay);
    // Fail the final INSERT, after challenge, identity, replay, and access mint.
    sqlx_core::query::query("ALTER TABLE oauth_refresh_tokens ADD CONSTRAINT test_native_mint_failure CHECK (client_id != 'native-test') NOT VALID")
        .execute(&pool).await.unwrap();
    let failed = store.redeem_identity_atomically(&r, Some(&c)).await;
    sqlx_core::query::query(
        "ALTER TABLE oauth_refresh_tokens DROP CONSTRAINT test_native_mint_failure",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        failed.unwrap_err().diagnostic_reason(),
        "database_constraint"
    );
    assert!(store.native_challenge(&c.hash).await.unwrap().is_some());
    assert!(store
        .find_identity("apple", &subject)
        .await
        .unwrap()
        .is_none());
    let pair = store
        .redeem_identity_atomically(&r, Some(&c))
        .await
        .unwrap()
        .unwrap();
    assert!(store
        .verify_token(&pair.access_token)
        .await
        .unwrap()
        .is_some());
    assert!(store
        .redeem_identity_atomically(&r, Some(&c))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn legacy_identity_redemption_is_atomic_under_concurrency() {
    let store = migrated_store().await;
    for provider in ["google", "microsoft"] {
        let subject = Uuid::new_v4().to_string();
        let replay = Uuid::new_v4().to_string();
        store.bind_identity(provider, &subject, None).await.unwrap();
        let mut r = redemption(&subject, &replay);
        r.provider = provider;
        let (a, b) = tokio::join!(
            store.redeem_identity_atomically(&r, None),
            store.redeem_identity_atomically(&r, None)
        );
        assert_eq!(
            usize::from(a.unwrap().is_some()) + usize::from(b.unwrap().is_some()),
            1
        );
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn native_expiry_context_and_revoked_owner_fail_without_consumption() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let owner = store.issue_token("context-owner").await.unwrap();
    let mut c = challenge("bind", Some(axon_core::hash_secret(&owner.token)));
    store.create_native_challenge(&c).await.unwrap();
    let subject = Uuid::new_v4().to_string();
    let replay = Uuid::new_v4().to_string();
    let r = redemption(&subject, &replay);
    c.instance = "https://other.example".into();
    assert!(store
        .redeem_identity_atomically(&r, Some(&c))
        .await
        .unwrap()
        .is_none());
    c.instance = "https://axon.example".into();
    c.purpose = "login".into();
    assert!(store
        .redeem_identity_atomically(&r, Some(&c))
        .await
        .unwrap()
        .is_none());
    c.purpose = "bind".into();
    store.revoke_token(owner.id).await.unwrap();
    assert!(store
        .redeem_identity_atomically(&r, Some(&c))
        .await
        .unwrap()
        .is_none());
    assert!(store.native_challenge(&c.hash).await.unwrap().is_some());
    sqlx_core::query::query(
        "UPDATE oauth_native_challenges SET expires_at=clock_timestamp() WHERE hash=$1",
    )
    .bind(&c.hash)
    .execute(&pool)
    .await
    .unwrap();
    assert!(store.native_challenge(&c.hash).await.unwrap().is_none());
    assert!(store
        .redeem_identity_atomically(&r, Some(&c))
        .await
        .unwrap()
        .is_none());
    assert!(store
        .find_identity("apple", &subject)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn native_pending_challenges_are_bounded_and_expired_rows_reclaimed() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    sqlx_core::query::query("DELETE FROM oauth_native_challenges")
        .execute(&pool)
        .await
        .unwrap();
    sqlx_core::query::query("INSERT INTO oauth_native_challenges (hash,purpose,client_id,instance,nonce,expires_at)
        SELECT n::text,'login','capacity-test','https://axon.example','nonce',clock_timestamp()+interval '1 hour' FROM generate_series(1,1024) n")
        .execute(&pool).await.unwrap();
    let c = challenge("login", None);
    assert!(!store.create_native_challenge(&c).await.unwrap());
    // A saturated public pool cannot consume the reserved authorized slots.
    for _ in 0..64 {
        assert!(store
            .create_native_challenge(&challenge("bind", Some("owner-hash".into())))
            .await
            .unwrap());
    }
    assert!(!store
        .create_native_challenge(&challenge("bootstrap", Some("session-hash".into())))
        .await
        .unwrap());
    sqlx_core::query::query("UPDATE oauth_native_challenges SET expires_at=clock_timestamp()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.create_native_challenge(&c).await.unwrap());
    sqlx_core::query::query("DELETE FROM oauth_native_challenges")
        .execute(&pool)
        .await
        .unwrap();
}
