//! Developer database maintenance helpers.
//!
//! These commands talk directly to Postgres instead of going through
//! [`axon_store::Store`] because `Store::connect` validates migrations first,
//! which is exactly the failure mode this module repairs.

use std::collections::HashMap;

use anyhow::{bail, Context};
use axon_core::Config;
use axon_store::{embedded_migrations, EmbeddedMigration};
use sqlx_core::query::query;
use sqlx_core::query_as::query_as;
use sqlx_core::query_scalar::query_scalar;
use sqlx_postgres::PgPool;

use crate::cli::DbAction;

pub async fn run(action: DbAction, config: &Config) -> anyhow::Result<()> {
    match action {
        DbAction::RepairMigrations { apply } => repair_migrations(config, apply).await,
    }
}

async fn repair_migrations(config: &Config, apply: bool) -> anyhow::Result<()> {
    let pool = axon_store::Store::database_pool(
        &config.database,
        1,
        config.database.timeouts.migration_statement_secs,
        "axon-db-repair",
    )
    .context("configuring database repair deadlines")?;

    ensure_migrations_table(&pool).await?;

    // Take the same advisory lock sqlx's own migrator holds around
    // `_sqlx_migrations` writes, so this can't race a concurrent `axon` startup
    // or another `repair-migrations --apply` invocation.
    let lock_id = advisory_lock_id(&pool).await?;
    query("SELECT pg_advisory_lock($1)")
        .bind(lock_id)
        .execute(&pool)
        .await
        .context("acquiring migrations advisory lock")?;

    let result = repair_migrations_locked(&pool, apply).await;

    query("SELECT pg_advisory_unlock($1)")
        .bind(lock_id)
        .execute(&pool)
        .await
        .context("releasing migrations advisory lock")?;

    result
}

async fn advisory_lock_id(pool: &PgPool) -> anyhow::Result<i64> {
    let database_name: String = query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .context("reading current_database()")?;

    const CRC_IEEE: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);
    // Matches sqlx-postgres's own `generate_lock_id` so this locks against the
    // same key the real migrator uses (see sqlx_postgres::migrate).
    Ok(0x3d32ad9e * (CRC_IEEE.checksum(database_name.as_bytes()) as i64))
}

