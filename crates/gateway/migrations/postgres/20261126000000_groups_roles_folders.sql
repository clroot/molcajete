-- Roles, user groups and connection folders (GH-46). Keep in sync with
-- ../sqlite and ../mysql.
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
);

CREATE TABLE molcajete_group_member (
    group_id VARCHAR(36) NOT NULL REFERENCES molcajete_group (group_id) ON DELETE CASCADE,
    user_id  VARCHAR(36) NOT NULL REFERENCES molcajete_user (user_id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, user_id)
);

-- Folders. parent_id NULL is the top level.
CREATE TABLE molcajete_connection_group (
    connection_group_id VARCHAR(36)  NOT NULL PRIMARY KEY,
    parent_id           VARCHAR(36)  REFERENCES molcajete_connection_group (connection_group_id),
    name                VARCHAR(255) NOT NULL,
    created_at          BIGINT       NOT NULL,
    updated_at          BIGINT       NOT NULL
);

ALTER TABLE molcajete_connection ADD COLUMN parent_id VARCHAR(36)
    REFERENCES molcajete_connection_group (connection_group_id);

CREATE TABLE molcajete_connection_group_permission (
    user_id             VARCHAR(36) NOT NULL REFERENCES molcajete_user (user_id) ON DELETE CASCADE,
    connection_group_id VARCHAR(36) NOT NULL REFERENCES molcajete_connection_group (connection_group_id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, connection_group_id)
);

CREATE TABLE molcajete_group_connection_permission (
    group_id      VARCHAR(36) NOT NULL REFERENCES molcajete_group (group_id) ON DELETE CASCADE,
    connection_id VARCHAR(64) NOT NULL REFERENCES molcajete_connection (connection_id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, connection_id)
);

CREATE TABLE molcajete_group_connection_group_permission (
    group_id            VARCHAR(36) NOT NULL REFERENCES molcajete_group (group_id) ON DELETE CASCADE,
    connection_group_id VARCHAR(36) NOT NULL REFERENCES molcajete_connection_group (connection_group_id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, connection_group_id)
);

CREATE INDEX molcajete_group_member_user ON molcajete_group_member (user_id);
CREATE INDEX molcajete_connection_group_parent ON molcajete_connection_group (parent_id);
CREATE INDEX molcajete_connection_parent ON molcajete_connection (parent_id);
CREATE INDEX molcajete_connection_group_permission_group ON molcajete_connection_group_permission (connection_group_id);
CREATE INDEX molcajete_group_connection_permission_connection ON molcajete_group_connection_permission (connection_id);
CREATE INDEX molcajete_group_connection_group_permission_group ON molcajete_group_connection_group_permission (connection_group_id);
