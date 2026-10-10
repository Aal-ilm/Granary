//! GRANARY_CACHE_LIMIT on the Overview API.

use super::{Harness, Opts};

#[tokio::test]
async fn overview_reports_budget_and_usage() {
    let h = Harness::with(Opts {
        cache_limit: Some(20 << 30),
        ..Opts::default()
    })
    .await;
    let jar = h.data.join("files/maven/org/example/demo/1.0/demo-1.0.jar");
    std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
    std::fs::write(&jar, vec![0u8; 1000]).unwrap();

    let cookie = h.login().await;
    let d = h.api(&cookie, "/api/overview").await;
    assert_eq!(d["cache_limit"], 20u64 << 30);
    assert_eq!(d["cache_used"], 1000);

    let h = Harness::new(false, std::time::Duration::from_secs(300)).await;
    let cookie = h.login().await;
    let d = h.api(&cookie, "/api/overview").await;
    assert!(d["cache_limit"].is_null());
}
