use barry_dylan::dispatcher::run::run_job;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn untrusted_author_only_gets_needs_approval_comment() {
    let server = MockServer::start().await;

    Mock::given(method("POST")).and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            crate::common::graphql_pr_context(
                1, "stranger", "sha1", None,
                serde_json::json!([]), serde_json::json!([]),
            )
        )).mount(&server).await;

    Mock::given(method("GET")).and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/repos/o/r/collaborators/stranger/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "read"
        }))).mount(&server).await;

    // Expect the needs-approval comment.
    Mock::given(method("POST")).and(path("/repos/o/r/issues/1/comments"))
        .and(body_string_contains("barry-dylan:needs-approval:v1"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 7 })))
        .expect(1)
        .mount(&server).await;

    // Forbid any check-run posts.
    Mock::given(method("POST")).and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server).await;

    let (store, deps) = crate::common::fixture(&server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
}
