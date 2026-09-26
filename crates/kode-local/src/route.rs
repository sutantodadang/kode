//! Kode's Laya routing questions and the pure step that turns model
//! probabilities into a [`RouteDecision`], falling back per question to the
//! static value when the model is unavailable or unsure.

use async_trait::async_trait;
use kode_core::event::{RouteAnswer, RouteSource};
use sha2::{Digest, Sha256};

use crate::sequence::{QType, QuestionDef};

#[derive(Debug, Clone, PartialEq)]
pub struct RouteInput {
    pub task: String,
    pub project: String,
    pub changed_files: usize,
}

impl RouteInput {
    /// The Laya `state`. Kept short: the multilingual checkpoint leaves the
    /// state ~768 tokens after the question header.
    pub fn state(&self) -> String {
        format!(
            "task: {}\nproject: {}\nuncommitted files: {}",
            self.task.trim(),
            self.project,
            self.changed_files
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteDecision {
    pub tier: String,
    /// `"config"` means: keep the configured effort.
    pub effort: String,
    pub plan: bool,
    pub answers: Vec<RouteAnswer>,
    /// Full probability vector per question the model answered.
    pub probs: Vec<(String, Vec<f32>)>,
    pub device: String,
    pub latency_ms: u64,
}

impl RouteDecision {
    pub fn answer(&self, key: &str) -> Option<&RouteAnswer> {
        self.answers.iter().find(|a| a.key == key)
    }
}

pub struct RouteQuestion {
    pub key: &'static str,
    pub def: QuestionDef,
    pub static_value: &'static str,
}

fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Order matters: `resolve_route` pairs probabilities with questions by index.
pub fn route_questions() -> Vec<RouteQuestion> {
    vec![
        RouteQuestion {
            key: "tier",
            static_value: "standard",
            def: QuestionDef {
                qtype: QType::Choice,
                instructions: "How much engineering work does this coding task need?".to_string(),
                options: opts(&[
                    (
                        "light",
                        "question, explanation, lookup, or a small single-file edit",
                    ),
                    ("standard", "a feature or bug fix touching a few files"),
                    (
                        "heavy",
                        "multi-file refactor, architecture, or unclear debugging",
                    ),
                ]),
            },
        },
        RouteQuestion {
            key: "effort",
            static_value: "config",
            def: QuestionDef {
                qtype: QType::Score,
                instructions: "How much reasoning depth does this coding task need?".to_string(),
                options: opts(&[
                    ("low", "quick answer or mechanical change"),
                    ("medium", "ordinary feature or bug fix"),
                    ("high", "subtle debugging, design, or cross-cutting change"),
                ]),
            },
        },
        RouteQuestion {
            key: "plan",
            static_value: "direct",
            def: QuestionDef {
                qtype: QType::Choice,
                instructions: "Should the agent write a plan before editing code?".to_string(),
                options: opts(&[
                    ("plan", "needs a written plan before editing"),
                    ("direct", "can be done directly"),
                ]),
            },
        },
    ]
}

/// Identifies the router questions (texts + options). Dataset records and
/// team models from another version are not comparable.
pub fn questions_version() -> String {
    version_of(&route_questions())
}

pub fn version_of(questions: &[RouteQuestion]) -> String {
    let defs: Vec<(&str, &QuestionDef)> = questions.iter().map(|q| (q.key, &q.def)).collect();
    let json = serde_json::to_string(&defs).unwrap_or_default();
    let digest = Sha256::digest(json.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// `softmax(logits / temperature)`, as laya `RLAgent.system_one` computes it.
pub fn softmax_with_temperature(logits: &[f32], temperature: f32) -> Vec<f32> {
    let t = if temperature > 0.0 { temperature } else { 1.0 };
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|z| ((z - max) / t).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.into_iter().map(|e| e / sum).collect()
}

/// `1 - normalized entropy` (laya `confidence_from_probs`).
pub fn confidence(p: &[f32]) -> f32 {
    let k = p.len();
    if k < 2 {
        return 1.0;
    }
    let h: f32 = p.iter().map(|&x| -x * x.clamp(1e-12, 1.0).ln()).sum();
    1.0 - h / (k as f32).ln()
}

fn argmax(p: &[f32]) -> usize {
    let mut best = 0;
    for (i, v) in p.iter().enumerate() {
        if *v > p[best] {
            best = i;
        }
    }
    best
}

pub fn resolve_route(
    questions: &[RouteQuestion],
    probs: Result<Vec<Vec<f32>>, String>,
    min_confidence: f32,
) -> RouteDecision {
    let mut answers = Vec::with_capacity(questions.len());
    let mut logged = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        let static_answer = |reason: String, confidence: Option<f32>| RouteAnswer {
            key: q.key.to_string(),
            value: q.static_value.to_string(),
            confidence,
            source: RouteSource::Static(reason),
        };
        let answer = match &probs {
            Err(reason) => static_answer(reason.clone(), None),
            Ok(all) => match all.get(i) {
                Some(p) if p.len() == q.def.options.len() => {
                    logged.push((q.key.to_string(), p.clone()));
                    let c = confidence(p);
                    if c >= min_confidence {
                        RouteAnswer {
                            key: q.key.to_string(),
                            value: q.def.options[argmax(p)].0.clone(),
                            confidence: Some(c),
                            source: RouteSource::Laya,
                        }
                    } else {
                        static_answer(format!("low confidence {c:.2}"), Some(c))
                    }
                }
                _ => static_answer("options did not fit".to_string(), None),
            },
        };
        answers.push(answer);
    }
    let value = |key: &str| {
        answers
            .iter()
            .find(|a| a.key == key)
            .map(|a| a.value.clone())
            .unwrap_or_default()
    };
    let tier = value("tier");
    let effort = value("effort");
    let plan = value("plan") == "plan";
    RouteDecision {
        tier,
        effort,
        plan,
        answers,
        probs: logged,
        device: String::new(),
        latency_ms: 0,
    }
}

#[async_trait]
pub trait TaskRouter: Send + Sync {
    async fn route(&self, input: &RouteInput) -> RouteDecision;
}

/// Today's behaviour, labelled with why the model was not used.
pub struct StaticRouter {
    pub reason: String,
}

#[async_trait]
impl TaskRouter for StaticRouter {
    async fn route(&self, _input: &RouteInput) -> RouteDecision {
        let mut d = resolve_route(&route_questions(), Err(self.reason.clone()), 1.0);
        d.device = "none".to_string();
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn onehot(k: usize, idx: usize) -> Vec<f32> {
        (0..k).map(|i| if i == idx { 1.0 } else { 0.0 }).collect()
    }

    #[test]
    fn confident_answers_come_from_laya() {
        let qs = route_questions();
        let d = resolve_route(&qs, Ok(vec![onehot(3, 2), onehot(3, 0), onehot(2, 0)]), 0.6);
        assert_eq!(d.tier, "heavy");
        assert_eq!(d.effort, "low");
        assert!(d.plan);
        assert!(d.answers.iter().all(|a| a.source == RouteSource::Laya));
        assert_eq!(d.probs.len(), 3);
    }

    #[test]
    fn low_confidence_question_falls_back_alone() {
        let qs = route_questions();
        let uniform = vec![1.0 / 3.0; 3];
        let d = resolve_route(&qs, Ok(vec![uniform, onehot(3, 2), onehot(2, 1)]), 0.6);
        assert_eq!(d.tier, "standard");
        let tier = d.answer("tier").unwrap();
        // Uniform confidence is ~0 but may format as "-0.00" in f32.
        assert!(matches!(&tier.source, RouteSource::Static(r) if r.starts_with("low confidence")));
        assert_eq!(d.effort, "high");
        assert!(!d.plan);
    }

    #[test]
    fn error_makes_every_answer_static_with_the_reason() {
        let d = resolve_route(&route_questions(), Err("cancelled".to_string()), 0.6);
        assert_eq!(d.tier, "standard");
        assert_eq!(d.effort, "config");
        assert!(!d.plan);
        assert!(
            d.answers
                .iter()
                .all(|a| a.source == RouteSource::Static("cancelled".to_string())
                    && a.confidence.is_none())
        );
        assert!(d.probs.is_empty());
    }

    #[test]
    fn wrong_probability_length_is_static() {
        let d = resolve_route(&route_questions(), Ok(vec![vec![1.0, 0.0]]), 0.6);
        let tier = d.answer("tier").unwrap();
        assert_eq!(
            tier.source,
            RouteSource::Static("options did not fit".to_string())
        );
    }

    #[test]
    fn confidence_is_one_minus_normalized_entropy() {
        assert!((confidence(&[1.0, 0.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(confidence(&[1.0 / 3.0; 3]).abs() < 1e-6);
        assert!((confidence(&[1.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn softmax_applies_temperature() {
        let p = softmax_with_temperature(&[0.0, 2.0f32.ln()], 1.0);
        assert!((p[0] - 1.0 / 3.0).abs() < 1e-6);
        assert!((p[1] - 2.0 / 3.0).abs() < 1e-6);
        let flat = softmax_with_temperature(&[0.0, 100.0], 1e9);
        assert!((flat[0] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn state_is_trimmed_and_labelled() {
        let input = RouteInput {
            task: "  fix the bug \n".to_string(),
            project: "rust".to_string(),
            changed_files: 2,
        };
        assert_eq!(
            input.state(),
            "task: fix the bug\nproject: rust\nuncommitted files: 2"
        );
    }

    #[test]
    fn questions_version_is_stable_and_prefixed() {
        let v = questions_version();
        assert_eq!(v, questions_version());
        assert!(v.starts_with("sha256:"));
        assert_eq!(v.len(), "sha256:".len() + 64);
    }

    #[test]
    fn questions_version_changes_when_a_question_changes() {
        let mut qs = route_questions();
        let before = version_of(&qs);
        qs[0].def.instructions.push_str(" (edited)");
        assert_ne!(before, version_of(&qs));
    }

    #[tokio::test]
    async fn static_router_reports_its_reason() {
        let d = StaticRouter {
            reason: "disabled".to_string(),
        }
        .route(&RouteInput {
            task: String::new(),
            project: "unknown".to_string(),
            changed_files: 0,
        })
        .await;
        assert_eq!(d.device, "none");
        assert!(
            d.answers
                .iter()
                .all(|a| a.source == RouteSource::Static("disabled".to_string()))
        );
    }
}
