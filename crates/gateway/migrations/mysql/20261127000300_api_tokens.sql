-- API tokens and one-time session links (GH-57), MySQL / MariaDB. Keep in sync with ../sqlite and ../postgres.

-- A bearer token for automation. It acts as its owner with the owner's
-- current role and grants. Only the SHA-256 of the token is stored;
-- `token_prefix` is its first characters, to recognise it in lists.
-- `allowed_ips` holds addresses and CIDR ranges, one per line; NULL
-- allows any address.
CREATE TABLE molcajete_api_token (
    token_id       VARCHAR(36)  NOT NULL PRIMARY KEY,
    token_hash     VARCHAR(64)  NOT NULL UNIQUE,
    token_prefix   VARCHAR(16)  NOT NULL,
    user_id        VARCHAR(36)  NOT NULL,
    name           VARCHAR(255) NOT NULL,
    allowed_ips    TEXT,
    created_at     BIGINT       NOT NULL,
    expires_at     BIGINT       NOT NULL,
    last_used_at   BIGINT,
    last_used_from VARCHAR(255),
    INDEX molcajete_api_token_user (user_id),
    FOREIGN KEY (user_id) REFERENCES molcajete_user (user_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

-- A one-time link that opens a connection as `user_id`, keyed by the
-- SHA-256 of the ticket; deleted when it is used. `parameters` is a JSON
-- object of display parameters set for the session. Revoking the token
-- that made a ticket deletes the ticket.
CREATE TABLE molcajete_session_ticket (
    ticket_hash   VARCHAR(64) NOT NULL PRIMARY KEY,
    user_id       VARCHAR(36) NOT NULL,
    connection_id VARCHAR(64) NOT NULL,
    token_id      VARCHAR(36),
    parameters    TEXT,
    created_at    BIGINT      NOT NULL,
    expires_at    BIGINT      NOT NULL,
    INDEX molcajete_session_ticket_expires (expires_at),
    FOREIGN KEY (user_id) REFERENCES molcajete_user (user_id) ON DELETE CASCADE,
    FOREIGN KEY (connection_id) REFERENCES molcajete_connection (connection_id) ON DELETE CASCADE,
    FOREIGN KEY (token_id) REFERENCES molcajete_api_token (token_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
