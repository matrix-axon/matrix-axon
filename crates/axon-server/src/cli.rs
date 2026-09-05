//! Command-line surface for the `axon` binary.
//!
//! With no subcommand, `axon` runs the server (the default and only
//! long-running mode). The `token` subcommand is the M7b bootstrap path: it
//! mints / lists / revokes the bearer tokens clients use to reach the `/v1/`
//! API, talking only to the database (no sync engine, no HTTP listener). The
//! `search` subcommand is the M9b operator path for the full-text index. The
//! `init` subcommand (M13, ADR 0051) generates a starter config on first run.
//! The `oauth` subcommand (M14, ADR 0054) binds upstream OIDC identities to
//! this instance's one owner and manages them — same DB-only pattern as
//! `token`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use uuid::Uuid;

/// `-V`/`--version` output: mirrors the fields logged in the "axon starting"
/// line (see `main.rs::serve`), so a build can be identified without starting
/// the server.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " git_hash=\"",
    env!("AXON_GIT_HASH"),
    "\" profile=\"",
    env!("AXON_PROFILE"),
    "\" build_time=\"",
    env!("AXON_BUILD_TIME"),
    "\" rustc_version=\"",
    env!("AXON_RUSTC_VERSION"),
    "\""
);

/// Top-level CLI: an optional subcommand, defaulting to "run the server".
#[derive(Debug, Parser)]
#[command(name = "axon-server", version = VERSION, about = "A personal Matrix state layer")]
pub struct Cli {
    /// Path to the TOML config file. Overrides the `AXON_CONFIG` env var and the
    /// `./axon.toml` / platform-config-dir discovery. Applies to the server and
    /// every subcommand.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The `axon` subcommands. Absent → run the server.
#[derive(Debug, Subcommand)]
pub enum Command {
    #[cfg(feature = "dev-tools")]
    /// Developer database maintenance helpers.
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// Manage client→axon bearer tokens (the local-API auth gate).
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// Manage the full-text search index.
    Search {
        #[command(subcommand)]
        action: SearchAction,
    },
    /// Operator actions for Unable-To-Decrypt (UTD) event back-fill.
    Utd {
        #[command(subcommand)]
        action: UtdAction,
    },
    /// Generate a starter configuration file (first-run setup).
    ///
    /// Writes a minimal config (a generated `store_key` plus the Postgres URL) to
    /// the platform config directory, or to `--config <PATH>` when given. On an
    /// interactive terminal it confirms the database URL and offers to mint a
    /// first bearer token; with `--non-interactive` it runs from flags/defaults.
    Init(InitArgs),
    /// Bind upstream OIDC identities to this instance's owner, and manage them.
    Oauth {
        #[command(subcommand)]
        action: OauthAction,
    },
}

#[cfg(feature = "dev-tools")]
/// `axon db …` actions.
#[derive(Debug, Subcommand)]
pub enum DbAction {
    /// Repair sqlx migration checksums/descriptions for already-applied versions.
    ///
    /// This is a local developer escape hatch for checksum drift in
    /// `_sqlx_migrations` after rebases or edited historical migration files. It
    /// never touches application tables. By default it prints a dry run; pass
    /// `--apply` to rewrite the metadata rows.
    RepairMigrations {
        /// Rewrite `_sqlx_migrations` rows in place. Without this flag the command
        /// only prints what it would change.
        #[arg(long)]
        apply: bool,
    },
}

/// Flags for `axon init`.
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Postgres connection URL to write into the generated config. Defaults to
    /// `postgres://axon:axon@127.0.0.1:5432/axon` (the prompt's default too).
    #[arg(long, value_name = "URL")]
    pub database_url: Option<String>,
    /// Overwrite an existing config file. Without this, `init` refuses to clobber
    /// one (regenerating `store_key` would orphan existing encrypted data).
    #[arg(long)]
    pub force: bool,
    /// Never prompt; take every value from flags/defaults. Implied when stdin is
    /// not a terminal.
    #[arg(long)]
    pub non_interactive: bool,
    /// Do not mint a first bearer token even if the database is reachable.
    #[arg(long, conflicts_with_all = ["print_token", "tui_config"])]
    pub no_token: bool,
    /// Mint and print a first bearer token without prompting (requires a reachable
    /// database).
    #[arg(long)]
    pub print_token: bool,
    /// If the database is unreachable, start a matching local Postgres in Docker
    /// without prompting (the container's credentials are derived from the config's
    /// database URL). Requires Docker and a loopback database host.
    #[arg(long)]
    pub start_postgres: bool,
    /// After minting a token, also write it into an axon-tui client config
    /// (`~/.config/axon-tui/config.toml`) without prompting. Implies minting a
    /// token; merges into an existing config, preserving other settings.
    #[arg(long)]
    pub tui_config: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_token_conflicts_with_tui_config() {
        let err = Cli::try_parse_from(["axon", "init", "--no-token", "--tui-config"])
            .expect_err("--tui-config implies minting, so --no-token must conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn revoke_requires_id_or_label() {
        let err = Cli::try_parse_from(["axon", "token", "revoke"])
            .expect_err("revoke needs either an id or a label");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[cfg(feature = "dev-tools")]
    #[test]
    fn db_repair_migrations_parses_apply() {
        let cli = Cli::try_parse_from(["axon", "db", "repair-migrations", "--apply"])
            .expect("valid db repair invocation");
        let Some(Command::Db {
            action: DbAction::RepairMigrations { apply },
        }) = cli.command
        else {
            panic!("expected db repair-migrations");
        };
        assert!(apply);
    }

    #[test]
    fn revoke_rejects_both_id_and_label() {
        let id = Uuid::nil().to_string();
        let err = Cli::try_parse_from(["axon", "token", "revoke", &id, "--label", "laptop"])
            .expect_err("id and --label are mutually exclusive");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn revoke_accepts_label_alone() {
        let cli = Cli::try_parse_from(["axon", "token", "revoke", "--label", "laptop"])
            .expect("label alone is valid");
        let Some(Command::Token {
            action: TokenAction::Revoke { id, label },
        }) = cli.command
        else {
            panic!("expected token revoke");
        };
        assert_eq!(id, None);
        assert_eq!(label.as_deref(), Some("laptop"));
    }

    #[test]
    fn utd_redecrypt_accepts_account_id() {
        let id = Uuid::nil();
        let cli =
            Cli::try_parse_from(["axon", "utd", "redecrypt", "--account-id", &id.to_string()])
                .expect("utd redecrypt is valid");
        let Some(Command::Utd {
            action:
                UtdAction::Redecrypt {
                    account_id,
                    base_url,
                    token,
                },
        }) = cli.command
        else {
            panic!("expected utd redecrypt");
        };
        assert_eq!(account_id, id);
        assert_eq!(base_url, None);
        assert_eq!(token, None);
    }

    #[test]
    fn oauth_bind_requires_provider() {
        let err =
            Cli::try_parse_from(["axon", "oauth", "bind"]).expect_err("bind needs --provider");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn oauth_bind_parses_provider() {
        let cli = Cli::try_parse_from(["axon", "oauth", "bind", "--provider", "google"])
            .expect("valid bind invocation");
        let Some(Command::Oauth {
            action: OauthAction::Bind { provider },
        }) = cli.command
        else {
            panic!("expected oauth bind");
        };
        assert_eq!(provider, "google");
    }

    #[test]
    fn oauth_identities_unbind_requires_id() {
        let err = Cli::try_parse_from(["axon", "oauth", "identities", "unbind"])
            .expect_err("unbind needs an id");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }
}

/// `axon search …` actions.
#[derive(Debug, Subcommand)]
pub enum SearchAction {
    /// Force a from-scratch rebuild of the search index: clears the seed marker so
    /// the next server start reseeds the whole corpus from Postgres.
    Reindex,
}

/// `axon utd …` actions.
#[derive(Debug, Subcommand)]
pub enum UtdAction {
    /// Explicitly retry pending UTD re-decryption for one active account.
    Redecrypt {
        /// Axon account id to retry.
        #[arg(long)]
        account_id: Uuid,
        /// Axon API base URL. Defaults to AXON_BASE_URL, then http://127.0.0.1:8080.
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
        /// Bearer token for the Axon API. Defaults to AXON_TOKEN.
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
    },
}

/// `axon oauth …` actions.
#[derive(Debug, Subcommand)]
pub enum OauthAction {
    /// Start binding a new upstream identity to this instance's owner.
    ///
    /// Prints a URL to open in any browser (on this machine or elsewhere —
    /// it only needs to reach axon's already-running `/v1/` HTTP surface),
    /// then polls the database until that browser leg completes or the
    /// 10-minute handshake expires.
    Bind {
        /// Which configured provider to bind against, e.g. `google`,
        /// `microsoft`.
        #[arg(long)]
        provider: String,
    },
    /// Manage already-bound identities.
    Identities {
        #[command(subcommand)]
        action: IdentitiesAction,
    },
}

/// `axon oauth identities …` actions.
#[derive(Debug, Subcommand)]
pub enum IdentitiesAction {
    /// List all bound identities.
    List,
    /// Unbind an identity by id (see `list`) and revoke every token/refresh
    /// token it was used to mint — "sign out everywhere" for that identity.
    Unbind {
        /// The identity's id.
        id: Uuid,
    },
}

/// `axon token …` actions.
#[derive(Debug, Subcommand)]
pub enum TokenAction {
    /// Mint a new token and print the secret once (it is never recoverable).
    Issue {
        /// A human-readable label, e.g. the device or client the token is for.
        #[arg(long)]
        label: String,
    },
    /// List all tokens with their status (never prints any secret).
    List,
    /// Revoke a token by id or by label (see `list`).
    #[command(group(clap::ArgGroup::new("target").args(["id", "label"]).required(true)))]
    Revoke {
        /// The token's id.
        id: Option<Uuid>,
        /// Revoke by label instead of id. Must uniquely identify one active
        /// token — if more than one active token has this label, the command
        /// refuses to guess and lists the matches instead.
        #[arg(long)]
        label: Option<String>,
    },
}
