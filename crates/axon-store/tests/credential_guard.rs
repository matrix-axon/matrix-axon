//! The lockout guard and the sign-in time step-up depends on (ADR 0109).
//!
//! Run against a disposable Postgres database, serially: the guard counts
//! every credential on the instance, so these tests clear the credential
//! tables to control what it sees.
mod common;

use axon_store::{IdentityRemoval, Store, TokenRevocation};
use chrono::{Duration, Utc};
use common::{migrated_store, raw_pool};
use sqlx_postgres::PgPool;
use uuid::Uuid;

async fn clear_credentials(pool: &PgPool) {
    for sql in [
        "DELETE FROM oauth_authorization_requests",
        "DELETE FROM oauth_bind_requests",
        "DELETE FROM oauth_refresh_tokens",
        "DELETE FROM tokens",
        "DELETE FROM oauth_identities",
    ] {
        sqlx_core::query::query(sql).execute(pool).await.unwrap();
    }
}

async fn bind(store: &Store, provider: &str) -> Uuid {
    store
        .bind_identity(provider, &Uuid::new_v4().to_string(), None)
        .await
        .unwrap()
        .id
}

fn providers(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_last_credential_is_not_removed_and_a_refusal_changes_nothing() {
    let store = migrated_store().await;
    clear_credentials(&raw_pool().await).await;
    let apple = bind(&store, "apple").await;
    // An hour of access left is not a way back in: it is never a survivor.
    let session = store
        .issue_oauth_token(
            "session",
            Utc::now() + Duration::hours(1),
            "apple",
            apple,
            "web",
            Some(Utc::now()),
        )
        .await
        .unwrap();

    // The identity must not count itself: the guard looks at what is left.
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &providers(&["apple"]))
            .await
            .unwrap(),
        IdentityRemoval::LastCredential
    );
    assert!(store.find_identity_by_id(apple).await.unwrap().is_some());
    assert!(
        store.verify_token(&session.token).await.unwrap().is_some(),
        "a refused unbind must not have revoked the identity's sessions"
    );

    assert_eq!(
        store
            .delete_identity_unless_last_credential(Uuid::new_v4(), &providers(&["apple"]))
            .await
            .unwrap(),
        IdentityRemoval::NotFound
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_non_expiring_token_or_a_usable_identity_is_a_survivor() {
    let store = migrated_store().await;
    let pool = raw_pool().await;

    // A non-expiring token survives the unbind.
    clear_credentials(&pool).await;
    let apple = bind(&store, "apple").await;
    let cli = store.issue_token("laptop").await.unwrap();
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &[])
            .await
            .unwrap(),
        IdentityRemoval::Removed
    );

    // A revoked one does not.
    let apple = bind(&store, "apple").await;
    store.revoke_token(cli.id).await.unwrap();
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &providers(&["apple"]))
            .await
            .unwrap(),
        IdentityRemoval::LastCredential
    );

    // Another identity survives only if its provider can still sign in.
    let google = bind(&store, "google").await;
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &providers(&["apple"]))
            .await
            .unwrap(),
        IdentityRemoval::LastCredential,
        "an identity whose provider is switched off cannot produce a session"
    );
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &providers(&["apple", "google"]))
            .await
            .unwrap(),
        IdentityRemoval::Removed
    );
    assert!(store.find_identity_by_id(google).await.unwrap().is_some());
}

