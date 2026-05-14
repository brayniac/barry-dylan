use barry_dylan::dispatcher::run::run_job;
use barry_dylan::storage::multi_review::RunKey;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Return an OpenAI-shaped chat completion with the given content.
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
async fn disagreement_posts_two_reviews_and_neutral_check() {
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

    // Judge call: identified by the distinctive "Barry's review" substring.
    // Returns disagree so both identities post.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("Barry's review"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_resp(
            r#"{"agree":false,"reason":"different outcomes"}"#,
        )))
        .mount(&server)
        .await;

    // Fallback: all persona + synthesis calls return request_changes.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_resp(
            r#"{"outcome":"request_changes","summary":"please fix","findings":[]}"#,
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

    // On disagreement, both Barry and Other Barry post reviews (exactly 2).
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/1/reviews"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(2)
        .mount(&server)
        .await;

    let (store, deps) = crate::common::fixture_with_llm(&server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();

    // Verify both identities are recorded as posted in the storage layer.
    let key = RunKey {
        owner: "o",
        repo: "r",
        pr: 1,
        head_sha: "sha1",
    };
    let st = store.run_state(key).await.unwrap().unwrap();
    assert!(st.barry_posted, "Barry should be recorded as posted");
    assert!(
        st.other_barry_posted,
        "Other Barry should be recorded as posted"
    );
}
