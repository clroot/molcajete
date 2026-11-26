//! Where the files of a filesystem object live: a directory on this host
//! (the RDP drive) or the SSH server, over SFTP.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::StatusCode;
use tokio::io::{AsyncRead, AsyncWrite};

use super::path::{Kind, VirtualPath};

pub type Reader = Box<dyn AsyncRead + Send + Unpin>;
pub type Writer = Box<dyn AsyncWrite + Send + Unpin>;

pub enum Backend {
    Local(LocalDirectory),
    Sftp(Sftp),
}

impl Backend {
    pub async fn stat(&self, path: &VirtualPath) -> io::Result<Kind> {
        match self {
            Self::Local(local) => local.stat(path).await,
            Self::Sftp(sftp) => sftp.stat(path).await,
        }
    }

    /// The entries of a directory: names and kinds. Entries that are
    /// neither directories nor regular files are left out.
    pub async fn list(&self, path: &VirtualPath) -> io::Result<Vec<(String, Kind)>> {
        let mut entries = match self {
            Self::Local(local) => local.list(path).await?,
            Self::Sftp(sftp) => sftp.list(path).await?,
        };
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    pub async fn open(&self, path: &VirtualPath) -> io::Result<Reader> {
        match self {
            Self::Local(local) => local.open(path).await,
            Self::Sftp(sftp) => sftp.open(path).await,
        }
    }

    /// Creates (or truncates) a file for writing.
    pub async fn create(&self, path: &VirtualPath) -> io::Result<Writer> {
        match self {
            Self::Local(local) => local.create(path).await,
            Self::Sftp(sftp) => sftp.create(path).await,
        }
    }

    pub async fn remove(&self, path: &VirtualPath) -> io::Result<()> {
        match self {
            Self::Local(local) => local.remove(path).await,
            Self::Sftp(sftp) => sftp.remove(path).await,
        }
    }

    /// Where uploads without a path (`file`) go.
    pub fn upload_directory(&self) -> &VirtualPath {
        match self {
            Self::Local(local) => &local.uploads,
            Self::Sftp(sftp) => &sftp.uploads,
        }
    }
}

/// A directory on this host. Paths are resolved one component at a time
/// and symbolic links (on Windows, junctions too) are refused, so a link
/// placed in the directory cannot lead the browser outside it. On Windows,
/// components that Windows would read as something else than a file name in
/// the directory (`C:`, `a:stream`, `CON`, `a.`) are refused as well.
pub struct LocalDirectory {
    root: PathBuf,
    uploads: VirtualPath,
}

impl LocalDirectory {
    /// Opens `root`, creating its last component first when `create` is
    /// set (the parent must exist, as with guacd's `create-drive-path`).
    pub async fn open_root(root: &Path, create: bool) -> io::Result<Self> {
        if create {
            match tokio::fs::create_dir(root).await {
                Ok(()) => set_private(root).await,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }
        let mut root = tokio::fs::canonicalize(root).await?;
        if cfg!(windows)
            && let Some(path) = root.to_str()
        {
            root = without_verbatim_prefix(path)?.into();
        }
        if !tokio::fs::metadata(&root).await?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", root.display()),
            ));
        }
        Ok(Self {
            root,
            uploads: VirtualPath::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The host path of `path`. Every component that exists must not be
    /// a symbolic link; only the last one may be missing.
    async fn resolve(&self, path: &VirtualPath) -> io::Result<PathBuf> {
        let mut real = self.root.clone();
        let count = path.components().len();
        for (i, component) in path.components().iter().enumerate() {
            if cfg!(windows)
                && let Some(reason) = windows_name_problem(component)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{component:?} {reason}"),
                ));
            }
            real.push(component);
            match tokio::fs::symlink_metadata(&real).await {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "symbolic links are not followed",
                    ));
                }
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound && i + 1 == count => {}
                Err(err) => return Err(err),
            }
        }
        Ok(real)
    }

    async fn stat(&self, path: &VirtualPath) -> io::Result<Kind> {
        let meta = tokio::fs::symlink_metadata(self.resolve(path).await?).await?;
        kind_of(&meta).ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "not a file"))
    }

    async fn list(&self, path: &VirtualPath) -> io::Result<Vec<(String, Kind)>> {
        let mut dir = tokio::fs::read_dir(self.resolve(path).await?).await?;
        let mut entries = Vec::new();
        while let Some(entry) = dir.next_entry().await? {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // Not followed: a symbolic link is neither.
            if let Ok(meta) = entry.metadata().await
                && let Some(kind) = kind_of(&meta)
            {
                entries.push((name, kind));
            }
        }
        Ok(entries)
    }

    async fn open(&self, path: &VirtualPath) -> io::Result<Reader> {
        let real = self.resolve(path).await?;
        let file = tokio::fs::File::open(&real).await?;
        if !file.metadata().await?.is_file() {
            return Err(io::Error::new(io::ErrorKind::IsADirectory, "not a file"));
        }
        Ok(Box::new(file))
    }

    async fn create(&self, path: &VirtualPath) -> io::Result<Writer> {
        if path.is_root() {
            return Err(io::Error::new(io::ErrorKind::IsADirectory, "not a file"));
        }
        let real = self.resolve(path).await?;
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&real)
            .await?;
        Ok(Box::new(file))
    }

    async fn remove(&self, path: &VirtualPath) -> io::Result<()> {
        tokio::fs::remove_file(self.resolve(path).await?).await
    }
}

