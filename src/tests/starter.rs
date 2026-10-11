//! Download starter and scheduled re-warms, through the HTTP API.

use std::{collections::HashMap, io::Read, time::Duration};

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::{
    Harness, Opts,
    jobs::{Plan, set_tiny_package, wait_for, with_fake},
};

fn unzip(bytes: &[u8]) -> HashMap<String, String> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut out = HashMap::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let name = f.name().unwrap().to_string();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        out.insert(name, s);
    }
    out
}

async fn create(h: &Harness, cookie: &str, body: Value) -> i64 {
    let (status, _, out) = h
        .send("POST", "/api/templates", Some(cookie), Some(body))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&out)
    );
    serde_json::from_slice::<Value>(&out).unwrap()["id"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn starter_after_a_warm_has_lockfiles_and_no_token() {
    let plan = Plan {
        lockfiles: HashMap::from([(
            "package-lock.json".into(),
            r#"{"lockfileVersion":3,"packages":{"node_modules/granary-tiny":{"version":"1.0.0"}}}"#
                .into(),
        )]),
        ..Plan::default()
    };
    let (h, fake, cookie) = with_fake(
        plan,
        Opts {
            client_auth: true,
            ..Opts::default()
        },
    )
    .await;
    set_tiny_package(&h, include_bytes!("fixtures/granary-tiny-1.0.0.npm.tar.gz"));
    let tid = create(
        &h,
        &cookie,
        json!({"name": "Tiny web", "npm": "\"granary-tiny\": \"^1\""}),
    )
    .await;

    let (_, _, out) = h
        .send(
            "POST",
            &format!("/api/templates/{tid}/warm"),
            Some(&cookie),
            None,
        )
        .await;
    let job = serde_json::from_slice::<Value>(&out).unwrap()["id"]
        .as_i64()
        .unwrap();
    wait_for(&h, &cookie, job, &["done"]).await;

    let (status, headers, zip) = h
        .send(
            "GET",
            &format!("/api/templates/{tid}/starter"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/zip");
    assert!(
        headers["content-disposition"]
            .to_str()
            .unwrap()
            .contains("tiny-web-starter.zip")
    );
    let files = unzip(&zip);
    let mut names: Vec<&str> = files.keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "tiny-web/.npmrc",
            "tiny-web/README.txt",
            "tiny-web/package-lock.json",
            "tiny-web/package.json",
        ]
    );
    assert!(files["tiny-web/package-lock.json"].contains("granary-tiny"));
    assert!(files["tiny-web/.npmrc"].contains("${GRANARY_TOKEN}"));

    // No token, anywhere: not the admin's, not the warmer's, not any.
    let warmer_token = fake.runs.lock().unwrap()[0]["token"]
        .as_str()
        .unwrap()
        .to_string();
    for (name, text) in &files {
        for secret in [h.token.as_str(), warmer_token.as_str(), "gran_"] {
            assert!(!text.contains(secret), "{name} contains {secret}");
        }
    }

    // Viewers can download starters too.
    let viewer = h.viewer_login().await;
    let (status, _, _) = h
        .send(
            "GET",
            &format!("/api/templates/{tid}/starter"),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn starter_before_any_warm() {
    let h = Harness::new(false, Duration::from_secs(300)).await;
    let cookie = h.login().await;
    let tid = create(
        &h,
        &cookie,
        json!({"name": "Fresh", "cargo": "serde = \"1\"", "pypi": "httpx", "maven": "junit:junit:4.13.2"}),
    )
    .await;
    let (status, _, zip) = h
        .send(
            "GET",
            &format!("/api/templates/{tid}/starter"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let files = unzip(&zip);
    assert!(files.contains_key("fresh/Cargo.toml"));
    assert!(files.contains_key("fresh/.cargo/config.toml"));
    assert!(files.contains_key("fresh/requirements.txt"));
    assert!(files.contains_key("fresh/pip.conf"));
    assert!(files.contains_key("fresh/.mvn/settings.xml"));
    assert!(
        !files.keys().any(|k| k.ends_with("Cargo.lock")),
        "no warm, no lockfile"
    );
    assert!(files["fresh/README.txt"].contains("has not been warmed yet"));
    assert!(files["fresh/pom.xml"].contains("<version>4.13.2</version>"));

    // A Maven line without a version needs a warm (or a lookup) first.
    let tid = create(
        &h,
        &cookie,
        json!({"name": "No version", "maven": "org.example:missing"}),
    )
    .await;
    let (status, _, body) = h
        .send(
            "GET",
            &format!("/api/templates/{tid}/starter"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(String::from_utf8_lossy(&body).contains("org.example:missing"));
}

#[tokio::test]
async fn scheduled_rewarms() {
    let (h, _fake, cookie) = with_fake(Plan::default(), Opts::default()).await;
    let list = h.api(&cookie, "/api/templates").await;
    let builtin = list["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "Rust CLI")
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    // Built-in templates can be scheduled: it's this Granary's setting.
    let (status, _, _) = h
        .send(
            "PUT",
            &format!("/api/templates/{builtin}/schedule"),
            Some(&cookie),
            Some(json!({"schedule": "weekly"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let t = h.api(&cookie, &format!("/api/templates/{builtin}")).await;
    assert_eq!(t["schedule"], "weekly");
    let (status, _, _) = h
        .send(
            "PUT",
            &format!("/api/templates/{builtin}/schedule"),
            Some(&cookie),
            Some(json!({"schedule": "hourly"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let viewer = h.viewer_login().await;
    let (status, _, _) = h
        .send(
            "PUT",
            &format!("/api/templates/{builtin}/schedule"),
            Some(&viewer),
            Some(json!({"schedule": "off"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Never warmed, so it's due now; the next check doesn't queue it again.
    assert_eq!(crate::jobs::queue_scheduled(&h.state).await, 1);
    assert_eq!(crate::jobs::queue_scheduled(&h.state).await, 0);
    let jobs = h
        .api(&cookie, &format!("/api/jobs?template={builtin}"))
        .await;
    let job = &jobs["jobs"][0];
    assert_eq!(job["created_by"], "schedule");
    let id = job["id"].as_i64().unwrap();
    wait_for(&h, &cookie, id, &["done"]).await;
    // Warmed just now: not due again until next week.
    assert_eq!(crate::jobs::queue_scheduled(&h.state).await, 0);

    // The schedule is not part of the template's exported content.
    let (_, _, toml) = h
        .send(
            "GET",
            &format!("/api/templates/{builtin}/export"),
            Some(&cookie),
            None,
        )
        .await;
    assert!(!String::from_utf8_lossy(&toml).contains("weekly"));
}
