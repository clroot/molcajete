//! Resuming a session after its tunnel ended, between a molcajete gateway
//! and molcajeted.
//!
//! molcajeted lists [`HANDSHAKE_ARG`] in `args`, right before
//! [`crate::binary::HANDSHAKE_ARG`]. A gateway that wants the session to
//! outlive its tunnel answers it with a key of its own making (the same for
//! the same user, connection and browser tab). molcajeted then keeps the
//! session for a grace period when the connection to the gateway ends
//! without a `disconnect`, and a later handshake with the same key gets that
//! session back instead of a new one.
//!
//! Stock guacd never lists the parameter, and a stock web application
//! sends an empty value for a parameter it has no setting for: the session
//! then ends with its connection, as with guacd.

/// Connection parameter carrying the resume key.
pub const HANDSHAKE_ARG: &str = "molcajete-resume";

/// Second argument of `ready` when the handshake attached to a held
/// session instead of starting one: `ready,<connection id>,resumed`. Only
/// sent to a gateway that gave a key.
pub const READY_RESUMED: &str = "resumed";

/// Connection parameter that overrides molcajeted's grace period for one
/// connection, in seconds; `0` turns resuming off for it.
pub const TIMEOUT_ARG: &str = "resume-timeout";
