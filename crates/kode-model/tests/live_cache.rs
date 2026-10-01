//! Opt-in checks against real providers. Not run by default.
//!
//! Each test sends the same request twice and requires the second one to
//! report a cache read. Set the model id for the provider you want to check,
//! then run:
//!
//! `cargo test -p kode-model --test live_cache -- --ignored --nocapture`

use kode_model::{Message, Model, ModelRequest, collect_response};

fn request(key: &str) -> ModelRequest {
    ModelRequest {
        messages: vec![
            // About 12k tokens: above every provider's minimum cacheable size.
            Message::System(format!(
                "You are a terse assistant. Reference text follows.\n{}",
                "The quick brown fox jumps over the lazy dog. ".repeat(1200)
            )),
            Message::User("Reply with the single word OK.".to_string()),
        ],
        max_tokens: Some(512),
        cache_key: Some(key.to_string()),
        ..Default::default()
    }
}

async fn second_call_reads_cache(model: &dyn Model) {
    let key = format!("kode-live-{}", std::process::id());
    let mut usages = Vec::new();
    for _ in 0..2 {
        let stream = model.stream(request(&key)).await.expect("request accepted");
        let response = collect_response(stream).await.expect("stream completes");
        usages.push(response.usage);
    }
    eprintln!("first: {:?}\nsecond: {:?}", usages[0], usages[1]);
    let cached = usages[1].and_then(|usage| usage.cache_read_tokens);
    assert!(
        cached.is_some_and(|tokens| tokens > 0),
        "second call reported no cache read: {cached:?}"
    );
}

fn model_id(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("set {var} to a model id to run this test"))
}

#[tokio::test]
#[ignore = "live: needs `kode auth login anthropic` and KODE_LIVE_ANTHROPIC_MODEL"]
async fn anthropic_second_call_reads_cache() {
    let auth = kode_model::anthropic::default_auth_path().expect("home directory");
    let model = kode_model::AnthropicModel::new(auth, model_id("KODE_LIVE_ANTHROPIC_MODEL"))
        .expect("anthropic auth");
    second_call_reads_cache(&model).await;
}

#[tokio::test]
#[ignore = "live: needs `kode auth login codex` and KODE_LIVE_CODEX_MODEL"]
async fn codex_second_call_reads_cache() {
    let auth = kode_model::codex::default_auth_path().expect("home directory");
    let model =
        kode_model::CodexModel::new(auth, model_id("KODE_LIVE_CODEX_MODEL")).expect("codex auth");
    second_call_reads_cache(&model).await;
}

#[tokio::test]
#[ignore = "live: needs OPENAI_API_KEY and KODE_LIVE_OPENAI_MODEL"]
async fn openai_second_call_reads_cache() {
    let model = kode_model::OpenAiModel::new(kode_model::OpenAiOptions {
        api_key: std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY"),
        model: model_id("KODE_LIVE_OPENAI_MODEL"),
        ..Default::default()
    });
    second_call_reads_cache(&model).await;
}

#[tokio::test]
#[ignore = "live: needs an opencode-go key and KODE_LIVE_OPENCODE_GO_MODEL"]
async fn opencode_go_second_call_reads_cache() {
    let auth = kode_model::opencode::default_auth_path().expect("home directory");
    let model = kode_model::opencode::resolve(
        "opencode-go",
        model_id("KODE_LIVE_OPENCODE_GO_MODEL"),
        &auth,
        None,
    )
    .expect("opencode-go auth");
    second_call_reads_cache(&model).await;
}