/// Two requests each removing one of the last two credentials. Under
/// `READ COMMITTED` and no shared lock, each counts the other as the survivor
/// and both commit. Repeated, because one lucky interleaving proves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Postgres"]
async fn concurrent_removals_cannot_each_count_the_other_as_the_survivor() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    let usable = providers(&["apple", "google"]);
    for round in 0..25 {
        clear_credentials(&pool).await;
        let apple = bind(&store, "apple").await;
        let google = bind(&store, "google").await;
        let (a, b) = tokio::join!(
            tokio::spawn({
                let (store, usable) = (store.clone(), usable.clone());
                async move {
                    store
                        .delete_identity_unless_last_credential(apple, &usable)
                        .await
                }
            }),
            tokio::spawn({
                let (store, usable) = (store.clone(), usable.clone());
                async move {
                    store
                        .delete_identity_unless_last_credential(google, &usable)
                        .await
                }
            }),
        );
        let mut outcomes = [a.unwrap().unwrap(), b.unwrap().unwrap()];
        outcomes.sort_by_key(|outcome| *outcome == IdentityRemoval::LastCredential);
        assert_eq!(
            outcomes,
            [IdentityRemoval::Removed, IdentityRemoval::LastCredential],
            "round {round}: exactly one removal may go through"
        );
        assert_eq!(store.list_identities().await.unwrap().len(), 1);
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_last_non_expiring_token_is_not_revoked_and_a_refusal_changes_nothing() {
    let store = migrated_store().await;
    clear_credentials(&raw_pool().await).await;
    let only = store.issue_token("only").await.unwrap();

    assert_eq!(
        store
            .revoke_token_unless_last_credential(only.id, &providers(&["google"]))
            .await
            .unwrap(),
        TokenRevocation::LastCredential
    );
    assert!(
        store.verify_token(&only.token).await.unwrap().is_some(),
        "a refused revoke must leave the token working"
    );

    // A second non-expiring token is a survivor, so either may now go.
    let other = store.issue_token("other").await.unwrap();
    assert_eq!(
        store
            .revoke_token_unless_last_credential(only.id, &[])
            .await
            .unwrap(),
        TokenRevocation::Revoked
    );
    assert!(store.verify_token(&only.token).await.unwrap().is_none());
    // And then the one that is left is the last again.
    assert_eq!(
        store
            .revoke_token_unless_last_credential(other.id, &[])
            .await
            .unwrap(),
        TokenRevocation::LastCredential
    );

    // A bound identity is a survivor only while its provider can sign in.
    bind(&store, "google").await;
    assert_eq!(
        store
            .revoke_token_unless_last_credential(other.id, &providers(&["apple"]))
            .await
            .unwrap(),
        TokenRevocation::LastCredential
    );
    assert_eq!(
        store
            .revoke_token_unless_last_credential(other.id, &providers(&["google"]))
            .await
            .unwrap(),
        TokenRevocation::Revoked
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn revoking_reports_unknown_and_already_revoked_apart() {
    let store = migrated_store().await;
    clear_credentials(&raw_pool().await).await;
    let keep = store.issue_token("keep").await.unwrap();
    let gone = store.issue_token("gone").await.unwrap();

    assert_eq!(
        store
            .revoke_token_unless_last_credential(Uuid::new_v4(), &[])
            .await
            .unwrap(),
        TokenRevocation::NotFound
    );
    assert_eq!(
        store
            .revoke_token_unless_last_credential(gone.id, &[])
            .await
            .unwrap(),
        TokenRevocation::Revoked
    );
    let revoked_at = |tokens: Vec<axon_store::Token>| {
        tokens
            .into_iter()
            .find(|token| token.id == gone.id)
            .unwrap()
            .revoked_at
    };
    let first = revoked_at(store.list_tokens().await.unwrap());
    for again in [
        store
            .revoke_token_unless_last_credential(gone.id, &[])
            .await
            .unwrap(),
        store.revoke_token_allowing_lockout(gone.id).await.unwrap(),
    ] {
        assert_eq!(again, TokenRevocation::AlreadyRevoked);
    }
    assert_eq!(
        revoked_at(store.list_tokens().await.unwrap()),
        first,
        "a repeat must not restamp the revocation"
    );
    assert!(store.verify_token(&keep.token).await.unwrap().is_some());
}

/// An OAuth access token is never a surviving credential, so revoking one
/// cannot be what locks the owner out, even where nothing else survives.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn revoking_an_expiring_session_token_is_never_the_last_credential() {
    let store = migrated_store().await;
    clear_credentials(&raw_pool().await).await;
    let apple = bind(&store, "apple").await;
    let session = store
        .issue_oauth_token(
            "session",
            Utc::now() + Duration::hours(1),
            "apple",
            apple,
            "web",
            Some(Utc::now()),
        )
        .await
        .unwrap();
    // Apple is switched off: this instance has no survivor at all.
    assert_eq!(
        store
            .revoke_token_unless_last_credential(session.id, &[])
            .await
            .unwrap(),
        TokenRevocation::Revoked
    );
    assert!(store.verify_token(&session.token).await.unwrap().is_none());
}

/// The token form of the race above: two requests each revoking one of the
/// last two non-expiring tokens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Postgres"]
async fn concurrent_revokes_cannot_each_count_the_other_as_the_survivor() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    for round in 0..25 {
        clear_credentials(&pool).await;
        let one = store.issue_token("one").await.unwrap().id;
        let two = store.issue_token("two").await.unwrap().id;
        let revoke = |id| {
            let store = store.clone();
            tokio::spawn(async move { store.revoke_token_unless_last_credential(id, &[]).await })
        };
        let (a, b) = tokio::join!(revoke(one), revoke(two));
        let mut outcomes = [a.unwrap().unwrap(), b.unwrap().unwrap()];
        outcomes.sort_by_key(|outcome| *outcome == TokenRevocation::LastCredential);
        assert_eq!(
            outcomes,
            [TokenRevocation::Revoked, TokenRevocation::LastCredential],
            "round {round}: exactly one revoke may go through"
        );
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_capped_list_keeps_working_tokens_ahead_of_dead_ones() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    clear_credentials(&pool).await;
    let apple = bind(&store, "apple").await;
    let old = store.issue_token("old-but-working").await.unwrap();
    let minted = store
        .issue_token_created_by("minted", old.id)
        .await
        .unwrap();
    // Newer than both, and dead: one revoked, several expired sessions.
    let revoked = store.issue_token("revoked").await.unwrap();
    store.revoke_token(revoked.id).await.unwrap();
    for _ in 0..3 {
        store
            .issue_oauth_token(
                "session",
                Utc::now() - Duration::hours(1),
                "apple",
                apple,
                "web",
                None,
            )
            .await
            .unwrap();
    }

    let top: Vec<Uuid> = store
        .list_tokens_live_first(2)
        .await
        .unwrap()
        .iter()
        .map(|token| token.id)
        .collect();
    assert_eq!(
        top,
        [minted.id, old.id],
        "a cap must drop dead tokens before working ones, however new"
    );

    let all = store.list_tokens_live_first(100).await.unwrap();
    assert_eq!(all.len(), 6);
    let by = |id: Uuid| all.iter().find(|token| token.id == id).unwrap();
    assert_eq!(by(minted.id).created_by_token_id, Some(old.id));
    assert_eq!(by(old.id).created_by_token_id, None);
    assert_eq!(by(minted.id).expires_at, None, "an API mint never expires");
}

/// Revoking a session's access token has to stop the session renewing
/// itself, or the client just refreshes and carries on.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn revoking_a_session_token_revokes_that_clients_refresh_tokens_only() {
    let store = migrated_store().await;
    clear_credentials(&raw_pool().await).await;
    let apple = bind(&store, "apple").await;
    let google = bind(&store, "google").await;
    let later = Utc::now() + Duration::days(30);
    let refresh = |identity: Uuid, client: &'static str| {
        let store = store.clone();
        async move {
            let hash = format!("hash-{}", Uuid::new_v4());
            store
                .issue_refresh_token(&hash, identity, client, later, None)
                .await
                .unwrap();
            hash
        }
    };
    let web = refresh(apple, "web").await;
    let web_again = refresh(apple, "web").await;
    let desktop = refresh(apple, "desktop").await;
    let other_identity = refresh(google, "web").await;
    let session = store
        .issue_oauth_token(
            "session",
            Utc::now() + Duration::hours(1),
            "apple",
            apple,
            "web",
            None,
        )
        .await
        .unwrap();
    // A non-expiring token has no session behind it: revoking one must not
    // touch anybody's refresh tokens.
    let plain = store.issue_token("plain").await.unwrap();
    store.issue_token("survivor").await.unwrap();
    store.revoke_token(plain.id).await.unwrap();
    let redeems = |hash: String| {
        let store = store.clone();
        async move {
            store
                .redeem_refresh_token(
                    &hash,
                    Uuid::new_v4(),
                    &format!("hash-{}", Uuid::new_v4()),
                    later,
                )
                .await
                .unwrap()
                .is_ok()
        }
    };
    let probe = refresh(apple, "web").await;
    assert!(redeems(probe).await, "nothing is revoked yet");

    assert_eq!(
        store
            .revoke_token_unless_last_credential(session.id, &[])
            .await
            .unwrap(),
        TokenRevocation::Revoked
    );
    assert!(!redeems(web).await, "the session's client cannot refresh");
    assert!(!redeems(web_again).await, "nor can its other sessions");
    assert!(redeems(desktop).await, "another client is untouched");
    assert!(
        redeems(other_identity).await,
        "another identity is untouched"
    );
}

