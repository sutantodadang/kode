/// Decision returned by [`evaluate`] for an agent-initiated memory write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Accept,
    Reject(String),
}

const HEDGE_PHRASES: &[&str] = &[
    "maybe", "probably", "i think", "might be", "possibly", "could be", "not sure", "perhaps",
];

const SECRET_INDICATORS: &[&str] = &[
    "api_key",
    "api key",
    "apikey",
    "password",
    "secret",
    "token=",
    "bearer ",
    "-----begin",
];

/// Evaluates whether an agent-initiated memory (summary + body) should be
/// stored. Applies only to [`crate::Provenance::AgentInference`] writes —
/// explicit user writes (`kode remember`) bypass this policy entirely.
pub fn evaluate(summary: &str, body: &str) -> PolicyDecision {
    let combined = format!("{summary} {body}");
    let trimmed = combined.trim();
    let lower = trimmed.to_lowercase();

    if trimmed.len() < 10 {
        return PolicyDecision::Reject("too short".to_string());
    }

    if trimmed.len() > 4000 {
        return PolicyDecision::Reject("too long for durable memory".to_string());
    }

    for phrase in HEDGE_PHRASES {
        if lower.contains(phrase) {
            return PolicyDecision::Reject(
                "uncertain language — verify before remembering".to_string(),
            );
        }
    }

    for indicator in SECRET_INDICATORS {
        if lower.contains(indicator) {
            return PolicyDecision::Reject("possible secret — never store credentials".to_string());
        }
    }

    PolicyDecision::Accept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_normal_engineering_fact() {
        assert_eq!(
            evaluate("We use cargo-nextest for all test runs", ""),
            PolicyDecision::Accept
        );
    }

    #[test]
    fn rejects_too_short() {
        match evaluate("short", "") {
            PolicyDecision::Reject(reason) => assert_eq!(reason, "too short"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn rejects_too_long() {
        let body = "a".repeat(4001);
        match evaluate("summary", &body) {
            PolicyDecision::Reject(reason) => assert_eq!(reason, "too long for durable memory"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn rejects_hedge_phrases() {
        match evaluate("I think this might be the right convention", "") {
            PolicyDecision::Reject(reason) => {
                assert_eq!(reason, "uncertain language — verify before remembering")
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn rejects_secret_indicators() {
        match evaluate("stored the api_key in .env", "") {
            PolicyDecision::Reject(reason) => {
                assert_eq!(reason, "possible secret — never store credentials")
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }
}
