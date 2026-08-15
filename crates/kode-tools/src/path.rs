use std::path::{Component, Path, PathBuf};

use crate::error::{Result, ToolError};

// ponytail: lexical only, symlinks can escape; upgrade to canonicalize-ancestor check if needed.

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

/// Resolves `requested` (relative or absolute) against `root`, guaranteeing
/// the result stays lexically within `root`. `root` itself must already be
/// absolute and normalized (it is not re-normalized here beyond joining).
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

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from(r"C:\workspace\root")
    }

    #[test]
    fn relative_path_ok() {
        let resolved = resolve_in_workspace(&root(), "src/main.rs").unwrap();
        assert_eq!(resolved, root().join("src").join("main.rs"));
    }

    #[test]
    fn relative_traversal_rejected() {
        let err = resolve_in_workspace(&root(), r"..\..\x").unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideWorkspace(_)));
    }

    #[test]
    fn absolute_inside_ok() {
        let inside = root().join("a").join("b.txt");
        let resolved = resolve_in_workspace(&root(), inside.to_str().unwrap()).unwrap();
        assert_eq!(resolved, inside);
    }

    #[test]
    fn absolute_outside_rejected() {
        let outside = r"C:\elsewhere\file.txt";
        let err = resolve_in_workspace(&root(), outside).unwrap_err();
        assert!(matches!(err, ToolError::PathOutsideWorkspace(_)));
    }

    #[test]
    fn dot_dot_normalizes_within_root() {
        let resolved = resolve_in_workspace(&root(), r"a\..\b").unwrap();
        assert_eq!(resolved, root().join("b"));
    }
}
