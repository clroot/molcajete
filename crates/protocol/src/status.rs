use std::fmt;

/// Guacamole protocol status code, as carried by `ack` and `error`
/// instructions and by WebSocket close reasons.
///
/// Represented as a newtype rather than an enum so that unknown codes sent by
/// newer peers survive a round trip.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Status(pub u16);

impl Status {
    pub const SUCCESS: Status = Status(0x0000);
    pub const UNSUPPORTED: Status = Status(0x0100);
    pub const SERVER_ERROR: Status = Status(0x0200);
    pub const SERVER_BUSY: Status = Status(0x0201);
    pub const UPSTREAM_TIMEOUT: Status = Status(0x0202);
    pub const UPSTREAM_ERROR: Status = Status(0x0203);
    pub const RESOURCE_NOT_FOUND: Status = Status(0x0204);
    pub const RESOURCE_CONFLICT: Status = Status(0x0205);
    pub const RESOURCE_CLOSED: Status = Status(0x0206);
    pub const UPSTREAM_NOT_FOUND: Status = Status(0x0207);
    pub const UPSTREAM_UNAVAILABLE: Status = Status(0x0208);
    pub const SESSION_CONFLICT: Status = Status(0x0209);
    pub const SESSION_TIMEOUT: Status = Status(0x020A);
    pub const SESSION_CLOSED: Status = Status(0x020B);
    pub const CLIENT_BAD_REQUEST: Status = Status(0x0300);
    pub const CLIENT_UNAUTHORIZED: Status = Status(0x0301);
    pub const CLIENT_FORBIDDEN: Status = Status(0x0303);
    pub const CLIENT_TIMEOUT: Status = Status(0x0308);
    pub const CLIENT_OVERRUN: Status = Status(0x030D);
    pub const CLIENT_BAD_TYPE: Status = Status(0x030F);
    pub const CLIENT_TOO_MANY: Status = Status(0x031D);

    pub fn code(self) -> u16 {
        self.0
    }

    pub fn is_error(self) -> bool {
        self.0 >= 0x0100
    }

    /// Parses a decimal status code as found in instruction arguments.
    pub fn parse(value: &str) -> Option<Status> {
        value.trim().parse().ok().map(Status)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::SUCCESS => "SUCCESS",
            Self::UNSUPPORTED => "UNSUPPORTED",
            Self::SERVER_ERROR => "SERVER_ERROR",
            Self::SERVER_BUSY => "SERVER_BUSY",
            Self::UPSTREAM_TIMEOUT => "UPSTREAM_TIMEOUT",
            Self::UPSTREAM_ERROR => "UPSTREAM_ERROR",
            Self::RESOURCE_NOT_FOUND => "RESOURCE_NOT_FOUND",
            Self::RESOURCE_CONFLICT => "RESOURCE_CONFLICT",
            Self::RESOURCE_CLOSED => "RESOURCE_CLOSED",
            Self::UPSTREAM_NOT_FOUND => "UPSTREAM_NOT_FOUND",
            Self::UPSTREAM_UNAVAILABLE => "UPSTREAM_UNAVAILABLE",
            Self::SESSION_CONFLICT => "SESSION_CONFLICT",
            Self::SESSION_TIMEOUT => "SESSION_TIMEOUT",
            Self::SESSION_CLOSED => "SESSION_CLOSED",
            Self::CLIENT_BAD_REQUEST => "CLIENT_BAD_REQUEST",
            Self::CLIENT_UNAUTHORIZED => "CLIENT_UNAUTHORIZED",
            Self::CLIENT_FORBIDDEN => "CLIENT_FORBIDDEN",
            Self::CLIENT_TIMEOUT => "CLIENT_TIMEOUT",
            Self::CLIENT_OVERRUN => "CLIENT_OVERRUN",
            Self::CLIENT_BAD_TYPE => "CLIENT_BAD_TYPE",
            Self::CLIENT_TOO_MANY => "CLIENT_TOO_MANY",
            _ => "UNKNOWN",
        }
    }

    /// WebSocket close code used when a tunnel terminates with this status
    /// (same mapping as guacamole-common).
    pub fn websocket_close_code(self) -> u16 {
        match self {
            Self::SUCCESS => 1000,
            Self::SERVER_BUSY
            | Self::RESOURCE_CONFLICT
            | Self::SESSION_CONFLICT
            | Self::CLIENT_UNAUTHORIZED
            | Self::CLIENT_FORBIDDEN
            | Self::CLIENT_TOO_MANY => 1008,
            Self::RESOURCE_NOT_FOUND
            | Self::RESOURCE_CLOSED
            | Self::SESSION_TIMEOUT
            | Self::SESSION_CLOSED
            | Self::CLIENT_BAD_REQUEST
            | Self::CLIENT_TIMEOUT => 1002,
            Self::CLIENT_OVERRUN => 1009,
            Self::CLIENT_BAD_TYPE => 1003,
            _ => 1011,
        }
    }

    /// HTTP status equivalent (same mapping as guacamole-common).
    pub fn http_status(self) -> u16 {
        match self {
            Self::SUCCESS => 200,
            Self::UNSUPPORTED => 501,
            Self::SERVER_BUSY => 503,
            Self::UPSTREAM_TIMEOUT => 504,
            Self::UPSTREAM_ERROR | Self::UPSTREAM_NOT_FOUND | Self::UPSTREAM_UNAVAILABLE => 502,
            Self::RESOURCE_NOT_FOUND | Self::RESOURCE_CLOSED | Self::SESSION_CLOSED => 404,
            Self::RESOURCE_CONFLICT | Self::SESSION_CONFLICT => 409,
            Self::SESSION_TIMEOUT | Self::CLIENT_TIMEOUT => 408,
            Self::CLIENT_BAD_REQUEST => 400,
            Self::CLIENT_UNAUTHORIZED | Self::CLIENT_FORBIDDEN => 403,
            Self::CLIENT_OVERRUN => 413,
            Self::CLIENT_BAD_TYPE => 415,
            Self::CLIENT_TOO_MANY => 429,
            _ => 500,
        }
    }

    /// Derives a status from a WebSocket close code when the close reason
    /// carries no Guacamole status (mirrors `Guacamole.Status.Code.fromWebSocketCode`).
    pub fn from_websocket_code(code: u16) -> Status {
        match code {
            1000 => Self::SUCCESS,
            1006 | 1015 => Self::UPSTREAM_NOT_FOUND,
            1001 | 1012 | 1013 | 1014 => Self::UPSTREAM_UNAVAILABLE,
            _ => Self::SERVER_ERROR,
        }
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}(0x{:04X})", self.name(), self.0)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}
