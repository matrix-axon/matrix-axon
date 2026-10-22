//! A non-UTF8 database is refused before it is migrated (GH #803). Needs a role
//! that can CREATE DATABASE:
//! DATABASE_URL=... cargo test -p axon-store --test database_encoding -- --ignored

use axon_store::{Store, StoreError};
use sqlx_postgres::PgPool;
use uuid::Uuid;

/// `DATABASE_URL` with its database name replaced, keeping any query string.
fn url_for(database: &str) -> String {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url.as_str(), None),
    };
    let (server, _) = base
        .rsplit_once('/')
        .expect("DATABASE_URL has a database name");
    match query {
        Some(query) => format!("{server}/{database}?{query}"),
        None => format!("{server}/{database}"),
    }
}

#[tokio::test]
#[ignore = "requires Postgres with CREATE DATABASE"]
async fn sql_ascii_database_is_refused_before_migrating() {
    let admin = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
        .await
        .expect("admin connection");
    let name = format!("axon_sql_ascii_{}", Uuid::new_v4().simple());
    // The C locale is compatible with every encoding, so this works on any cluster.
    sqlx_core::query::query(&format!(
        "CREATE DATABASE {name} ENCODING 'SQL_ASCII' TEMPLATE template0 LC_COLLATE 'C' LC_CTYPE 'C'"
    ))
    .execute(&admin)
    .await
    .expect("create SQL_ASCII database");

    let refused = Store::connect(&url_for(&name), 1).await;

    let probe = PgPool::connect(&url_for(&name))
        .await
        .expect("probe connection");
    let migrated: bool =
        sqlx_core::query_scalar::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(&probe)
            .await
            .expect("probe migrations table");
    probe.close().await;
    // FORCE: a wrongly accepted Store still holds a connection.
    sqlx_core::query::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .execute(&admin)
        .await
        .expect("drop SQL_ASCII database");

    match refused {
        Err(StoreError::UnsupportedEncoding(encoding)) => assert_eq!(encoding, "SQL_ASCII"),
        Err(other) => panic!("expected UnsupportedEncoding, got {other}"),
        Ok(_) => panic!("SQL_ASCII database was accepted"),
    }
    assert!(!migrated, "migrations ran on a refused database");
}
