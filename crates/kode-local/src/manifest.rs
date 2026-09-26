//! `.kode/router/team-model.json`: which router model a team uses — a
//! calibration of the pinned model, or its own fine-tuned checkpoint from
//! an HF repo, a shared path, or a Git LFS path in the repo.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::LocalError;
use crate::models::{LocalPaths, verify_file};
use crate::temps::Temperatures;

pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(".kode").join("router").join("team-model.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestKind {
    Calibration,
    Checkpoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Hf {
        repo: String,
        revision: String,
    },
    Path(String),
    /// Relative to the repo root; typically tracked with Git LFS.
    RepoPath(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamModel {
    pub kind: ManifestKind,
    /// Pinned model revision a calibration applies to.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_revision: String,
    pub questions_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileEntry>,
    #[serde(flatten)]
    pub temps: Temperatures,
    #[serde(default)]
    pub report: serde_json::Value,
}

pub fn read_manifest(root: &Path) -> Result<Option<TeamModel>, String> {
    let path = manifest_path(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn write_manifest(root: &Path, manifest: &TeamModel) -> std::io::Result<()> {
    let path = manifest_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(manifest).map_err(std::io::Error::other)?;
    std::fs::write(path, json + "\n")
}

pub fn checkpoint_dir(source: &Source, root: &Path, paths: &LocalPaths) -> PathBuf {
    match source {
        Source::Hf { repo, revision } => paths
            .root
            .join("models")
            .join("team")
            .join(repo.replace('/', "__"))
            .join(revision),
        Source::Path(p) => PathBuf::from(p),
        Source::RepoPath(p) => root.join(p),
    }
}

pub fn verify_checkpoint(dir: &Path, files: &[FileEntry]) -> Result<(), LocalError> {
    for f in files {
        verify_file(&dir.join(&f.path), &f.sha256, f.size)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelChoice {
    Team {
        dir: PathBuf,
        temps: Temperatures,
        label: String,
    },
    Pinned {
        temps: Option<Temperatures>,
        label: &'static str,
    },
}

fn label_of(source: &Source) -> String {
    match source {
        Source::Hf { revision, .. } => format!("team@{}", &revision[..revision.len().min(7)]),
        Source::Path(_) => "team@path".to_string(),
        Source::RepoPath(_) => "team@lfs".to_string(),
    }
}

/// Loading precedence: a verified team checkpoint, else the pinned model
/// with a team calibration, else the pinned model as shipped. A manifest
/// that exists but cannot be used yields a note explaining the fallback.
pub fn choose(
    root: &Path,
    paths: &LocalPaths,
    pinned_revision: &str,
    questions_version: &str,
) -> (ModelChoice, Option<String>) {
    let plain = || ModelChoice::Pinned {
        temps: None,
        label: "pinned",
    };
    let fallback = |why: String| (plain(), Some(format!("router: {why} — using pinned")));
    let manifest = match read_manifest(root) {
        Ok(None) => return (plain(), None),
        Ok(Some(m)) => m,
        Err(e) => return fallback(format!("team model manifest unreadable ({e})")),
    };
    if manifest.questions_version != questions_version {
        return fallback("team model was made for other router questions".to_string());
    }
    match manifest.kind {
        ManifestKind::Calibration => {
            if manifest.base_revision == pinned_revision {
                (
                    ModelChoice::Pinned {
                        temps: Some(manifest.temps),
                        label: "pinned+cal",
                    },
                    None,
                )
            } else {
                fallback("team calibration is for another pinned model".to_string())
            }
        }
        ManifestKind::Checkpoint => {
            let Some(source) = manifest.source.as_ref() else {
                return fallback("team checkpoint manifest has no source".to_string());
            };
            if manifest.files.is_empty() {
                return fallback("team checkpoint manifest lists no files".to_string());
            }
            let dir = checkpoint_dir(source, root, paths);
            match verify_checkpoint(&dir, &manifest.files) {
                Ok(()) => (
                    ModelChoice::Team {
                        dir,
                        temps: manifest.temps.clone(),
                        label: label_of(source),
                    },
                    None,
                ),
                Err(LocalError::Missing(p)) => {
                    let hint = if matches!(source, Source::Hf { .. }) {
                        "; run `kode setup`"
                    } else {
                        ""
                    };
                    fallback(format!(
                        "team model unavailable ({} missing{hint})",
                        p.display()
                    ))
                }
                Err(LocalError::ChecksumMismatch(p)) => {
                    let hint = if matches!(source, Source::RepoPath(_)) {
                        "; run `git lfs pull`"
                    } else {
                        ""
                    };
                    fallback(format!(
                        "team model unavailable (checksum mismatch: {}{hint})",
                        p.display()
                    ))
                }
                Err(e) => fallback(format!("team model unavailable ({e})")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const V: &str = "sha256:q";
    const PINNED: &str = "06065571de7cbd6d2ddcbb3f938e5bd5457fc4a6";

    fn temp(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("kode-manifest-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cal_temps() -> Temperatures {
        Temperatures {
            temperature: vec![1.3, 0.9, 1.0],
            temperature_by_options: Default::default(),
        }
    }

    fn checkpoint(source: Source) -> TeamModel {
        TeamModel {
            kind: ManifestKind::Checkpoint,
            base_revision: String::new(),
            questions_version: V.to_string(),
            source: Some(source),
            files: vec![FileEntry {
                path: "model.onnx".to_string(),
                sha256: HELLO_SHA.to_string(),
                size: 5,
            }],
            temps: cal_temps(),
            report: serde_json::Value::Null,
        }
    }

    fn setup(label: &str) -> (PathBuf, LocalPaths) {
        let root = temp(label);
        let home = temp(&format!("{label}-home"));
        (root, LocalPaths { root: home })
    }

    #[test]
    fn no_manifest_means_pinned_without_note() {
        let (root, paths) = setup("none");
        let (choice, note) = choose(&root, &paths, PINNED, V);
        assert!(matches!(
            choice,
            ModelChoice::Pinned {
                temps: None,
                label: "pinned"
            }
        ));
        assert!(note.is_none());
    }

    #[test]
    fn calibration_for_pinned_applies_temperatures() {
        let (root, paths) = setup("cal");
        write_manifest(
            &root,
            &TeamModel {
                kind: ManifestKind::Calibration,
                base_revision: PINNED.to_string(),
                questions_version: V.to_string(),
                source: None,
                files: vec![],
                temps: cal_temps(),
                report: serde_json::Value::Null,
            },
        )
        .unwrap();
        let (choice, note) = choose(&root, &paths, PINNED, V);
        match choice {
            ModelChoice::Pinned {
                temps: Some(t),
                label,
            } => {
                assert_eq!(t, cal_temps());
                assert_eq!(label, "pinned+cal");
            }
            _ => panic!("expected calibrated pinned"),
        }
        assert!(note.is_none());
        let (_, note) = choose(&root, &paths, "otherrevision", V);
        assert!(note.unwrap().contains("another pinned model"));
    }

    #[test]
    fn choose_rejects_other_questions_version() {
        let (root, paths) = setup("version");
        write_manifest(&root, &checkpoint(Source::Path("x".to_string()))).unwrap();
        let (choice, note) = choose(&root, &paths, PINNED, "sha256:new");
        assert!(matches!(choice, ModelChoice::Pinned { temps: None, .. }));
        assert!(note.unwrap().contains("other router questions"));
    }

    #[test]
    fn verified_path_checkpoint_is_chosen() {
        let (root, paths) = setup("path");
        let dir = temp("path-model");
        std::fs::write(dir.join("model.onnx"), b"hello").unwrap();
        write_manifest(
            &root,
            &checkpoint(Source::Path(dir.to_string_lossy().to_string())),
        )
        .unwrap();
        let (choice, note) = choose(&root, &paths, PINNED, V);
        match choice {
            ModelChoice::Team {
                dir: d,
                temps,
                label,
            } => {
                assert_eq!(d, dir);
                assert_eq!(temps, cal_temps());
                assert_eq!(label, "team@path");
            }
            _ => panic!("expected team model, note: {note:?}"),
        }
    }

    #[test]
    fn missing_hf_checkpoint_says_run_setup() {
        let (root, paths) = setup("hf");
        write_manifest(
            &root,
            &checkpoint(Source::Hf {
                repo: "team/router".to_string(),
                revision: "abcdef123456".to_string(),
            }),
        )
        .unwrap();
        let (choice, note) = choose(&root, &paths, PINNED, V);
        assert!(matches!(choice, ModelChoice::Pinned { .. }));
        assert!(note.unwrap().contains("kode setup"));
        assert_eq!(
            checkpoint_dir(
                &Source::Hf {
                    repo: "team/router".to_string(),
                    revision: "abc".to_string()
                },
                &root,
                &paths
            ),
            paths
                .root
                .join("models")
                .join("team")
                .join("team__router")
                .join("abc")
        );
    }

    #[test]
    fn choose_hints_lfs_pull_on_checksum_mismatch() {
        let (root, paths) = setup("lfs");
        std::fs::create_dir_all(root.join("models/router")).unwrap();
        // An un-pulled LFS pointer: same size is unlikely, so any mismatch triggers.
        std::fs::write(root.join("models/router/model.onnx"), b"hellp").unwrap();
        write_manifest(
            &root,
            &checkpoint(Source::RepoPath("models/router".to_string())),
        )
        .unwrap();
        let (_, note) = choose(&root, &paths, PINNED, V);
        assert!(note.unwrap().contains("git lfs pull"));
    }

    #[test]
    fn unreadable_manifest_falls_back_with_note() {
        let (root, paths) = setup("bad");
        std::fs::create_dir_all(root.join(".kode/router")).unwrap();
        std::fs::write(manifest_path(&root), "{not json").unwrap();
        let (choice, note) = choose(&root, &paths, PINNED, V);
        assert!(matches!(choice, ModelChoice::Pinned { .. }));
        assert!(note.unwrap().contains("unreadable"));
    }

    #[test]
    fn manifest_serializes_with_kind_and_external_source_tag() {
        let (root, _) = setup("serde");
        write_manifest(
            &root,
            &checkpoint(Source::Hf {
                repo: "t/r".to_string(),
                revision: "abc".to_string(),
            }),
        )
        .unwrap();
        let text = std::fs::read_to_string(manifest_path(&root)).unwrap();
        assert!(text.contains(r#""kind": "checkpoint""#));
        assert!(text.contains(r#""hf": {"#));
        assert!(text.contains(r#""temperature": ["#));
        assert_eq!(
            read_manifest(&root).unwrap().unwrap(),
            checkpoint(Source::Hf {
                repo: "t/r".to_string(),
                revision: "abc".to_string()
            })
        );
    }
}
