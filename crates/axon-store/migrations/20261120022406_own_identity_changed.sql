ALTER TABLE accounts
    ADD COLUMN own_identity_master_key BYTEA CHECK (octet_length(own_identity_master_key) = 32),
    ADD COLUMN own_identity_changed BOOLEAN NOT NULL DEFAULT false;

COMMENT ON COLUMN accounts.own_identity_master_key IS
    'Last observed public cross-signing master key. NULL until first observed.';
COMMENT ON COLUMN accounts.own_identity_changed IS
    'Whether a replaced cross-signing identity still requires this Axon device to be cross-signed.';
