use barry_dylan::dispatcher::run::run_job;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn opened_pr_runs_hygiene_and_posts_check_runs() {
    let server = MockServer::start().await;

    // PR metadata + comments + reviews + .barry.toml blob (single GraphQL round trip).
    Mock::given(method("POST")).and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            crate::common::graphql_pr_context(
                1, "alice", "sha1", None,
                serde_json::json!([]), serde_json::json!([]),
            )
        )).mount(&server).await;

    Mock::given(method("GET")).and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "filename": "src/lib.rs", "status": "modified",
              "additions": 10, "deletions": 0, "changes": 10,
              "patch": "@@ -1,1 +1,2 @@\n a\n+b\n" }
        ]))).mount(&server).await;

    Mock::given(method("GET")).and(path("/repos/o/r/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "write"
        }))).mount(&server).await;

    // Check Run writes: capture how many.
    Mock::given(method("POST")).and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(4..)
        .mount(&server).await;

    let (store, deps) = crate::common::fixture(&server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
    // wiremock `expect(4..)` verifies on Drop.
}
