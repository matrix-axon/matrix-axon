-- Device push registrations (ADR 0113).
--
-- One row is one phone. It belongs to a credential session, not to one
-- access-token row: refresh mints a new access token, and the registration
-- has to outlive that row. `token_id` records the access token that last
-- wrote the row. Authorization does not use it, and neither id has a foreign
-- key. A cascade from `tokens` would drop a live registration when that one
-- access token is removed, which is the loss `session_id` exists to avoid.
--
-- The device token is plaintext. The upsert finds the row by `(provider,
-- token)`, and `pgp_sym_encrypt` is non-deterministic, so it cannot be the
-- lookup key. A copy of the token cannot read a Matrix account. Logs and the
-- HTTP response identify a row by `id` and do not include `token`.
--
-- `environment` is `sandbox` or `production` for APNs and null for FCM.
-- `disclosure` is what a later send may put in the banner. It is required at
-- insert so an omitted value cannot land on `preview`.

CREATE TABLE push_registrations (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id   UUID        NOT NULL,
    token_id     UUID        NOT NULL,
    provider     TEXT        NOT NULL,
    token        TEXT        NOT NULL,
    environment  TEXT,
    disclosure   TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT push_registrations_provider_chk
        CHECK (provider IN ('apns', 'fcm')),
    CONSTRAINT push_registrations_disclosure_chk
        CHECK (disclosure IN ('activity', 'sender_and_room', 'preview')),
    CONSTRAINT push_registrations_token_chk
        CHECK (octet_length(token) BETWEEN 1 AND 4096),
    CONSTRAINT push_registrations_environment_chk
        CHECK (
            (provider = 'apns' AND environment IN ('sandbox', 'production'))
            OR (provider = 'fcm' AND environment IS NULL)
        ),
    CONSTRAINT push_registrations_provider_token_key UNIQUE (provider, token)
);

CREATE INDEX push_registrations_session_id_idx
    ON push_registrations (session_id);

CREATE TRIGGER push_registrations_set_updated_at
    BEFORE UPDATE ON push_registrations
    FOR EACH ROW EXECUTE FUNCTION trigger_set_updated_at();
