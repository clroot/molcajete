-- molcajete schema (MySQL / MariaDB). Keep in sync with ../sqlite and ../postgres.
-- Integers are BIGINT and ids are strings so the same queries and decoding
-- work on every backend; timestamps are Unix milliseconds.

CREATE TABLE molcajete_user (
    user_id       VARCHAR(36)  NOT NULL PRIMARY KEY,
    username      VARCHAR(128) NOT NULL UNIQUE,
    password_hash VARCHAR(255) NOT NULL,
    is_admin      BIGINT       NOT NULL DEFAULT 0,
    disabled      BIGINT       NOT NULL DEFAULT 0,
    created_at    BIGINT       NOT NULL,
    updated_at    BIGINT       NOT NULL,
    last_login_at BIGINT
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE molcajete_connection (
    connection_id VARCHAR(64)  NOT NULL PRIMARY KEY,
    name          VARCHAR(255) NOT NULL,
    protocol      VARCHAR(32)  NOT NULL,
    guacd_host    VARCHAR(255),
    guacd_port    BIGINT,
    created_at    BIGINT       NOT NULL,
    updated_at    BIGINT       NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE molcajete_connection_parameter (
    connection_id   VARCHAR(64)  NOT NULL,
    parameter_name  VARCHAR(128) NOT NULL,
    parameter_value TEXT         NOT NULL,
    PRIMARY KEY (connection_id, parameter_name),
    FOREIGN KEY (connection_id) REFERENCES molcajete_connection (connection_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE molcajete_connection_permission (
    user_id       VARCHAR(36) NOT NULL,
    connection_id VARCHAR(64) NOT NULL,
    PRIMARY KEY (user_id, connection_id),
    INDEX molcajete_connection_permission_connection (connection_id),
    FOREIGN KEY (user_id) REFERENCES molcajete_user (user_id) ON DELETE CASCADE,
    FOREIGN KEY (connection_id) REFERENCES molcajete_connection (connection_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE molcajete_session (
    token_hash VARCHAR(64) NOT NULL PRIMARY KEY,
    user_id    VARCHAR(36) NOT NULL,
    created_at BIGINT      NOT NULL,
    expires_at BIGINT      NOT NULL,
    INDEX molcajete_session_user (user_id),
    FOREIGN KEY (user_id) REFERENCES molcajete_user (user_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
