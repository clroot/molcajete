-- OpenID Connect sign-in. Keep in sync with ../postgres and ../mysql.

-- Links a user to an identity at an OpenID Connect provider (the ID token's
-- `iss` and `sub`), so a user renamed at the provider keeps their account.
CREATE TABLE molcajete_user_identity (
    issuer     VARCHAR(255) NOT NULL,
    subject    VARCHAR(255) NOT NULL,
    user_id    VARCHAR(36)  NOT NULL REFERENCES molcajete_user (user_id) ON DELETE CASCADE,
    created_at BIGINT       NOT NULL,
    PRIMARY KEY (issuer, subject)
);

CREATE INDEX molcajete_user_identity_user ON molcajete_user_identity (user_id);

-- Sign-ins started at the provider and not finished yet, keyed by the
-- SHA-256 of their `state`; each is used once.
CREATE TABLE molcajete_oidc_login (
    state_hash    VARCHAR(64)  NOT NULL PRIMARY KEY,
    nonce         VARCHAR(255) NOT NULL,
    pkce_verifier VARCHAR(255) NOT NULL,
    created_at    BIGINT       NOT NULL,
    expires_at    BIGINT       NOT NULL
);

-- The ID token of a session opened through the provider, sent back as
-- `id_token_hint` when signing out there.
ALTER TABLE molcajete_session ADD COLUMN id_token TEXT;
