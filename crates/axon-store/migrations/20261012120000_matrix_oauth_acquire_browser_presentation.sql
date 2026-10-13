-- ADR 0117: browser sign-in is a third presentation of the Matrix OAuth
-- acquisition flow, beside the two QR presentations of ADR 0097.
--
-- The check was declared inline without a name, so it carries PostgreSQL's
-- generated one. Both statements run in the migration's single transaction.
ALTER TABLE matrix_oauth_acquire_flows
    DROP CONSTRAINT matrix_oauth_acquire_flows_presentation_check;

ALTER TABLE matrix_oauth_acquire_flows
    ADD CONSTRAINT matrix_oauth_acquire_flows_presentation_check
        CHECK (presentation IN ('display', 'scan', 'browser'));
