use barry_bot::storage::queue::NewJob;
use barry_bot::storage::Store;

#[tokio::test]
async fn synchronize_events_coalesce_into_one_job() {
    let store = Store::in_memory().await.unwrap();
    let job = NewJob {
        installation_id: 1, repo_owner: "o".into(), repo_name: "r".into(),
        pr_number: 1, event_kind: "pull_request.synchronize".into(), delivery_id: "d".into(),
    };
    for (delivery, now) in [("d1", 100), ("d2", 110), ("d3", 120)] {
        let mut j = job.clone();
        j.delivery_id = delivery.into();
        store.enqueue(&j, now, now + 30).await.unwrap();
    }
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM jobs")
        .fetch_one(&store.pool).await.unwrap();
    assert_eq!(n, 1);
    let after = store.pending_run_after("o", "r", 1, "pull_request.synchronize")
        .await.unwrap();
    assert_eq!(after, Some(150));
}