/// `path`, a resolved Windows path, in its plain form: `C:\a` for
/// `\\?\C:\a`, as RDP drive redirection takes it. Network shares are
/// refused: neither drive redirection nor guacd's `drive-path` serves them.
fn without_verbatim_prefix(path: &str) -> io::Result<&str> {
    let unsupported = || {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{path} is not on a drive letter; network shares cannot be redirected"),
        )
    };
    let Some(rest) = path.strip_prefix(r"\\?\") else {
        return if path.starts_with(r"\\") {
            Err(unsupported())
        } else {
            Ok(path)
        };
    };
    let bytes = rest.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":\\" {
        Ok(rest)
    } else {
        Err(unsupported())
    }
}

/// Why Windows would not read `component` as the name of a file in its
/// directory; `None` if it would.
///
/// Pushing `C:` onto a path replaces it, `a:b` names a stream of `a`,
/// device names (`CON`, `nul.txt`) open the device wherever they are, and
/// trailing dots and spaces are stripped, so `a.` is `a`.
fn windows_name_problem(component: &str) -> Option<&'static str> {
    if component
        .chars()
        .any(|c| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
    {
        return Some("contains a character Windows does not allow in file names");
    }
    if component.ends_with(['.', ' ']) {
        return Some("ends with a dot or a space");
    }
    // `nul.txt` and `NUL .txt` are the device too: the part before the
    // first dot, without trailing spaces.
    let stem = component
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ');
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.get(..3)
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
            && matches!(
                &stem[3..],
                "1" | "2"
                    | "3"
                    | "4"
                    | "5"
                    | "6"
                    | "7"
                    | "8"
                    | "9"
                    | "\u{b9}"
                    | "\u{b2}"
                    | "\u{b3}"
            )
    });
    reserved.then_some("is a Windows device name")
}

fn kind_of(meta: &std::fs::Metadata) -> Option<Kind> {
    let file_type = meta.file_type();
    if file_type.is_dir() {
        Some(Kind::Directory)
    } else if file_type.is_file() {
        Some(Kind::File(meta.len()))
    } else {
        None
    }
}

/// Keeps a drive directory molcajeted creates to its own user.
#[cfg(unix)]
async fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await;
}

#[cfg(not(unix))]
async fn set_private(_path: &Path) {}

/// The SSH server's files, through an SFTP session on the connection of the
/// terminal. What the user may read and write is up to the server; the root
/// directory only sets where browsing starts and what it shows, like
/// guacd's `sftp-root-directory`.
pub struct Sftp {
    session: Arc<SftpSession>,
    root: String,
    uploads: VirtualPath,
}

