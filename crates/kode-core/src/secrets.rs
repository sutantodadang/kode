//! One "does this look like a credential" check for everything Kode
//! persists from user or agent text (team memories, router training data).

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

pub fn looks_like_secret(text: &str) -> bool {
    let lower = text.to_lowercase();
    SECRET_INDICATORS.iter().any(|i| lower.contains(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_credential_looking_text_case_insensitively() {
        assert!(looks_like_secret("set OPENAI API_KEY to sk-123"));
        assert!(looks_like_secret("the Password is hunter2"));
        assert!(looks_like_secret("curl -H 'Authorization: Bearer abc'"));
        assert!(looks_like_secret("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(looks_like_secret("url?token=abc"));
    }

    #[test]
    fn ordinary_engineering_text_passes() {
        assert!(!looks_like_secret(
            "refactor the token budget in the context compiler"
        ));
        assert!(!looks_like_secret("tolong jelaskan fungsi run_plan_phase"));
    }
}
