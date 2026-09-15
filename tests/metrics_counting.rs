//! Does a counter actually count?
//!
//! It did not. On delta every `barry_*_total` rendered as 1 however many times
//! it was incremented — four deliveries, four log lines, four on `/healthz`,
//! and 1 on `/metrics`. metrics-exporter-prometheus 0.16.2 (with metrics-util
//! 0.19.1) handed back a FRESH metric on every `counter!()` call, so only the
//! increments made through a handle you kept were ever rendered. Code that
//! calls the macro inline each time — which is how the macro is meant to be
//! used, and what barry does everywhere — counted to one, forever.
//!
//! Fixed by moving to the 0.18 exporter (metrics-util 0.20). This test is here
//! so a future dependency bump cannot quietly restore it: the failure is
//! invisible in the service, because a stuck counter looks exactly like a quiet
//! week.
//!
//! Its own test binary because a global recorder can only be installed once per
//! process, and `cargo test` shares a process across the tests in one binary.

use metrics_exporter_prometheus::PrometheusBuilder;

fn value_of(rendered: &str, needle: &str) -> Option<f64> {
    rendered
        .lines()
        .find(|l| l.starts_with(needle))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

#[test]
fn every_kind_of_metric_counts_every_increment() {
    // One install, one render, every assertion: a global recorder can be
    // installed once per process, and a second test that quietly fell back to
    // a different recorder would assert against an empty registry and pass
    // without testing anything.
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("install recorder");

    for _ in 0..4 {
        metrics::counter!("barry_test_plain_total").increment(1);
        metrics::counter!("barry_test_labelled_total", "reason" => "repo".to_string()).increment(1);
        // barry_relay_connected is a gauge, and gauges broke the same way.
        metrics::gauge!("barry_test_gauge").increment(1.0);
    }

    let rendered = handle.render();
    for metric in [
        "barry_test_plain_total",
        "barry_test_labelled_total",
        "barry_test_gauge",
    ] {
        assert_eq!(
            value_of(&rendered, metric),
            Some(4.0),
            "{metric} did not count four increments:\n{rendered}"
        );
    }
}