async fn repair_migrations_locked(pool: &PgPool, apply: bool) -> anyhow::Result<()> {
    let applied = list_applied_migrations(pool).await?;
    let embedded = embedded_migrations().context("loading embedded migrations")?;
    let plan = plan_repair(&applied, &embedded);

    if !plan.missing_versions.is_empty() {
        print_versions(
            "Applied versions missing from the current source tree:",
            &plan.missing_versions,
        );
    }
    if !plan.dirty_versions.is_empty() {
        print_versions(
            "Dirty migration rows (success = false) detected:",
            &plan.dirty_versions,
        );
    }

    if !plan.missing_versions.is_empty() || !plan.dirty_versions.is_empty() {
        bail!(
            "migration history has blockers beyond checksum drift; this command only repairs \
             signature/description mismatches"
        );
    }

    if plan.updates.is_empty() {
        println!("No migration signature repair needed.");
        return Ok(());
    }

    println!(
        "{} migration metadata row(s) {} update:",
        plan.updates.len(),
        if apply { "to" } else { "would" }
    );
    for update in &plan.updates {
        println!(
            "  {}  {} -> {}",
            update.version, update.current_description, update.expected_description
        );
    }

    if !apply {
        println!();
        println!("Dry run only. Re-run with `axon db repair-migrations --apply` to rewrite `_sqlx_migrations`.");
        return Ok(());
    }

    let mut txn = pool
        .begin()
        .await
        .context("starting migration repair transaction")?;
    for update in &plan.updates {
        query(
            r#"
UPDATE _sqlx_migrations
SET description = $1,
    checksum = $2
WHERE version = $3
  AND success = TRUE
            "#,
        )
        .bind(&update.expected_description)
        .bind(&update.expected_checksum)
        .bind(update.version)
        .execute(&mut *txn)
        .await
        .with_context(|| format!("repairing migration {}", update.version))?;
    }
    txn.commit()
        .await
        .context("committing migration repair transaction")?;

    println!();
    println!(
        "Repaired {} migration metadata row(s). The next Axon start should use the current build's migration checksums.",
        plan.updates.len()
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedMigrationRow {
    version: i64,
    description: String,
    success: bool,
    checksum: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedUpdate {
    version: i64,
    current_description: String,
    expected_description: String,
    expected_checksum: Vec<u8>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RepairPlan {
    updates: Vec<PlannedUpdate>,
    missing_versions: Vec<i64>,
    dirty_versions: Vec<i64>,
}

fn plan_repair(applied: &[AppliedMigrationRow], embedded: &[EmbeddedMigration]) -> RepairPlan {
    let embedded_by_version: HashMap<i64, &EmbeddedMigration> =
        embedded.iter().map(|m| (m.version, m)).collect();
    let mut plan = RepairPlan::default();

    for row in applied {
        if !row.success {
            plan.dirty_versions.push(row.version);
            continue;
        }

        let Some(expected) = embedded_by_version.get(&row.version) else {
            plan.missing_versions.push(row.version);
            continue;
        };

        if row.description != expected.description || row.checksum != expected.checksum {
            plan.updates.push(PlannedUpdate {
                version: row.version,
                current_description: row.description.clone(),
                expected_description: expected.description.clone(),
                expected_checksum: expected.checksum.clone(),
            });
        }
    }

    plan
}

async fn ensure_migrations_table(pool: &sqlx_postgres::PgPool) -> Result<(), sqlx_core::Error> {
    query(
        r#"
CREATE TABLE IF NOT EXISTS _sqlx_migrations (
    version BIGINT PRIMARY KEY,
    description TEXT NOT NULL,
    installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
    success BOOLEAN NOT NULL,
    checksum BYTEA NOT NULL,
    execution_time BIGINT NOT NULL
)
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn list_applied_migrations(
    pool: &sqlx_postgres::PgPool,
) -> Result<Vec<AppliedMigrationRow>, sqlx_core::Error> {
    query_as::<_, (i64, String, bool, Vec<u8>)>(
        r#"
SELECT version, description, success, checksum
FROM _sqlx_migrations
ORDER BY version
        "#,
    )
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(
                |(version, description, success, checksum)| AppliedMigrationRow {
                    version,
                    description,
                    success,
                    checksum,
                },
            )
            .collect()
    })
}

fn print_versions(label: &str, versions: &[i64]) {
    println!("{label}");
    for version in versions {
        println!("  {version}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded(version: i64, description: &str, checksum: &[u8]) -> EmbeddedMigration {
        EmbeddedMigration {
            version,
            description: description.to_owned(),
            checksum: checksum.to_vec(),
        }
    }

    fn applied(
        version: i64,
        description: &str,
        success: bool,
        checksum: &[u8],
    ) -> AppliedMigrationRow {
        AppliedMigrationRow {
            version,
            description: description.to_owned(),
            success,
            checksum: checksum.to_vec(),
        }
    }

    #[test]
    fn plan_repair_collects_checksum_and_description_updates() {
        let applied = vec![applied(1, "old description", true, b"old")];
        let embedded = vec![embedded(1, "new description", b"new")];
        let plan = plan_repair(&applied, &embedded);

        assert_eq!(plan.missing_versions, Vec::<i64>::new());
        assert_eq!(plan.dirty_versions, Vec::<i64>::new());
        assert_eq!(
            plan.updates,
            vec![PlannedUpdate {
                version: 1,
                current_description: "old description".to_owned(),
                expected_description: "new description".to_owned(),
                expected_checksum: b"new".to_vec(),
            }]
        );
    }

    #[test]
    fn plan_repair_flags_missing_and_dirty_rows() {
        let applied = vec![
            applied(1, "present", true, b"same"),
            applied(2, "missing", true, b"other"),
            applied(3, "dirty", false, b"dirty"),
        ];
        let embedded = vec![embedded(1, "present", b"same")];
        let plan = plan_repair(&applied, &embedded);

        assert!(plan.updates.is_empty());
        assert_eq!(plan.missing_versions, vec![2]);
        assert_eq!(plan.dirty_versions, vec![3]);
    }
}
