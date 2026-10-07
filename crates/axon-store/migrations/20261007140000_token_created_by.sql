-- Which token minted a token through the management API (ADR 0109).
--
-- `POST /v1/management/tokens` lets a bearer create another bearer. The mint
-- is logged, but a log is no help to an owner reading the token list from a
-- client, so the list itself says where each token came from: an entry the
-- owner does not recognize points at the credential that made it.
--
-- NULL for every token minted any other way: `axon token issue`, `axon init`,
-- the first-run bootstrap, and OAuth sessions, none of which has a calling
-- token. A row written before this migration is one of those.
--
-- Token rows are retained after revocation (the list is the audit trail), so
-- the reference normally outlives both ends. ON DELETE SET NULL only keeps a
-- manual cleanup of old rows from being blocked by the tokens they minted.
ALTER TABLE tokens
    ADD COLUMN created_by_token_id UUID REFERENCES tokens(id) ON DELETE SET NULL;
