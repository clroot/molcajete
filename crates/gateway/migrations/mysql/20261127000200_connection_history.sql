-- Connection history (GH-49), MySQL / MariaDB. Keep in sync with ../sqlite and ../postgres.

-- One remote desktop session, written when its tunnel closes. The user and
-- the connection are copied, not referenced: the history outlives both.
-- `end_status` is the Guacamole status the tunnel closed with;
-- `end_reason` is set when someone else ended the session (admin,
-- user_disabled, user_deleted, access_revoked) and `ended_by` names the
-- administrator whose action did.
CREATE TABLE molcajete_connection_history (
    history_id       VARCHAR(36)  NOT NULL PRIMARY KEY,
    user_id          VARCHAR(36)  NOT NULL,
    username         VARCHAR(128) NOT NULL,
    connection_id    VARCHAR(64)  NOT NULL,
    connection_name  VARCHAR(255) NOT NULL,
    protocol         VARCHAR(32)  NOT NULL,
    peer_address     VARCHAR(255),
    started_at       BIGINT       NOT NULL,
    ended_at         BIGINT       NOT NULL,
    end_status       BIGINT       NOT NULL,
    end_reason       VARCHAR(32),
    ended_by         VARCHAR(128),
    bytes_to_browser BIGINT       NOT NULL DEFAULT 0,
    bytes_to_guacd   BIGINT       NOT NULL DEFAULT 0,
    recording_id     VARCHAR(36),
    INDEX molcajete_connection_history_started (started_at),
    INDEX molcajete_connection_history_user (username),
    INDEX molcajete_connection_history_connection (connection_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
