-- Instance-wide preferences (ADR 0103): one human per Axon process, so this
-- store is not account-scoped. Space-rail order interleaves spaces from several
-- Matrix accounts and therefore cannot live in Matrix account data or in
-- account-scoped device_state (those vanish with the account).
--
-- Last-write-wins on the whole JSON value. `updated_at` is bumped by the shared
-- trigger on UPDATE (DEFAULT now() on insert).
CREATE TABLE instance_preferences (
    key        TEXT        PRIMARY KEY,
    value      JSONB       NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TRIGGER instance_preferences_set_updated_at
    BEFORE UPDATE ON instance_preferences
    FOR EACH ROW EXECUTE FUNCTION trigger_set_updated_at();
