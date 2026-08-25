-- ADR 0097: crash-safe, non-secret breadcrumbs for Matrix OAuth QR login.
--
-- Interactive QR data, check codes, OAuth user codes, verification URLs, and
-- tokens deliberately never enter this table.  A row exists only so boot can
-- remove an abandoned SDK staging store or finish an adoption whose encrypted
-- account session committed before the process stopped.
CREATE TABLE matrix_oauth_acquire_flows (
    flow_id            UUID PRIMARY KEY,
    expected_user_id   TEXT NOT NULL UNIQUE
        CHECK (octet_length(expected_user_id) BETWEEN 1 AND 1024),
    presentation       TEXT NOT NULL
        CHECK (presentation IN ('display', 'scan')),
    staging_dir_name   TEXT NOT NULL UNIQUE
        CHECK (octet_length(staging_dir_name) BETWEEN 1 AND 128),
    finalization_state TEXT NOT NULL DEFAULT 'staging'
        CHECK (finalization_state IN ('staging', 'session_committed')),
    account_id         UUID REFERENCES accounts(account_id),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (
        (finalization_state = 'staging' AND account_id IS NULL)
        OR
        (finalization_state = 'session_committed' AND account_id IS NOT NULL)
    )
);

CREATE TRIGGER matrix_oauth_acquire_flows_set_updated_at
    BEFORE UPDATE ON matrix_oauth_acquire_flows
    FOR EACH ROW EXECUTE FUNCTION trigger_set_updated_at();
