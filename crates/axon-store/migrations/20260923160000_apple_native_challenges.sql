-- Native Apple capabilities are hashed; the nonce is a public signed-claim
-- expectation, not the capability used to redeem the flow.
CREATE TABLE oauth_native_challenges (
    hash TEXT PRIMARY KEY,
    purpose TEXT NOT NULL CHECK (purpose IN ('login', 'bind', 'bootstrap')),
    client_id TEXT NOT NULL,
    instance TEXT NOT NULL,
    nonce TEXT NOT NULL,
    authority_hash TEXT,
    expires_at TIMESTAMPTZ NOT NULL DEFAULT (clock_timestamp() + interval '5 minutes'),
    CHECK ((purpose = 'login') = (authority_hash IS NULL))
);
CREATE INDEX oauth_native_challenges_expiry ON oauth_native_challenges (expires_at);
