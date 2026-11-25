-- Session recordings (GH-47), PostgreSQL. Keep in sync with ../sqlite and ../mysql.

-- molcajete's own settings of a connection, which are never sent to guacd
-- (like the attributes of Guacamole's JDBC schema).
CREATE TABLE molcajete_connection_attribute (
    connection_id   VARCHAR(64)  NOT NULL REFERENCES molcajete_connection (connection_id) ON DELETE CASCADE,
    attribute_name  VARCHAR(128) NOT NULL,
    attribute_value TEXT         NOT NULL,
    PRIMARY KEY (connection_id, attribute_name)
);

-- One recorded session. The connection and the user are copied, not
-- referenced: a recording outlives both.
CREATE TABLE molcajete_recording (
    recording_id    VARCHAR(36)  NOT NULL PRIMARY KEY,
    connection_id   VARCHAR(64)  NOT NULL,
    connection_name VARCHAR(255) NOT NULL,
    protocol        VARCHAR(32)  NOT NULL,
    user_id         VARCHAR(36)  NOT NULL,
    username        VARCHAR(128) NOT NULL,
    file_name       VARCHAR(255) NOT NULL,
    include_keys    BIGINT       NOT NULL DEFAULT 0,
    started_at      BIGINT       NOT NULL,
    ended_at        BIGINT,
    size_bytes      BIGINT       NOT NULL DEFAULT 0
);

CREATE INDEX molcajete_recording_started ON molcajete_recording (started_at);
