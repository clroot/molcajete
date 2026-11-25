-- Audit log (GH-50), SQLite. Keep in sync with ../postgres and ../mysql.

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
    changes        TEXT
);

CREATE INDEX molcajete_audit_log_occurred ON molcajete_audit_log (occurred_at);
CREATE INDEX molcajete_audit_log_action ON molcajete_audit_log (action);
CREATE INDEX molcajete_audit_log_actor ON molcajete_audit_log (actor_name);
CREATE INDEX molcajete_audit_log_target ON molcajete_audit_log (target_type, target_id);
