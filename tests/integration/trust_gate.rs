use barry_dylan::dispatcher::run::run_job;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn untrusted_author_only_gets_needs_approval_comment() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
                1,
                "stranger",
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
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/stranger/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "read"
        })))
        .mount(&server)
        .await;

    // Expect the needs-approval comment.
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/1/comments"))
        .and(body_string_contains("barry-dylan:needs-approval:v1"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 7 })))
        .expect(1)
        .mount(&server)
        .await;

    // Forbid any check-run posts.
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let (store, deps) = crate::common::fixture(&server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
}

/// Mounts the reads a review of PR 1 by `stranger` (read permission) makes,
/// with `comments` as the PR's conversation.
async fn mount_stranger_pr(server: &MockServer, comments: serde_json::Value) {
    // The trust gate tells barry's bot from a user of the same login by the
    // author's `__typename`, so the query has to ask for it.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("__typename"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
                1,
                "stranger",
                "sha1",
                None,
                comments,
                serde_json::json!([]),
            )),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/stranger/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "read"
        })))
        .mount(server)
        .await;
}

async fn run_opened(server: &MockServer) {
    let (store, deps) = crate::common::fixture(server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
}

/// Expects barry to ask for approval exactly once and to review nothing.
async fn expect_gated(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/1/comments"))
        .and(body_string_contains("barry-dylan:needs-approval:v1"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 7 })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(server)
        .await;
}

#[tokio::test]
async fn an_approve_marker_from_a_user_with_barrys_login_does_not_trust_the_author() {
    let server = MockServer::start().await;
    // GraphQL reports a bot's login without the `[bot]` suffix, so a user
    // account named `barry-dylan` has the same login string as barry's bot and
    // differs only in its type.
    mount_stranger_pr(
        &server,
        serde_json::json!([{
            "databaseId": 5, "id": "IC_5",
            "author": { "__typename": "User", "login": "barry-dylan" },
            "body": barry_dylan::dispatcher::trust::approve_comment_body(),
        }]),
    )
    .await;
    expect_gated(&server).await;
    run_opened(&server).await;
}

#[tokio::test]
async fn a_needs_approval_marker_from_a_user_with_barrys_login_does_not_stop_barry_asking() {
    let server = MockServer::start().await;
    mount_stranger_pr(
        &server,
        serde_json::json!([{
            "databaseId": 5, "id": "IC_5",
            "author": { "__typename": "User", "login": "barry-dylan" },
            "body": barry_dylan::dispatcher::trust::needs_approval_body("stranger"),
        }]),
    )
    .await;
    expect_gated(&server).await;
    run_opened(&server).await;
}

#[tokio::test]
async fn an_approve_marker_from_barrys_bot_trusts_the_author() {
    let server = MockServer::start().await;
    mount_stranger_pr(
        &server,
        serde_json::json!([{
            "databaseId": 5, "id": "IC_5",
            "author": { "__typename": "Bot", "login": "barry-dylan" },
            "body": barry_dylan::dispatcher::trust::approve_comment_body(),
        }]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 7 })))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(4..)
        .mount(&server)
        .await;
    run_opened(&server).await;
}

#[tokio::test]
async fn an_untrusted_authors_pr_never_fetches_its_changed_files() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
                1,
                "stranger",
                "sha1",
                None,
                serde_json::json!([]),
                serde_json::json!([]),
            )),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/collaborators/stranger/permission"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "permission": "read"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/1/comments"))
        .and(body_string_contains("barry-dylan:needs-approval:v1"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 7 })))
        .expect(1)
        .mount(&server)
        .await;

    // The file list (with every patch) is only needed by checkers, which an
    // untrusted author's PR never reaches.
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(0)
        .mount(&server)
        .await;

    let (store, deps) = crate::common::fixture(&server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
}

#[tokio::test]
async fn a_trusted_authors_pr_fetches_its_changed_files_for_the_checkers() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(crate::common::graphql_pr_context(
                1,
                "alice",
                "sha1",
                // An autolabel rule on src/** means a label is posted only if
                // the checkers received the fetched file list.
                Some("[hygiene.autolabel]\nrules = [ { paths = [\"src/**\"], labels = [\"area/src\"] } ]\n"),
                serde_json::json!([]),
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
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/o/r/pulls/1/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "filename": "src/lib.rs", "status": "modified",
              "additions": 10, "deletions": 0, "changes": 10,
              "patch": "@@ -1,1 +1,2 @@\n a\n+b\n" }
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/1/labels"))
        .and(body_string_contains("area/src"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .expect(1)
        .mount(&server)
        .await;

    let (store, deps) = crate::common::fixture(&server).await;
    crate::common::enqueue_opened(&store, "o", "r", 1).await;
    let job = store.lease_next(0, 300).await.unwrap().unwrap();
    run_job(&deps, &job).await.unwrap();
}