impl Sftp {
    /// Uploads without a path go to the user's home directory when it lies
    /// below `root`, to `root` otherwise.
    pub async fn new(session: SftpSession, root: &str) -> Self {
        let root = match root.trim_end_matches('/') {
            "" => "/".to_owned(),
            trimmed => trimmed.to_owned(),
        };
        let home = session.canonicalize(".").await.ok();
        let uploads = home
            .as_deref()
            .and_then(|home| below(&root, home))
            .and_then(VirtualPath::parse)
            .unwrap_or_default();
        Self {
            session: Arc::new(session),
            root,
            uploads,
        }
    }

    fn real(&self, path: &VirtualPath) -> String {
        let mut real = if self.root == "/" {
            String::new()
        } else {
            self.root.clone()
        };
        for component in path.components() {
            real.push('/');
            real.push_str(component);
        }
        if real.is_empty() {
            real.push('/');
        }
        real
    }

    async fn stat(&self, path: &VirtualPath) -> io::Result<Kind> {
        let meta = self
            .session
            .metadata(self.real(path))
            .await
            .map_err(sftp_error)?;
        sftp_kind(&meta).ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "not a file"))
    }

    async fn list(&self, path: &VirtualPath) -> io::Result<Vec<(String, Kind)>> {
        let real = self.real(path);
        let dir = self.session.read_dir(real).await.map_err(sftp_error)?;
        let mut entries = Vec::new();
        for entry in dir {
            let meta = entry.metadata();
            let kind = if meta.is_symlink() {
                // Shown as what it points to, as guacd does.
                match self.session.metadata(entry.path()).await {
                    Ok(target) => sftp_kind(&target),
                    Err(_) => None,
                }
            } else {
                sftp_kind(&meta)
            };
            if let Some(kind) = kind {
                entries.push((entry.file_name(), kind));
            }
        }
        Ok(entries)
    }

    async fn open(&self, path: &VirtualPath) -> io::Result<Reader> {
        let file = self
            .session
            .open(self.real(path))
            .await
            .map_err(sftp_error)?;
        Ok(Box::new(file))
    }

    async fn create(&self, path: &VirtualPath) -> io::Result<Writer> {
        if path.is_root() {
            return Err(io::Error::new(io::ErrorKind::IsADirectory, "not a file"));
        }
        let file = self
            .session
            .create(self.real(path))
            .await
            .map_err(sftp_error)?;
        Ok(Box::new(file))
    }

    async fn remove(&self, path: &VirtualPath) -> io::Result<()> {
        self.session
            .remove_file(self.real(path))
            .await
            .map_err(sftp_error)
    }
}

/// `path` relative to `root`, if it lies below it.
fn below<'a>(root: &str, path: &'a str) -> Option<&'a str> {
    if root == "/" {
        return path.starts_with('/').then_some(path);
    }
    let rest = path.strip_prefix(root)?;
    (rest.is_empty() || rest.starts_with('/')).then_some(rest)
}

fn sftp_kind(meta: &russh_sftp::client::fs::Metadata) -> Option<Kind> {
    if meta.is_dir() {
        Some(Kind::Directory)
    } else if meta.is_regular() {
        Some(Kind::File(meta.size.unwrap_or(0)))
    } else {
        None
    }
}

