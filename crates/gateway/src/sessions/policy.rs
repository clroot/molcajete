//! Ending the sessions a user may no longer have open.
//!
//! After a change that can take a connection away from someone (a user
//! disabled or deleted, a grant or group membership revoked, a role
//! lowered, a folder or connection moved or deleted), the handler calls
//! [`enforce`]. It checks every live session against the database again,
//! the way the tunnel checked it when it opened
//! ([`Database::connection_for_user`]), and ends those that would not open
//! now. Live sessions are few, so checking them all is simpler and safer
//! than working out who a change affects.
//!
//! A session that joined another needs no grant of the connection: a share
//! link is the grant, and ends with its session. Its user must still exist
//! and be enabled, and an administrator watching must still be one.

use std::collections::HashMap;

use tracing::{info, warn};

use super::{EndReason, Ending, JoinVia, LiveSession};
use crate::AppState;
use crate::audit::{self, Entry, action, target};
use crate::auth::ClientAddress;
use crate::db::{Database, DbError, Role, User};

/// Ends the live sessions whose user may no longer open them, recording
/// each in the audit log as done by `by`. Returns how many were ended.
pub async fn enforce(state: &AppState, by: &User, address: &ClientAddress) -> usize {
    let mut ended = 0;
    for (session, reason) in revoked(&state.db, state.sessions.list()).await {
        if end(state, &session, reason, by, address).await {
            ended += 1;
        }
    }
    ended
}

/// Ends `session` for `reason` on behalf of `by` and records it in the
/// audit log. `false` when it had already closed.
pub async fn end(
    state: &AppState,
    session: &LiveSession,
    reason: EndReason,
    by: &User,
    address: &ClientAddress,
) -> bool {
    let ending = Ending {
        reason,
        by: Some(by.username.clone()),
    };
    if !state.sessions.end(session.id, ending) {
        return false;
    }
    info!(
        session = %session.id,
        user = %session.username,
        connection = %session.connection_id,
        reason = reason.as_str(),
        by = %by.username,
        "ending session"
    );
    let entry = Entry::new(action::SESSION_END)
        .by(by)
        .from(address)
        .target(
            target::SESSION,
            &session.id.to_string(),
            &session.connection_name,
        )
        .detail("reason", reason.as_str())
        .detail("user", session.username.as_str())
        .detail("connection", session.connection_id.as_str())
        .detail("startedAt", session.started_at);
    audit::record(&state.db, entry).await;
    true
}

/// The sessions among `live` whose user may no longer open them, and why.
/// A session whose check fails for another reason (the database is down)
/// is left open.
async fn revoked(db: &Database, live: Vec<LiveSession>) -> Vec<(LiveSession, EndReason)> {
    let mut users: HashMap<String, Result<User, EndReason>> = HashMap::new();
    let mut revoked = Vec::new();
    for session in live {
        if !users.contains_key(&session.user_id) {
            let user = match db.user(&session.user_id).await {
                Ok(user) if user.disabled => Err(EndReason::UserDisabled),
                Ok(user) => Ok(user),
                Err(DbError::NotFound) => Err(EndReason::UserDeleted),
                Err(err) => {
                    warn!(error = %err, session = %session.id, "cannot check a session's user");
                    continue;
                }
            };
            users.insert(session.user_id.clone(), user);
        }
        let user = match &users[&session.user_id] {
            Ok(user) => user,
            Err(reason) => {
                revoked.push((session, *reason));
                continue;
            }
        };
        if let Some(joined) = session.joined {
            if joined.via == JoinVia::Shadow && user.effective_role < Role::Admin {
                revoked.push((session, EndReason::AccessRevoked));
            }
            continue;
        }
        match db.connection_for_user(&session.connection_id, user).await {
            Ok(_) => {}
            Err(DbError::NotFound) => revoked.push((session, EndReason::AccessRevoked)),
            Err(err) => {
                warn!(error = %err, session = %session.id, "cannot check a session's access");
            }
        }
    }
    revoked
}
