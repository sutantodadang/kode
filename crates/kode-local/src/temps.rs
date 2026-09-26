//! Softmax temperatures per question type and per option-count bucket
//! (laya `temp_bucket`), shared by `laya.json`, the team manifest, and
//! calibration.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::sequence::QType;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Temperatures {
    /// Indexed by `QType::index()`: choice, score, noul.
    pub temperature: Vec<f32>,
    /// `"choice:3-5"`-style bucket overrides.
    #[serde(default)]
    pub temperature_by_options: BTreeMap<String, f32>,
}

impl Default for Temperatures {
    fn default() -> Self {
        Self {
            temperature: vec![1.0; 3],
            temperature_by_options: BTreeMap::new(),
        }
    }
}

/// laya `temp_bucket(qtype, k)`.
pub fn bucket(qtype: QType, k: usize) -> String {
    let size = if k <= 2 {
        "2"
    } else if k <= 5 {
        "3-5"
    } else if k <= 10 {
        "6-10"
    } else {
        "11+"
    };
    format!("{}:{size}", qtype.name())
}

impl Temperatures {
    pub fn get(&self, qtype: QType, k: usize) -> f32 {
        self.temperature_by_options
            .get(&bucket(qtype, k))
            .copied()
            .unwrap_or_else(|| self.temperature.get(qtype.index()).copied().unwrap_or(1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequence::QType;

    #[test]
    fn bucket_names_match_laya_temp_bucket() {
        assert_eq!(bucket(QType::Choice, 2), "choice:2");
        assert_eq!(bucket(QType::Choice, 3), "choice:3-5");
        assert_eq!(bucket(QType::Score, 5), "score:3-5");
        assert_eq!(bucket(QType::Choice, 7), "choice:6-10");
        assert_eq!(bucket(QType::Noul, 14), "noul:11+");
    }

    #[test]
    fn get_prefers_bucket_then_qtype_then_one() {
        let t = Temperatures {
            temperature: vec![1.5, 2.0, 3.0],
            temperature_by_options: [("choice:3-5".to_string(), 0.7)].into_iter().collect(),
        };
        assert!((t.get(QType::Choice, 3) - 0.7).abs() < 1e-6);
        assert!((t.get(QType::Choice, 2) - 1.5).abs() < 1e-6);
        assert!((t.get(QType::Score, 3) - 2.0).abs() < 1e-6);
        let short = Temperatures {
            temperature: vec![],
            temperature_by_options: Default::default(),
        };
        assert!((short.get(QType::Noul, 2) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn serde_names_match_laya_json() {
        let t: Temperatures = serde_json::from_str(
            r#"{"temperature":[1.0,2.0,3.0],"temperature_by_options":{"choice:2":0.5}}"#,
        )
        .unwrap();
        assert_eq!(t.temperature, vec![1.0, 2.0, 3.0]);
        assert_eq!(t.temperature_by_options.get("choice:2"), Some(&0.5));
    }
}
