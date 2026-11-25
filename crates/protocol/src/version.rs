use std::fmt;

/// Guacamole protocol version, negotiated through the first argument of the
/// `args` instruction during the handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProtocolVersion {
    /// Implied when the server does not announce a version.
    V1_0_0,
    V1_1_0,
    V1_3_0,
    V1_5_0,
}

impl ProtocolVersion {
    pub const LATEST: ProtocolVersion = ProtocolVersion::V1_5_0;

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "VERSION_1_0_0" => Self::V1_0_0,
            "VERSION_1_1_0" => Self::V1_1_0,
            "VERSION_1_3_0" => Self::V1_3_0,
            "VERSION_1_5_0" => Self::V1_5_0,
            other => {
                // Accept future versions and clamp them to the newest one we
                // speak, like guacamole-common does.
                let rest = other.strip_prefix("VERSION_")?;
                let mut parts = rest.split('_').map(|p| p.parse::<u32>());
                let (Some(Ok(major)), Some(Ok(minor))) = (parts.next(), parts.next()) else {
                    return None;
                };
                if (major, minor) > (1, 5) {
                    Self::LATEST
                } else {
                    return None;
                }
            }
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1_0_0 => "VERSION_1_0_0",
            Self::V1_1_0 => "VERSION_1_1_0",
            Self::V1_3_0 => "VERSION_1_3_0",
            Self::V1_5_0 => "VERSION_1_5_0",
        }
    }

    /// Handshake instructions may arrive in any order (and `timezone` is sent).
    pub fn supports_timezone_handshake(self) -> bool {
        self >= Self::V1_1_0
    }

    /// The server may request missing parameters with `required`.
    pub fn supports_required(self) -> bool {
        self >= Self::V1_3_0
    }

    /// `name` handshake instruction and `msg` instruction.
    pub fn supports_name_handshake(self) -> bool {
        self >= Self::V1_5_0
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_and_future_versions() {
        assert_eq!(
            ProtocolVersion::parse("VERSION_1_3_0"),
            Some(ProtocolVersion::V1_3_0)
        );
        assert_eq!(
            ProtocolVersion::parse("VERSION_1_6_0"),
            Some(ProtocolVersion::LATEST)
        );
        assert_eq!(
            ProtocolVersion::parse("VERSION_2_0_0"),
            Some(ProtocolVersion::LATEST)
        );
        assert_eq!(ProtocolVersion::parse("hostname"), None);
        assert_eq!(ProtocolVersion::parse("VERSION_1_2_0"), None);
    }
}
