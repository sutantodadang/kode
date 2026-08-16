use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use crate::error::{Result, ToolError};

/// Normalizes `path` by lexically resolving `.` and `..` components, without
/// touching the filesystem. Returns `None` if a `..` would pop past the root
/// of the path (i.e. escape past an absolute prefix or above a relative
/// start).
fn normalize_lexically(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut depth: i64 = 0;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                out.pop();
            }
            Component::Normal(part) => {
                depth += 1;
                out.push(part);
            }
            Component::RootDir | Component::Prefix(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    Some(out)
}

/// Canonicalizes the closest existing ancestor of `path`.
///
/// `symlink_metadata` is deliberately used before `canonicalize`: a dangling
/// symlink exists as a directory entry and must be rejected rather than being
/// mistaken for an ordinary, not-yet-created path.
fn canonical_existing_ancestor(path: &Path) -> std::io::Result<PathBuf> {
    let mut ancestor = path.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(_) => return std::fs::canonicalize(&ancestor),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if !ancestor.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// Resolves `requested` (relative or absolute) against `root`, guaranteeing
/// that its real filesystem location stays within `root`.
///
/// The lexical check rejects `..` traversal first. The canonical check then
/// follows symlinks in the target (or its nearest existing ancestor), which
/// also protects paths that do not exist yet, such as a new file below an
/// existing symlinked directory.
pub fn resolve_in_workspace(root: &Path, requested: &str) -> Result<PathBuf> {
    let requested_path = Path::new(requested);

    let joined = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        root.join(requested_path)
    };

    let normalized = normalize_lexically(&joined)
        .ok_or_else(|| ToolError::PathOutsideWorkspace(requested.to_string()))?;
    let normalized_root = normalize_lexically(root)
        .ok_or_else(|| ToolError::PathOutsideWorkspace(requested.to_string()))?;

    if !normalized.starts_with(&normalized_root) {
        return Err(ToolError::PathOutsideWorkspace(requested.to_string()));
    }

    let canonical_root = std::fs::canonicalize(&normalized_root)?;
    let canonical_ancestor = canonical_existing_ancestor(&normalized)?;
    if !canonical_ancestor.starts_with(&canonical_root) {
        return Err(ToolError::PathOutsideWorkspace(requested.to_string()));
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "kode-tools-path-{label}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn relative_path_ok() {
        let root = temp_dir("relative");
        let resolved = resolve_in_workspace(&root, "src/main.rs").unwrap();
        assert_eq!(resolved, root.join("src").join("main.rs"));
    }

    #[test]
    fn relative_traversal_rejected() {
        let root = temp_dir("traversal");
        let err = resolve_in_workspace(&root, "../../x").unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideWorkspace(_)));
    }

    #[test]
    fn absolute_inside_ok() {
        let root = temp_dir("inside");
        let inside = root.join("a").join("b.txt");
        let resolved = resolve_in_workspace(&root, inside.to_str().unwrap()).unwrap();
        assert_eq!(resolved, inside);
    }

    #[test]
    fn absolute_outside_rejected() {
        let root = temp_dir("outside-root");
        let outside = temp_dir("outside-target").join("file.txt");
        let err = resolve_in_workspace(&root, outside.to_str().unwrap()).unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideWorkspace(_)));
    }

    #[test]
    fn dot_dot_normalizes_within_root() {
        let root = temp_dir("dotdot");
        let resolved = resolve_in_workspace(&root, "a/../b").unwrap();
        assert_eq!(resolved, root.join("b"));
    }

    #[cfg(unix)]
    fn symlink_dir(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn symlink_dir(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => true,
            // Creating symlinks on Windows requires Developer Mode or an
            // elevated test process.
            Err(error) if error.raw_os_error() == Some(1314) => false,
            Err(error) => panic!("failed to create test symlink: {error}"),
        }
    }

    #[test]
    fn symlink_to_directory_inside_workspace_is_allowed() {
        let root = temp_dir("symlink-inside");
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        if !symlink_dir(&target, &root.join("link")) {
            return;
        }

        let resolved = resolve_in_workspace(&root, "link/new/file.txt").unwrap();
        assert_eq!(resolved, root.join("link/new/file.txt"));
    }

    #[test]
    fn symlink_to_directory_outside_workspace_is_rejected() {
        let root = temp_dir("symlink-outside-root");
        let outside = temp_dir("symlink-outside-target");
        if !symlink_dir(&outside, &root.join("link")) {
            return;
        }

        let err = resolve_in_workspace(&root, "link/new/file.txt").unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideWorkspace(_)));
    }

    #[test]
    fn existing_file_symlink_outside_workspace_is_rejected() {
        let root = temp_dir("file-symlink-root");
        let outside = temp_dir("file-symlink-target");
        let target = outside.join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, root.join("secret.txt")).unwrap();
        #[cfg(windows)]
        match std::os::windows::fs::symlink_file(&target, root.join("secret.txt")) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => return,
            Err(error) => panic!("failed to create test symlink: {error}"),
        }

        let err = resolve_in_workspace(&root, "secret.txt").unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideWorkspace(_)));
    }

    #[test]
    fn dangling_symlink_is_rejected() {
        let root = temp_dir("dangling-root");
        let missing = temp_dir("dangling-target").join("missing");
        if !symlink_dir(&missing, &root.join("link")) {
            return;
        }

        assert!(resolve_in_workspace(&root, "link/file.txt").is_err());
    }
}
