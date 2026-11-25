//! Paths inside a filesystem object, as the client names them, and the
//! directory listings sent back.

use std::fmt;

/// Mimetype of a directory listing (Guacamole's stream index).
pub const INDEX_MIMETYPE: &str = "application/vnd.glyptodon.guacamole.stream-index+json";

/// A path below a filesystem's root: its components, none of them empty,
/// `.` or `..`. The client writes it as `/a/b`; the root is `/`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VirtualPath(Vec<String>);

impl VirtualPath {
    /// Parses a path the client sent. `/` and `\` both separate
    /// components, and empty ones and `.` are skipped; `..` is refused
    /// rather than resolved, so a path can never leave the root. A NUL
    /// is refused too.
    pub fn parse(name: &str) -> Option<Self> {
        let mut components = Vec::new();
        for component in name.split(['/', '\\']) {
            match component {
                "" | "." => {}
                ".." => return None,
                c if c.contains('\0') => return None,
                c => components.push(c.to_owned()),
            }
        }
        Some(Self(components))
    }

    /// A file name for an upload without a path (`file`): its last
    /// component, which must not be `.` or `..`.
    pub fn file_name(name: &str) -> Option<String> {
        let last = name.rsplit(['/', '\\']).next()?;
        (!matches!(last, "" | "." | "..") && !last.contains('\0')).then(|| last.to_owned())
    }

    pub fn components(&self) -> &[String] {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn join(&self, name: &str) -> Self {
        let mut components = self.0.clone();
        components.push(name.to_owned());
        Self(components)
    }
}

impl fmt::Display for VirtualPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("/");
        }
        for component in &self.0 {
            write!(f, "/{component}")?;
        }
        Ok(())
    }
}

/// What a path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Directory,
    /// A regular file of this many bytes.
    File(u64),
}

/// The mimetype of a file in a listing and of its download: Guacamole's
/// `application/octet-stream`, with the size as a parameter so that a
/// client can show progress. Stock clients only compare the mimetype with
/// the stream index's and treat everything else as a file.
pub fn file_mimetype(size: u64) -> String {
    format!("application/octet-stream; size={size}")
}

/// The JSON listing of `directory`: every entry's path mapped to its
/// mimetype, directories to [`INDEX_MIMETYPE`].
pub fn index(directory: &VirtualPath, entries: &[(String, Kind)]) -> Vec<u8> {
    let mut map = serde_json::Map::new();
    for (name, kind) in entries {
        let mimetype = match kind {
            Kind::Directory => INDEX_MIMETYPE.to_owned(),
            Kind::File(size) => file_mimetype(*size),
        };
        map.insert(directory.join(name).to_string(), mimetype.into());
    }
    serde_json::to_vec(&map).unwrap_or_else(|_| b"{}".to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paths_without_leaving_the_root() {
        let path = VirtualPath::parse("/home//user/./notes.txt").unwrap();
        assert_eq!(path.components(), ["home", "user", "notes.txt"]);
        assert_eq!(path.to_string(), "/home/user/notes.txt");
        assert!(VirtualPath::parse("/").unwrap().is_root());
        assert!(VirtualPath::parse("").unwrap().is_root());
        assert_eq!(VirtualPath::parse("/").unwrap().to_string(), "/");
        assert_eq!(
            VirtualPath::parse("\\Windows\\a").unwrap().components(),
            ["Windows", "a"]
        );
        for escaping in ["/..", "/a/../../b", "..\\x", "/a/b\0c"] {
            assert_eq!(VirtualPath::parse(escaping), None, "{escaping:?}");
        }
    }

    #[test]
    fn upload_file_names() {
        assert_eq!(
            VirtualPath::file_name("report.pdf").as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            VirtualPath::file_name("C:\\x\\a.txt").as_deref(),
            Some("a.txt")
        );
        assert_eq!(VirtualPath::file_name("../a.txt").as_deref(), Some("a.txt"));
        for bad in ["", "..", "a/..", "/", "x\0"] {
            assert_eq!(VirtualPath::file_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn listings_map_paths_to_mimetypes() {
        let dir = VirtualPath::parse("/docs").unwrap();
        let json = index(
            &dir,
            &[
                ("a \"b\".txt".into(), Kind::File(12)),
                ("sub".into(), Kind::Directory),
            ],
        );
        let parsed: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({
                "/docs/a \"b\".txt": "application/octet-stream; size=12",
                "/docs/sub": INDEX_MIMETYPE,
            })
        );
    }
}
