-- Roles, user groups and connection folders (GH-46). Keep in sync with
-- ../sqlite and ../postgres.
--
-- Roles are a fixed set stored as text: 'user', 'connection_manager' and
-- 'admin'. A user's effective role is the highest of their own and their
-- groups'. Existing administrators become 'admin'.
--
-- A grant gives a user or a group a connection or a folder; a folder grant
-- covers every connection below it. Existing per-user grants stay in
-- molcajete_connection_permission.

ALTER TABLE molcajete_user ADD COLUMN role VARCHAR(32) NOT NULL DEFAULT 'user';
UPDATE molcajete_user SET role = 'admin' WHERE is_admin <> 0;
ALTER TABLE molcajete_user DROP COLUMN is_admin;

CREATE TABLE molcajete_group (
    group_id   VARCHAR(36)  NOT NULL PRIMARY KEY,
    name       VARCHAR(128) NOT NULL UNIQUE,
    role       VARCHAR(32)  NOT NULL DEFAULT 'user',
    created_at BIGINT       NOT NULL,
    updated_at BIGINT       NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE molcajete_group_member (
    group_id VARCHAR(36) NOT NULL,
    user_id  VARCHAR(36) NOT NULL,
    PRIMARY KEY (group_id, user_id),
    INDEX molcajete_group_member_user (user_id),
    FOREIGN KEY (group_id) REFERENCES molcajete_group (group_id) ON DELETE CASCADE,
    FOREIGN KEY (user_id) REFERENCES molcajete_user (user_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

-- Folders. parent_id NULL is the top level.
CREATE TABLE molcajete_connection_group (
    connection_group_id VARCHAR(36)  NOT NULL PRIMARY KEY,
    parent_id           VARCHAR(36),
    name                VARCHAR(255) NOT NULL,
    created_at          BIGINT       NOT NULL,
    updated_at          BIGINT       NOT NULL,
    INDEX molcajete_connection_group_parent (parent_id),
    FOREIGN KEY (parent_id) REFERENCES molcajete_connection_group (connection_group_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

ALTER TABLE molcajete_connection
    ADD COLUMN parent_id VARCHAR(36),
    ADD INDEX molcajete_connection_parent (parent_id),
    ADD FOREIGN KEY (parent_id) REFERENCES molcajete_connection_group (connection_group_id);

CREATE TABLE molcajete_connection_group_permission (
    user_id             VARCHAR(36) NOT NULL,
    connection_group_id VARCHAR(36) NOT NULL,
    PRIMARY KEY (user_id, connection_group_id),
    INDEX molcajete_connection_group_permission_group (connection_group_id),
    FOREIGN KEY (user_id) REFERENCES molcajete_user (user_id) ON DELETE CASCADE,
    FOREIGN KEY (connection_group_id) REFERENCES molcajete_connection_group (connection_group_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE molcajete_group_connection_permission (
    group_id      VARCHAR(36) NOT NULL,
    connection_id VARCHAR(64) NOT NULL,
    PRIMARY KEY (group_id, connection_id),
    INDEX molcajete_group_connection_permission_connection (connection_id),
    FOREIGN KEY (group_id) REFERENCES molcajete_group (group_id) ON DELETE CASCADE,
    FOREIGN KEY (connection_id) REFERENCES molcajete_connection (connection_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;

CREATE TABLE molcajete_group_connection_group_permission (
    group_id            VARCHAR(36) NOT NULL,
    connection_group_id VARCHAR(36) NOT NULL,
    PRIMARY KEY (group_id, connection_group_id),
    INDEX molcajete_group_connection_group_permission_group (connection_group_id),
    FOREIGN KEY (group_id) REFERENCES molcajete_group (group_id) ON DELETE CASCADE,
    FOREIGN KEY (connection_group_id) REFERENCES molcajete_connection_group (connection_group_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