fn sftp_error(err: russh_sftp::client::error::Error) -> io::Error {
    use russh_sftp::client::error::Error;
    let kind = match &err {
        Error::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => io::ErrorKind::NotFound,
            StatusCode::PermissionDenied => io::ErrorKind::PermissionDenied,
            _ => io::ErrorKind::Other,
        },
        Error::Timeout => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_below_the_root() {
        assert_eq!(below("/", "/home/u"), Some("/home/u"));
        assert_eq!(below("/home", "/home/u"), Some("/u"));
        assert_eq!(below("/home", "/home"), Some(""));
        assert_eq!(below("/home", "/homework"), None);
        assert_eq!(below("/srv", "/home/u"), None);
    }

    #[test]
    fn verbatim_windows_paths_become_plain() {
        assert_eq!(
            without_verbatim_prefix(r"\\?\C:\drive").unwrap(),
            r"C:\drive"
        );
        assert_eq!(without_verbatim_prefix(r"\\?\d:\").unwrap(), r"d:\");
        assert_eq!(without_verbatim_prefix(r"C:\drive").unwrap(), r"C:\drive");
        for share in [
            r"\\?\UNC\server\share",
            r"\\server\share",
            r"\\?\Volume{0}\",
        ] {
            assert_eq!(
                without_verbatim_prefix(share).unwrap_err().kind(),
                io::ErrorKind::Unsupported,
                "{share}"
            );
        }
    }

    #[test]
    fn names_windows_reads_otherwise_are_refused() {
        for name in [
            "C:",
            "a:stream",
            "a<b",
            "a|b",
            "a?",
            "a*",
            "tab\t",
            "a.",
            "a ",
            "..",
            "CON",
            "con",
            "nul.txt",
            "NUL .txt",
            "Com1",
            "lpt9.log",
            "COM\u{b9}",
            "CONIN$",
            "aux.tar.gz",
        ] {
            assert!(windows_name_problem(name).is_some(), "{name:?}");
        }
        for name in [
            "a.txt",
            ".hidden",
            "CONSOLE",
            "com10",
            "COM0",
            "nullable",
            "lpt",
            "한글 파일.txt",
            "a b",
        ] {
            assert_eq!(windows_name_problem(name), None, "{name:?}");
        }
    }

    #[tokio::test]
    async fn local_directory_stays_inside_its_root() {
        let dir = std::env::temp_dir().join(format!("molcajete-files-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.txt"), b"hello").unwrap();
        let outside = std::env::temp_dir().join(format!("molcajete-out-{}", uuid::Uuid::new_v4()));
        std::fs::write(&outside, b"secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        // A junction needs no privileges, unlike a symbolic link.
        #[cfg(windows)]
        {
            let elsewhere = outside.with_extension("dir");
            std::fs::create_dir(&elsewhere).unwrap();
            std::fs::write(elsewhere.join("secret.txt"), b"secret").unwrap();
            let status = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(dir.join("junction"))
                .arg(&elsewhere)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }

        let local = Backend::Local(LocalDirectory::open_root(&dir, false).await.unwrap());
        let root = VirtualPath::default();
        let names: Vec<_> = local.list(&root).await.unwrap();
        assert_eq!(names, [("sub".to_owned(), Kind::Directory)]);
        let file = VirtualPath::parse("/sub/a.txt").unwrap();
        assert_eq!(local.stat(&file).await.unwrap(), Kind::File(5));
        #[cfg(unix)]
        {
            let link = VirtualPath::parse("/link").unwrap();
            assert!(local.open(&link).await.is_err());
            assert!(local.create(&link).await.is_err());
            assert_eq!(std::fs::read(&outside).unwrap(), b"secret");
        }
        #[cfg(windows)]
        {
            let through = VirtualPath::parse("/junction/secret.txt").unwrap();
            assert!(local.open(&through).await.is_err());
            assert!(local.create(&through).await.is_err());
            assert!(
                local
                    .list(&VirtualPath::parse("/junction").unwrap())
                    .await
                    .is_err()
            );
            // `C:` would replace the root.
            let drive = VirtualPath::parse("/C:").unwrap();
            assert!(local.list(&drive).await.is_err());
            assert!(
                local
                    .open(&VirtualPath::parse("/sub/a.txt:x").unwrap())
                    .await
                    .is_err()
            );
            assert!(
                local
                    .open(&VirtualPath::parse("/CON").unwrap())
                    .await
                    .is_err()
            );
        }

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let new = VirtualPath::parse("/sub/b.bin").unwrap();
        let mut writer = local.create(&new).await.unwrap();
        writer.write_all(b"data").await.unwrap();
        writer.shutdown().await.unwrap();
        let mut read = Vec::new();
        local
            .open(&new)
            .await
            .unwrap()
            .read_to_end(&mut read)
            .await
            .unwrap();
        assert_eq!(read, b"data");
        local.remove(&new).await.unwrap();
        assert!(local.stat(&new).await.is_err());

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_file(&outside).unwrap();
        #[cfg(windows)]
        std::fs::remove_dir_all(outside.with_extension("dir")).unwrap();
    }
}
