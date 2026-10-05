//! The hindsight teacher: a prompt asking the task's own model what the
//! router should have decided, and a strict parser for its JSON answer.

use std::collections::BTreeMap;

use crate::dataset::TaskSummary;
use crate::route::route_questions;
use crate::sequence::QType;

const SUM_TOLERANCE: f32 = 0.05;

pub fn teacher_prompt(state: &str, s: &TaskSummary) -> String {
    let mut out = String::new();
    out.push_str(
        "You are labeling, with hindsight, the routing decision a coding agent made before starting a task.\n\n",
    );
    out.push_str("The agent saw:\n<state>\n");
    out.push_str(state);
    out.push_str("\n</state>\n\n");
    out.push_str(&format!(
        "What actually happened: {} model iterations, {} tool calls, {} files changed, mutated: {}, verification: {}, repair attempted: {}.\n\n",
        s.iterations, s.tool_calls, s.files_changed, s.mutated, s.verification, s.repair_attempted
    ));
    out.push_str(
        "For each question, give a probability for every option: what would have been the best choice for this task, knowing what happened. Each question's probabilities must sum to 1.\n\n",
    );
    let mut example = Vec::new();
    for q in route_questions() {
        out.push_str(&format!("{} — {}\n", q.key, q.def.instructions));
        for (i, (key, text)) in q.def.options.iter().enumerate() {
            if q.def.qtype == QType::Score {
                out.push_str(&format!("  \"{key}\" (level {i}): {text}\n"));
            } else {
                out.push_str(&format!("  \"{key}\": {text}\n"));
            }
        }
        out.push('\n');
        let fields: Vec<String> = q
            .def
            .options
            .iter()
            .map(|(k, _)| format!("\"{k}\": p"))
            .collect();
        example.push(format!("\"{}\": {{{}}}", q.key, fields.join(", ")));
    }
    out.push_str("Answer with only this JSON object (p = a number between 0 and 1), no prose:\n");
    out.push_str(&format!("{{{}}}\n", example.join(", ")));
    out
}

/// Question key → probabilities in option order, renormalised to sum to 1.
pub fn parse_teacher(text: &str) -> Result<BTreeMap<String, Vec<f32>>, String> {
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Err("teacher answer has no JSON object".to_string());
    };
    if end < start {
        return Err("teacher answer has no JSON object".to_string());
    }
    let value: serde_json::Value = serde_json::from_str(&text[start..=end])
        .map_err(|e| format!("teacher JSON invalid: {e}"))?;
    let mut out = BTreeMap::new();
    for q in route_questions() {
        let obj = value
            .get(q.key)
            .and_then(|v| v.as_object())
            .ok_or_else(|| format!("teacher answer missing `{}`", q.key))?;
        let mut probs = Vec::with_capacity(q.def.options.len());
        for (key, _) in &q.def.options {
            let p = obj
                .get(key)
                .and_then(|v| v.as_f64())
                .ok_or_else(|| format!("teacher answer missing `{}.{key}`", q.key))?;
            if !(0.0..=1.0).contains(&p) {
                return Err(format!(
                    "teacher probability `{}.{key}` = {p} is outside [0, 1]",
                    q.key
                ));
            }
            probs.push(p as f32);
        }
        let sum: f32 = probs.iter().sum();
        if (sum - 1.0).abs() > SUM_TOLERANCE {
            return Err(format!(
                "teacher probabilities for `{}` sum to {sum:.2}",
                q.key
            ));
        }
        out.insert(q.key.to_string(), probs.iter().map(|p| p / sum).collect());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{"tier":{"light":0.1,"standard":0.3,"heavy":0.6},
        "effort":{"low":0.2,"medium":0.5,"high":0.3},
        "plan":{"plan":0.7,"direct":0.3},
        "answer":{"graph":0.3,"model":0.7},
        "graph_query":{"definition":0.4,"callers":0.2,"callees":0.2,"impact":0.1,"structure":0.1}}"#;

    fn summary() -> TaskSummary {
        TaskSummary {
            iterations: 4,
            tool_calls: 12,
            files_changed: 3,
            mutated: true,
            verification: "verified".to_string(),
            repair_attempted: true,
        }
    }

    #[test]
    fn prompt_carries_state_outcome_and_every_option() {
        let p = teacher_prompt("task: refactor providers", &summary());
        assert!(p.contains("task: refactor providers"));
        assert!(p.contains("12 tool calls"));
        assert!(p.contains("repair attempted: true"));
        for key in [
            "light",
            "standard",
            "heavy",
            "low",
            "medium",
            "high",
            "plan",
            "direct",
            "graph",
            "model",
            "definition",
            "callers",
            "callees",
            "impact",
            "structure",
        ] {
            assert!(p.contains(&format!("\"{key}\"")), "missing {key}");
        }
    }

    #[test]
    fn parses_valid_json_in_option_order() {
        let m = parse_teacher(VALID).unwrap();
        assert_eq!(m["tier"], vec![0.1, 0.3, 0.6]);
        assert_eq!(m["plan"], vec![0.7, 0.3]);
        assert_eq!(m["graph_query"], vec![0.4, 0.2, 0.2, 0.1, 0.1]);
    }

    #[test]
    fn parse_accepts_fenced_json_with_prose() {
        let text = format!("Here you go:\n```json\n{VALID}\n```\nHope that helps.");
        assert!(parse_teacher(&text).is_ok());
    }

    #[test]
    fn rejects_missing_question_and_option() {
        assert!(
            parse_teacher(r#"{"tier":{"light":1,"standard":0,"heavy":0}}"#)
                .unwrap_err()
                .contains("`effort`")
        );
        let missing = VALID.replace(r#""heavy":0.6"#, r#""huge":0.6"#);
        assert!(parse_teacher(&missing).unwrap_err().contains("tier.heavy"));
    }

    #[test]
    fn rejects_out_of_range_and_bad_sums() {
        let neg = VALID.replace(r#""light":0.1"#, r#""light":-0.1"#);
        assert!(parse_teacher(&neg).unwrap_err().contains("outside"));
        let big = VALID.replace(r#""heavy":0.6"#, r#""heavy":0.9"#);
        assert!(parse_teacher(&big).unwrap_err().contains("sum to"));
        assert!(parse_teacher("no json here").is_err());
    }

    #[test]
    fn renormalises_small_rounding() {
        let near = VALID.replace(r#""heavy":0.6"#, r#""heavy":0.58"#);
        let m = parse_teacher(&near).unwrap();
        let sum: f32 = m["tier"].iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
    }
}
