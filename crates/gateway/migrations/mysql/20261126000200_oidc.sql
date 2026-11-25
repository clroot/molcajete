-- OpenID Connect sign-in. Keep in sync with ../sqlite and ../postgres.

-- Links a user to an identity at an OpenID Connect provider (the ID token's
-- `iss` and `sub`), so a user renamed at the provider keeps their account.
CREATE TABLE molcajete_user_identity (
    issuer     VARCHAR(255) NOT NULL,
    subject    VARCHAR(255) NOT NULL,
    user_id    VARCHAR(36)  NOT NULL,
    created_at BIGINT       NOT NULL,
    PRIMARY KEY (issuer, subject),
    INDEX molcajete_user_identity_user (user_id),
    FOREIGN KEY (user_id) REFERENCES molcajete_user (user_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

-- Sign-ins started at the provider and not finished yet, keyed by the
-- SHA-256 of their `state`; each is used once.
CREATE TABLE molcajete_oidc_login (
    state_hash    VARCHAR(64)  NOT NULL PRIMARY KEY,
    nonce         VARCHAR(255) NOT NULL,
    pkce_verifier VARCHAR(255) NOT NULL,
    created_at    BIGINT       NOT NULL,
    expires_at    BIGINT       NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

-- The ID token of a session opened through the provider, sent back as
-- `id_token_hint` when signing out there.
ALTER TABLE molcajete_session ADD COLUMN id_token TEXT;