/// A token minted from a session carries no identity of its own. Unbinding
/// the identity must still take it, and anything it minted, with it.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn unbinding_revokes_what_that_identitys_sessions_minted() {
    let store = migrated_store().await;
    clear_credentials(&raw_pool().await).await;
    let apple = bind(&store, "apple").await;
    let google = bind(&store, "google").await;
    let session_of = |identity: Uuid, provider: &'static str| {
        let store = store.clone();
        async move {
            store
                .issue_oauth_token(
                    "session",
                    Utc::now() + Duration::hours(1),
                    provider,
                    identity,
                    "web",
                    Some(Utc::now()),
                )
                .await
                .unwrap()
        }
    };
    let apple_session = session_of(apple, "apple").await;
    let google_session = session_of(google, "google").await;
    let minted = store
        .issue_token_created_by("from-apple", apple_session.id)
        .await
        .unwrap();
    let grandchild = store
        .issue_token_created_by("from-that", minted.id)
        .await
        .unwrap();
    let from_google = store
        .issue_token_created_by("from-google", google_session.id)
        .await
        .unwrap();
    let cli = store.issue_token("cli").await.unwrap();
    let from_cli = store
        .issue_token_created_by("from-cli", cli.id)
        .await
        .unwrap();
    let works = |token: String| {
        let store = store.clone();
        async move { store.verify_token(&token).await.unwrap().is_some() }
    };

    // The guard counts what is left after the cascade: with only the Apple
    // identity's own descendants as non-expiring tokens, and no other usable
    // identity, this unbind would lock the owner out.
    store.revoke_token(cli.id).await.unwrap();
    store.revoke_token(from_cli.id).await.unwrap();
    store.revoke_token(from_google.id).await.unwrap();
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &providers(&["apple"]))
            .await
            .unwrap(),
        IdentityRemoval::LastCredential,
        "tokens the unbind would revoke are not survivors"
    );
    assert!(
        works(minted.token.clone()).await,
        "a refusal changes nothing"
    );

    let other = store.issue_token("other").await.unwrap();
    let from_other = store
        .issue_token_created_by("from-other", other.id)
        .await
        .unwrap();
    assert_eq!(
        store
            .delete_identity_unless_last_credential(apple, &providers(&["apple"]))
            .await
            .unwrap(),
        IdentityRemoval::Removed
    );
    assert!(!works(apple_session.token).await);
    assert!(!works(minted.token).await, "minted by the unbound session");
    assert!(!works(grandchild.token).await, "and what that minted");
    assert!(
        works(google_session.token).await,
        "another identity's session"
    );
    assert!(works(other.token).await);
    assert!(
        works(from_other.token).await,
        "minted by an unrelated token"
    );
}

