-- ADR 0097: persist Matrix OAuth sessions without changing legacy Matrix auth.
ALTER TABLE accounts
    ADD COLUMN auth_kind TEXT NOT NULL DEFAULT 'matrix'
        CHECK (auth_kind IN ('matrix', 'oauth')),
    ADD COLUMN oauth_refresh_token_encrypted BYTEA,
    ADD COLUMN oauth_client_id TEXT;

ALTER TABLE accounts
    ADD CONSTRAINT accounts_auth_session_shape CHECK (
        (auth_kind = 'matrix'
            AND oauth_refresh_token_encrypted IS NULL
            AND oauth_client_id IS NULL)
        OR
        (auth_kind = 'oauth'
            AND access_token_encrypted IS NOT NULL
            AND oauth_refresh_token_encrypted IS NOT NULL
            AND oauth_client_id IS NOT NULL
            AND octet_length(oauth_client_id) BETWEEN 1 AND 512
            AND device_id IS NOT NULL)
    );

-- Public OAuth client registrations are shared across accounts. The issuer is
-- the stable authorization-server identity discovered by matrix-rust-sdk;
-- homeserver_url is retained for operator diagnostics and static-registration
-- matching. OAuth access and refresh tokens never enter this table.
CREATE TABLE matrix_oauth_registrations (
    issuer_url     TEXT PRIMARY KEY
        CHECK (octet_length(issuer_url) BETWEEN 1 AND 2048),
    homeserver_url TEXT NOT NULL
        CHECK (octet_length(homeserver_url) BETWEEN 1 AND 2048),
    client_id      TEXT NOT NULL
        CHECK (octet_length(client_id) BETWEEN 1 AND 512),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER matrix_oauth_registrations_set_updated_at
    BEFORE UPDATE ON matrix_oauth_registrations
    FOR EACH ROW EXECUTE FUNCTION trigger_set_updated_at();
