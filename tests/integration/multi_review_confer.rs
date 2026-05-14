use barry_dylan::checker::multi_review::identity::Identity;
use barry_dylan::dispatcher::run::run_job;
use barry_dylan::storage::multi_review::RunKey;
use barry_dylan::storage::queue::NewJob;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
async fn confer_summons_other_barry_after_barry_posted() {
    let server = MockServer::start().await;

    // PR context: alice commented "/barry confer"; Barry already posted a review on sha1.
    let prior_review_body = "<!-- barry-dylan:multi-review:barry:v1 -->\n**Barry** — Approve\nLGTM";
    let comments = serde_json::json!([{
        "databaseId": 1, "id": "c1",
        "author": { "login": "alice" }, "body": "/barry confer"
    }]);
    let reviews = serde_json::json!([{
        "databaseId": 100, "id": "r1",
        "author": { "login": "barry-dylan[bot]" },
        "body": prior_review_body,
    }]);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
                1, "alice", "sha1", None, comments, reviews,
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

    // Synthesis call (identified by the distinctive "synthesis stage" prompt substring).
    // Mount first so it takes priority over the fallback persona mock.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("synthesis stage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_resp(
            r#"{"outcome":"approve","summary":"OB agrees","findings":[]}"#,
        )))
        .mount(&server)
        .await;

    // Persona drafts (3 calls): fallback approves.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_resp(r#"{"findings":[],"summary":"persona stub"}"#)),
        )
        .mount(&server)
        .await;

    // OB posts exactly one review.
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/1/reviews"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;

    // OB's check-run from posting.
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .mount(&server)
        .await;

    let (store, deps) = crate::common::fixture_with_llm(&server).await;

    // Pre-populate: Barry already posted approve on sha1.
    let key = RunKey {
        owner: "o".to_string(),
        repo: "r".to_string(),
        pr: 1,
        head_sha: "sha1".to_string(),
    };
    store
        .record_post(key.clone(), Identity::Barry, "approve", 100)
        .await
        .unwrap();

    // Enqueue a confer job.
    let job = NewJob {
        installation_id: 1,
        repo_owner: "o".into(),
        repo_name: "r".into(),
        pr_number: 1,
        event_kind: "issue_comment.confer".into(),
        delivery_id: "d-confer".into(),
    };
    store.enqueue(&job, 0, 0).await.unwrap();
    let leased = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &leased).await.unwrap();

    let st = store.run_state(key).await.unwrap().unwrap();
    assert!(st.barry_posted);
    assert!(st.other_barry_posted);
    assert!(!st.other_other_barry_posted);
    assert_eq!(st.confers_used, 1);
}

#[tokio::test]
async fn confer_rejected_when_max_per_pr_reached() {
    let server = MockServer::start().await;

    let comments = serde_json::json!([{
        "databaseId": 1, "id": "c1",
        "author": { "login": "alice" }, "body": "/barry confer"
    }]);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
                1,
                "alice",
                "sha1",
                None,
                comments,
                serde_json::json!([]),
            )),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "write"
        })))
        .mount(&server)
        .await;

    // Expect an "all reached" issue comment, no review posts.
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/1/reviews"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let (store, deps) = crate::common::fixture_with_llm(&server).await;

    // Pre-populate confers_used = 2 (= max_per_pr in the test config).
    let key = RunKey {
        owner: "o".to_string(),
        repo: "r".to_string(),
        pr: 1,
        head_sha: "sha1".to_string(),
    };
    store
        .record_post(key.clone(), Identity::Barry, "approve", 100)
        .await
        .unwrap();
    store.record_confer_used(key.clone(), 101).await.unwrap();
    store.record_confer_used(key.clone(), 102).await.unwrap();

    let job = NewJob {
        installation_id: 1,
        repo_owner: "o".into(),
        repo_name: "r".into(),
        pr_number: 1,
        event_kind: "issue_comment.confer".into(),
        delivery_id: "d-confer-max".into(),
    };
    store.enqueue(&job, 0, 0).await.unwrap();
    let leased = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &leased).await.unwrap();

    let st = store.run_state(key).await.unwrap().unwrap();
    assert!(!st.other_barry_posted);
    assert_eq!(st.confers_used, 2);
}
