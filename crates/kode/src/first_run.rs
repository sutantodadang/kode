//! First-run decisions: which setup card to show next, and the per-repo
//! "index this repo?" answer, stored outside the repository.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexPrompt {
    Accepted,
    Declined,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RepoState {
    #[serde(default)]
    index_prompt: Option<IndexPrompt>,
}

/// `home/state/<first 16 hex of sha256(canonical repo path)>.json`.
pub fn state_path(home: &Path, repo: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    home.join("state").join(format!("{hex}.json"))
}

pub fn load_index_prompt(home: &Path, repo: &Path) -> Option<IndexPrompt> {
    let text = std::fs::read_to_string(state_path(home, repo)).ok()?;
    serde_json::from_str::<RepoState>(&text).ok()?.index_prompt
}

pub fn save_index_prompt(home: &Path, repo: &Path, prompt: IndexPrompt) -> std::io::Result<()> {
    let path = state_path(home, repo);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let state = RepoState {
        index_prompt: Some(prompt),
    };
    std::fs::write(path, serde_json::to_string_pretty(&state)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SetupCard {
    Provider,
    Login,
    Engine,
    Index,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupFacts {
    pub model_configured: bool,
    pub provider_logged_in: bool,
    pub zindeks_enabled: bool,
    pub engine_installed: bool,
    /// `None` until the startup probe answers; no Index card before that.
    pub repo_indexed: Option<bool>,
    pub index_prompt: Option<IndexPrompt>,
}

/// The first card that applies and was not skipped this launch.
pub fn next_card(facts: &SetupFacts, skipped: &HashSet<SetupCard>) -> Option<SetupCard> {
    let applies = |card: SetupCard| match card {
        SetupCard::Provider => !facts.model_configured,
        SetupCard::Login => facts.model_configured && !facts.provider_logged_in,
        SetupCard::Engine => facts.zindeks_enabled && !facts.engine_installed,
        SetupCard::Index => {
            facts.zindeks_enabled
                && facts.engine_installed
                && facts.repo_indexed == Some(false)
                && facts.index_prompt.is_none()
        }
    };
    [
        SetupCard::Provider,
        SetupCard::Login,
        SetupCard::Engine,
        SetupCard::Index,
    ]
    .into_iter()
    .find(|card| !skipped.contains(card) && applies(*card))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> SetupFacts {
        SetupFacts {
            model_configured: true,
            provider_logged_in: true,
            zindeks_enabled: true,
            engine_installed: true,
            repo_indexed: Some(true),
            index_prompt: None,
        }
    }

    #[test]
    fn fully_ready_shows_no_card() {
        assert_eq!(next_card(&ready(), &HashSet::new()), None);
    }

    #[test]
    fn cards_come_in_order_and_skips_move_on() {
        let facts = SetupFacts {
            model_configured: false,
            provider_logged_in: false,
            engine_installed: false,
            repo_indexed: Some(false),
            ..ready()
        };
        let mut skipped = HashSet::new();
        assert_eq!(next_card(&facts, &skipped), Some(SetupCard::Provider));
        skipped.insert(SetupCard::Provider);
        // Login needs a chosen model/provider first, so it is not offered.
        assert_eq!(next_card(&facts, &skipped), Some(SetupCard::Engine));
        skipped.insert(SetupCard::Engine);
        // Index needs the engine.
        assert_eq!(next_card(&facts, &skipped), None);
    }

    #[test]
    fn login_card_when_model_set_but_not_logged_in() {
        let facts = SetupFacts {
            provider_logged_in: false,
            ..ready()
        };
        assert_eq!(next_card(&facts, &HashSet::new()), Some(SetupCard::Login));
    }

    #[test]
    fn index_card_only_when_known_unindexed_and_undecided() {
        let unknown = SetupFacts {
            repo_indexed: None,
            ..ready()
        };
        assert_eq!(next_card(&unknown, &HashSet::new()), None);
        let unindexed = SetupFacts {
            repo_indexed: Some(false),
            ..ready()
        };
        assert_eq!(
            next_card(&unindexed, &HashSet::new()),
            Some(SetupCard::Index)
        );
        let declined = SetupFacts {
            index_prompt: Some(IndexPrompt::Declined),
            ..unindexed
        };
        assert_eq!(next_card(&declined, &HashSet::new()), None);
    }

    #[test]
    fn zindeks_disabled_skips_engine_and_index() {
        let facts = SetupFacts {
            zindeks_enabled: false,
            engine_installed: false,
            repo_indexed: Some(false),
            ..ready()
        };
        assert_eq!(next_card(&facts, &HashSet::new()), None);
    }

    #[test]
    fn index_prompt_round_trips_outside_the_repo() {
        let base = std::env::temp_dir().join(format!(
            "kode-first-run-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("home");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        assert_eq!(load_index_prompt(&home, &repo), None);
        save_index_prompt(&home, &repo, IndexPrompt::Declined).unwrap();
        assert_eq!(load_index_prompt(&home, &repo), Some(IndexPrompt::Declined));
        assert!(state_path(&home, &repo).starts_with(home.join("state")));
        assert!(!repo.join(".kode").exists());
    }
}
