use barry_dylan::dispatcher::run::run_job;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Helper: return an OpenAI-shaped chat completion response.
fn chat_resp(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "created": 0,
        "model": "test",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": content }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
}

#[tokio::test]
async fn agreement_posts_only_barry_review() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
                1,
                "alice",
                "sha1",
                None,
                serde_json::json!([]),
                serde_json::json!([]),
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "filename": "src/lib.rs",
                "status": "modified",
                "additions": 1,
                "deletions": 0,
                "changes": 1,
                "patch": "@@ -1 +1 @@\n+x"
            }
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "write"
        })))
        .mount(&server)
        .await;

    // Judge call: identified by the distinctive "Barry's review" substring in its body.
    // Mount this first (higher priority) so it matches before the fallback approve mock.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("Barry's review"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_resp(r#"{"agree":true,"reason":"same outcome"}"#)),
        )
        .mount(&server)
        .await;

    // Fallback: all persona + synthesis calls return approve.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_resp(
            r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#,
        )))
        .mount(&server)
        .await;

    // 4 hygiene check-runs + 1 multi-review check-run = 5 minimum.
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(5..)
        .mount(&server)
        .await;

    // On agreement, only Barry posts a review (exactly 1).
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/1/reviews"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;

    let (store, deps) = crate::common::fixture_with_llm(&server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
    // wiremock verifies expectations on Drop.
}
