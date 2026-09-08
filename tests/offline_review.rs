use barry_dylan::github::pr::ChangedFile;
use barry_dylan::offline;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The model returns the same JSON review for every call — persona drafts and
/// the synthesis pass alike. We assert the synthesis result surfaces intact.
fn chat_completion_body(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
    })
}

#[tokio::test]
async fn produces_a_review_from_a_changed_file_set() {
    let server = MockServer::start().await;

    let review_json = r#"{"outcome":"request_changes",
        "summary":"unwrap can panic",
        "findings":[{"file":"src/a.rs","line":3,"message":"unwrap on None"}]}"#;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_completion_body(review_json)))
        .mount(&server)
        .await;

    let cfg_text = format!(
        r#"
[llm]
provider = "openai"
endpoint = "{}/v1"
model = "test-model"
max_tokens = 2048
request_timeout_secs = 10
"#,
        server.uri()
    );
    let cfg: offline::config::OfflineConfig = toml::from_str(&cfg_text).unwrap();

    let files = vec![ChangedFile {
        filename: "src/a.rs".into(),
        status: "modified".into(),
        additions: 1,
        deletions: 0,
        changes: 1,
        patch: Some("@@ -1,2 +1,3 @@\n fn a() {\n+    x.unwrap();\n }".into()),
    }];

    let review = offline::run(&cfg, &files).await.unwrap();

    assert_eq!(review.summary, "unwrap can panic");
    assert!(!review.findings.is_empty());
}

#[tokio::test]
async fn rejects_an_empty_changed_file_set() {
    let cfg: offline::config::OfflineConfig = toml::from_str(
        r#"
[llm]
provider = "openai"
endpoint = "http://127.0.0.1:1/v1"
model = "test-model"
"#,
    )
    .unwrap();

    let err = offline::run(&cfg, &[]).await.unwrap_err();
    assert!(err.to_string().contains("no changed files"));
}
