-- ADR 0136: opt in to excluding non-cross-signed devices per account.
ALTER TABLE accounts ADD COLUMN exclude_insecure_devices BOOLEAN NOT NULL DEFAULT false;
