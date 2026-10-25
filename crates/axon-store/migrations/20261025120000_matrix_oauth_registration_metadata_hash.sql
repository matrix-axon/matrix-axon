-- Fingerprint of the client metadata a dynamic registration was made with
-- (SHA-256, lowercase hex). A registration whose fingerprint differs from the
-- metadata this build would register is registered again, so a change such as a
-- new logo reaches installs that registered earlier. NULL means unknown: every
-- row that predates this column, and operator-provided static registrations,
-- whose metadata Axon does not own.
ALTER TABLE matrix_oauth_registrations
    ADD COLUMN metadata_hash TEXT
        CHECK (metadata_hash IS NULL OR octet_length(metadata_hash) BETWEEN 1 AND 128);