/// A session token revoked by something that did not end the session (a
/// build before the revoke did) must not make a later revoke a no-op.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn revoking_an_already_revoked_session_token_still_ends_the_session() {
    let store = migrated_store().await;
    let pool = raw_pool().await;
    clear_credentials(&pool).await;
    let apple = bind(&store, "apple").await;
    let later = Utc::now() + Duration::days(30);
    let refresh = format!("hash-{}", Uuid::new_v4());
    store
        .issue_refresh_token(&refresh, apple, "web", later, None)
        .await
        .unwrap();
    let session = store
        .issue_oauth_token(
            "session",
            Utc::now() + Duration::hours(1),
            "apple",
            apple,
            "web",
            None,
        )
        .await
        .unwrap();
    sqlx_core::query::query(
        "UPDATE tokens SET revoked_at = now() - interval '1 day' WHERE id = $1",
    )
    .bind(session.id)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(
        store
            .revoke_token_unless_last_credential(session.id, &[])
            .await
            .unwrap(),
        TokenRevocation::AlreadyRevoked
    );
    let redeemed = store
        .redeem_refresh_token(
            &refresh,
            Uuid::new_v4(),
            &format!("hash-{}", Uuid::new_v4()),
            later,
        )
        .await
        .unwrap();
    assert!(redeemed.is_err(), "the session can no longer renew itself");
}

