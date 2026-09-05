#!/bin/sh
# Idempotent first-run wrapper for the axon server container (ADR 0052, Decision 3).
#
# On first boot only, generate the config (a CSPRNG store_key + the Postgres URL)
# by reusing `axon-server init` — no bespoke key-generation shell. $AXON_CONFIG points at
# the mounted data volume, so the store_key persists; later boots find the file,
# skip init, and keep the key stable. We NEVER pass --force (it would regenerate
# store_key and orphan every account's encrypted data).
#
# Compose owns Postgres (started first via depends_on: service_healthy); init owns
# only config. init's --start-postgres helper is loopback-guarded and the DB host
# here is `postgres`, not loopback, so it never engages.
set -eu

: "${AXON_CONFIG:=/var/lib/axon/axon.toml}"

if [ ! -f "$AXON_CONFIG" ]; then
    echo "axon: no config at $AXON_CONFIG — running first-run 'axon-server init'"
    if [ -z "${DATABASE_URL:-}" ]; then
        echo "axon: DATABASE_URL is unset; cannot generate a config" >&2
        exit 1
    fi

    # Token minting at init time depends on how the operator will onboard:
    #
    # * Web bootstrap armed (AXON_SERVER__BOOTSTRAP_WEB_AUTO=true — the default
    #   for this stack): DON'T mint here. The one-time first-credential web
    #   bootstrap must itself be the first credential; it mints one for the web
    #   client when the operator opens the bootstrap URL (ADR 0052). Minting here
    #   — or running `axon-server token issue` — before that would consume the bootstrap.
    # * Otherwise: mint + print the first bearer token to the logs by default so
    #   the operator has a credential immediately (it prints only on this first
    #   boot). It then persists in `docker compose logs` — set
    #   AXON_INIT_PRINT_TOKEN=false to suppress it and mint out-of-band instead.
    #   Rotate any exposed token with `axon-server token revoke`.
    if [ "${AXON_SERVER__BOOTSTRAP_WEB_AUTO:-false}" = "true" ]; then
        echo "axon: web bootstrap armed — leaving the first credential to the web setup page (init --no-token)"
        axon-server init --non-interactive --config "$AXON_CONFIG" \
            --database-url "$DATABASE_URL" --no-token
    elif [ "${AXON_INIT_PRINT_TOKEN:-true}" = "false" ]; then
        axon-server init --non-interactive --config "$AXON_CONFIG" \
            --database-url "$DATABASE_URL" --no-token
    else
        axon-server init --non-interactive --config "$AXON_CONFIG" \
            --database-url "$DATABASE_URL" --print-token
    fi
else
    echo "axon: config present at $AXON_CONFIG — skipping init"
fi

# init ran migrations via its DB probe; the server re-runs them idempotently.
exec axon-server --config "$AXON_CONFIG"
