-- ADR 0098: durable intent for megolm backup enable/export crash-resume.
-- Set true before recovery().enable_backup(); cleared only after export_secrets
-- succeeds. Application code never sets updated_at by hand (existing trigger).
ALTER TABLE accounts
    ADD COLUMN backup_enable_intent BOOLEAN NOT NULL DEFAULT false;