/// Many starts at once. Without one lock around the count and the insert,
/// each reads a count below the cap and all of them insert.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Postgres"]
async fn simultaneous_capped_bind_starts_cannot_exceed_the_cap() {
    let store = Store::connect(
        &std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests"),
        16,
    )
    .await
    .unwrap();
    let pool = raw_pool().await;
    let expires = Utc::now() + Duration::minutes(10);
    for round in 0..5 {
        clear_credentials(&pool).await;
        let starts: Vec<_> = (0..12)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move {
                    store
                        .create_bind_request_unless_too_many(
                            "google",
                            &Uuid::new_v4().to_string(),
                            expires,
                            5,
                        )
                        .await
                        .unwrap()
                        .is_some()
                })
            })
            .collect();
        let mut started = 0;
        for start in starts {
            started += usize::from(start.await.unwrap());
        }
        assert_eq!(started, 5, "round {round}");
    }

    // The uncapped form, the CLI's, is never refused, and a code that is
    // already on record is reported as one a caller can redraw.
    let code = Uuid::new_v4().to_string();
    store
        .create_bind_request("google", &code, expires)
        .await
        .expect("an uncapped start goes through past the cap");
    let taken = store
        .create_bind_request("google", &code, expires)
        .await
        .unwrap_err();
    assert!(taken.is_unique_violation());
    let taken = store
        .create_bind_request_unless_too_many("google", &code, expires, 100)
        .await
        .unwrap_err();
    assert!(taken.is_unique_violation());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_sign_in_time_survives_refresh_rotation_unchanged() {
    let store = migrated_store().await;
    let identity = bind(&store, "google").await;
    let signed_in = Utc::now() - Duration::days(3);
    let first = format!("hash-{}", Uuid::new_v4());
    store
        .issue_refresh_token(
            &first,
            identity,
            "web",
            Utc::now() + Duration::days(30),
            Some(signed_in),
        )
        .await
        .unwrap();

    // Rotate twice: a refresh must never move the time forward.
    let mut presented = first;
    for _ in 0..2 {
        let next = format!("hash-{}", Uuid::new_v4());
        let rotated = store
            .redeem_refresh_token(
                &presented,
                Uuid::new_v4(),
                &next,
                Utc::now() + Duration::days(30),
            )
            .await
            .unwrap()
            .unwrap();
        // Postgres keeps microseconds; compare at that precision.
        assert_eq!(
            rotated.authenticated_at.map(|at| at.timestamp_micros()),
            Some(signed_in.timestamp_micros())
        );
        presented = next;
    }

    let access = store
        .issue_oauth_token(
            "refreshed",
            Utc::now() + Duration::hours(1),
            "google",
            identity,
            "web",
            Some(signed_in),
        )
        .await
        .unwrap();
    let verified = store.verify_token(&access.token).await.unwrap().unwrap();
    assert_eq!(verified.id, access.id);
    assert_eq!(verified.oauth_identity_id, Some(identity));
    assert!(verified.expires_at.is_some());
    assert_eq!(
        verified.authenticated_at.map(|at| at.timestamp_micros()),
        Some(signed_in.timestamp_micros())
    );
    store.delete_identity(identity).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_chain_with_no_recorded_sign_in_stays_without_one() {
    let store = migrated_store().await;
    let identity = bind(&store, "google").await;
    let first = format!("hash-{}", Uuid::new_v4());
    store
        .issue_refresh_token(
            &first,
            identity,
            "web",
            Utc::now() + Duration::days(30),
            None,
        )
        .await
        .unwrap();
    let rotated = store
        .redeem_refresh_token(
            &first,
            Uuid::new_v4(),
            &format!("hash-{}", Uuid::new_v4()),
            Utc::now() + Duration::days(30),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rotated.authenticated_at, None);

    // A token minted on the command line has no sign-in and never expires.
    let cli = store.issue_token("cli").await.unwrap();
    let verified = store.verify_token(&cli.token).await.unwrap().unwrap();
    assert_eq!(verified.expires_at, None);
    assert_eq!(verified.authenticated_at, None);
    assert_eq!(verified.oauth_identity_id, None);
    store.revoke_token(cli.id).await.unwrap();
    store.delete_identity(identity).await.unwrap();
}
