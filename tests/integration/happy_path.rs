use barry_dylan::dispatcher::run::run_job;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn opened_pr_runs_hygiene_and_posts_check_runs() {
    let server = MockServer::start().await;

    // PR metadata.
    Mock::given(method("GET")).and(path("/repos/o/r/pulls/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": 1, "title": "feat: add x", "body": "Long enough body to pass checks.",
            "user": { "login": "alice" }, "draft": false, "state": "open",
            "head": { "sha": "sha1", "ref": "feat" },
            "base": { "sha": "sha0", "ref": "main" },
            "additions": 10, "deletions": 0, "changed_files": 1
        }))).mount(&server).await;

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

    // Repo config absent.
    Mock::given(method("GET")).and(path("/repos/o/r/contents/.barry.toml"))
        .respond_with(ResponseTemplate::new(404)).mount(&server).await;

    // No prior comments / reviews.
    Mock::given(method("GET")).and(path("/repos/o/r/issues/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/repos/o/r/pulls/1/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server).await;

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
