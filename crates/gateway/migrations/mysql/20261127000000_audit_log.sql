-- Audit log (GH-50), MySQL / MariaDB. Keep in sync with ../sqlite and ../postgres.

-- One administrative change, sign-in or recording access. Who did it is
-- copied, not referenced: an entry outlives the user. `changes` is JSON
-- with secrets already redacted (see crate::audit).
CREATE TABLE molcajete_audit_log (
    entry_id       VARCHAR(36)  NOT NULL PRIMARY KEY,
    occurred_at    BIGINT       NOT NULL,
    actor_id       VARCHAR(36),
    actor_name     VARCHAR(255),
    source_address VARCHAR(255),
    action         VARCHAR(64)  NOT NULL,
    target_type    VARCHAR(32),
    target_id      VARCHAR(255),
    target_name    VARCHAR(255),
    changes        TEXT,
    INDEX molcajete_audit_log_occurred (occurred_at),
    INDEX molcajete_audit_log_action (action),
    INDEX molcajete_audit_log_actor (actor_name),
    INDEX molcajete_audit_log_target (target_type, target_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin;
