use barry_dylan::dispatcher::run::run_job;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn merged_pr_aborts_without_posting() {
    let server = MockServer::start().await;

    let pr_ctx = serde_json::json!({
        "data": {
            "repository": {
                "pullRequest": {
                    "number": 1,
                    "title": "feat: something",
                    "body": "Long enough body.",
                    "state": "MERGED",
                    "isDraft": false,
                    "additions": 1,
                    "deletions": 0,
                    "changedFiles": 1,
                    "author": { "login": "alice" },
                    "headRefOid": "sha1",
                    "headRefName": "feat",
                    "baseRefOid": "sha0",
                    "baseRefName": "main",
                    "comments": { "nodes": [] },
                    "reviews": { "nodes": [] },
                },
                "config": null,
            }
        }
    });

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr_ctx))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    // Nothing downstream should be called.
    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/alice/permission"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/repos/o/r/issues/1/comments"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let (store, deps) = crate::common::fixture(&server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
}
