use std::path::{Path, PathBuf};

use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::config::MIN_ADMIN_TOKEN_LEN;

const MAX_TOKEN_FILE_BYTES: u64 = 4096;

/// The admin listener's credential (ADR 0002 §2.3). No `Debug`, `Display`,
/// or `Clone`; wiped on drop.
pub struct AdminToken(Zeroizing<String>);

impl std::fmt::Debug for AdminToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdminToken(<redacted>)")
    }
}

/// Why the token could not be loaded. Never carries token material.
#[derive(Debug, thiserror::Error)]
pub enum AdminTokenError {
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{path}: not a regular file (symlinks are refused)")]
    NotRegularFile { path: PathBuf },
    #[error("{path}: mode {mode:o} is accessible to group or others; expected 0600")]
    Permissions { path: PathBuf, mode: u32 },
    #[error("{path}: owned by uid {owner}, not Guard's uid {expected}")]
    WrongOwner {
        path: PathBuf,
        owner: u32,
        expected: u32,
    },
    #[error("environment variable {0} is not set or not valid UTF-8")]
    MissingEnv(String),
    #[error(
        "admin token must be at least {MIN_ADMIN_TOKEN_LEN} printable ASCII characters \
         and must not look like a session handle"
    )]
    Weak,
    #[error("file admin tokens are not supported on this platform; use admin.token_env")]
    Unsupported,
}

impl AdminToken {
    pub fn new(token: String) -> Result<Self, AdminTokenError> {
        let token = Zeroizing::new(token);
        let printable = token.bytes().all(|b| b.is_ascii_graphic());
        if token.len() < MIN_ADMIN_TOKEN_LEN
            || !printable
            || token.starts_with(crom_guard::HANDLE_PREFIX)
        {
            return Err(AdminTokenError::Weak);
        }
        Ok(Self(token))
    }

    pub fn from_env(var: &str) -> Result<Self, AdminTokenError> {
        let value = std::env::var(var).map_err(|_| AdminTokenError::MissingEnv(var.to_owned()))?;
        Self::new(value)
    }

    /// Reads the first line of `path`, refusing anything but a regular file
    /// readable only by Guard's user. Checks run on the opened descriptor.
    #[cfg(unix)]
    pub fn from_file(path: &Path) -> Result<Self, AdminTokenError> {
        use std::io::Read;

        use rustix::fs::{FileType, Mode, OFlags};

        let io = |e: &dyn std::fmt::Display| AdminTokenError::Io {
            path: path.to_owned(),
            message: e.to_string(),
        };
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| match e {
            rustix::io::Errno::LOOP => AdminTokenError::NotRegularFile {
                path: path.to_owned(),
            },
            e => io(&e),
        })?;
        let st = rustix::fs::fstat(&fd).map_err(|e| io(&e))?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err(AdminTokenError::NotRegularFile {
                path: path.to_owned(),
            });
        }
        let mode = st.st_mode & 0o777;
        if mode & 0o077 != 0 {
            return Err(AdminTokenError::Permissions {
                path: path.to_owned(),
                mode,
            });
        }
        let euid = rustix::process::geteuid().as_raw();
        if st.st_uid != euid {
            return Err(AdminTokenError::WrongOwner {
                path: path.to_owned(),
                owner: st.st_uid,
                expected: euid,
            });
        }
        let mut text = Zeroizing::new(String::new());
        std::fs::File::from(fd)
            .take(MAX_TOKEN_FILE_BYTES)
            .read_to_string(&mut text)
            .map_err(|e| io(&e))?;
        let line = text.lines().next().unwrap_or("").trim().to_owned();
        Self::new(line)
    }

    #[cfg(not(unix))]
    pub fn from_file(_path: &Path) -> Result<Self, AdminTokenError> {
        Err(AdminTokenError::Unsupported)
    }

    /// Constant-time comparison against a presented bearer value.
    pub fn verify(&self, presented: &str) -> bool {
        bool::from(self.0.as_bytes().ct_eq(presented.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "adm_0123456789abcdef0123456789abcdef";

    /// The token matches only its exact value.
    #[test]
    fn verify_matches_exact_value_only() {
        let t = AdminToken::new(GOOD.into()).unwrap();
        assert!(t.verify(GOOD));
        assert!(!t.verify(&GOOD[..GOOD.len() - 1]));
        assert!(!t.verify(""));
        assert!(!t.verify(&format!("{GOOD}x")));
    }

    /// Short, non-printable, and handle-shaped tokens are refused.
    #[test]
    fn weak_tokens_are_refused() {
        assert!(AdminToken::new("short".into()).is_err());
        assert!(AdminToken::new(format!("{GOOD} space")).is_err());
        assert!(AdminToken::new(format!("gsh_{}", "0".repeat(64))).is_err());
    }

    /// `Debug` output never contains the token.
    #[test]
    fn debug_is_redacted() {
        let t = AdminToken::new(GOOD.into()).unwrap();
        assert!(!format!("{t:?}").contains(GOOD));
    }

    /// A group- or world-readable token file is refused, and the error omits the token.
    #[cfg(unix)]
    #[test]
    fn token_file_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, format!("{GOOD}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = AdminToken::from_file(&path).unwrap_err();
        assert!(matches!(err, AdminTokenError::Permissions { .. }));
        assert!(!err.to_string().contains(GOOD));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(AdminToken::from_file(&path).unwrap().verify(GOOD));
    }

    /// A symlink to a valid token file is refused.
    #[cfg(unix)]
    #[test]
    fn token_file_symlink_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::write(&real, GOOD).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            AdminToken::from_file(&link),
            Err(AdminTokenError::NotRegularFile { .. })
        ));
    }
}
