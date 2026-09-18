//! Directory-traversal-proof path resolution.
//!
//! Only bare file names are accepted (no `/`, `\`, `.`, `..`); the candidate
//! is then canonicalized and must remain under the root and be a regular
//! file. A symlink pointing outside the root therefore fails.
//!
//! `root` must already be canonical (the server canonicalizes it once at
//! startup) — it is deliberately not re-resolved on every request.

use std::path::{Path, PathBuf};

use crate::error::AppError;

pub fn resolve_under_root(root: &Path, file: &str) -> Result<PathBuf, AppError> {
    // Bare names only: reject empty, ".", "..", and any path separators.
    if file.is_empty() || file == "." || file == ".." || file.contains('/') || file.contains('\\') {
        return Err(AppError::Forbidden("invalid file name".into()));
    }

    let canon = std::fs::canonicalize(root.join(file)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::NotFound("file not found".into())
        } else {
            AppError::Forbidden("path resolution failed".into())
        }
    })?;

    if !canon.starts_with(root) {
        return Err(AppError::Forbidden("path escapes log root".into()));
    }
    if !canon.is_file() {
        return Err(AppError::NotFound("not a regular file".into()));
    }

    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    // The contract requires a canonical root; canonicalize tempdirs (on
    // macOS their /var prefix is a symlink to /private/var).
    fn canon_root(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().unwrap()
    }

    #[test]
    fn bare_name_resolves() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.log"), "x").unwrap();
        let p = resolve_under_root(&canon_root(&dir), "a.log").unwrap();
        assert!(p.ends_with("a.log"));
    }

    #[test]
    fn dotdot_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            resolve_under_root(&canon_root(&dir), "../etc/passwd"),
            Err(AppError::Forbidden(_))
        ));
    }

    #[test]
    fn absolute_path_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            resolve_under_root(&canon_root(&dir), "/etc/passwd"),
            Err(AppError::Forbidden(_))
        ));
    }

    #[test]
    fn missing_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            resolve_under_root(&canon_root(&dir), "nope.log"),
            Err(AppError::NotFound(_))
        ));
    }

    #[test]
    fn symlink_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        symlink(&outside, dir.path().join("link.log")).unwrap();
        assert!(matches!(
            resolve_under_root(&canon_root(&dir), "link.log"),
            Err(AppError::Forbidden(_))
        ));
    }

    #[test]
    fn directory_rejected_as_not_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        assert!(matches!(
            resolve_under_root(&canon_root(&dir), "subdir"),
            Err(AppError::NotFound(_))
        ));
    }
}
