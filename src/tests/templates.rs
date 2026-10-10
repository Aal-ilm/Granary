//! Templates through the HTTP API: built-ins, create, edit, clone, delete,
//! export, checking and manifest import.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::Harness;

async fn open() -> (Harness, String) {
    let h = Harness::new(false, std::time::Duration::from_secs(300)).await;
    let cookie = h.login().await;
    (h, cookie)
}

fn by_name<'a>(list: &'a Value, name: &str) -> &'a Value {
    list["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no template named {name}"))
}

#[tokio::test]
async fn ships_read_only_builtins() {
    let (h, cookie) = open().await;
    let list = h.api(&cookie, "/api/templates").await;
    assert_eq!(list["templates"].as_array().unwrap().len(), 8);
    let cli = by_name(&list, "Rust CLI");
    assert_eq!(cli["builtin"], true);
    assert_eq!(cli["counts"]["cargo"], 7);
    assert_eq!(cli["platforms"], json!(["linux-x86_64", "macos-arm64"]));
    assert_eq!(by_name(&list, "React + Vite + TS")["counts"]["npm"], 8);
    assert_eq!(by_name(&list, "Java Spring web")["counts"]["maven"], 2);
    let id = cli["id"].as_i64().unwrap();

    let body = json!({"name": "Hacked", "cargo": "serde = \"1\""});
    let (status, _, _) = h
        .send(
            "PUT",
            &format!("/api/templates/{id}"),
            Some(&cookie),
            Some(body),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = h
        .send(
            "DELETE",
            &format!("/api/templates/{id}"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn create_edit_clone_delete() {
    let (h, cookie) = open().await;
    let body = json!({
        "name": "Team service",
        "description": "What our services use",
        "tags": ["Rust", "team"],
        "cargo": "# web\naxum = \"*\"\nserde = { version = \"1\", features = [\"derive\"] }",
        "pypi": "httpx",
    });
    let (status, _, out) = h
        .send("POST", "/api/templates", Some(&cookie), Some(body.clone()))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&out)
    );
    let id = serde_json::from_slice::<Value>(&out).unwrap()["id"]
        .as_i64()
        .unwrap();

    let t = h.api(&cookie, &format!("/api/templates/{id}")).await;
    assert_eq!(t["builtin"], false);
    assert_eq!(t["tags"], json!(["rust", "team"]));
    assert_eq!(t["counts"]["cargo"], 2);
    assert!(
        t["cargo"].as_str().unwrap().starts_with("# web"),
        "comments are kept"
    );

    // Same name again is refused.
    let (status, _, _) = h
        .send("POST", "/api/templates", Some(&cookie), Some(body))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Invalid lines are refused with the line named.
    let (status, _, out) = h
        .send(
            "PUT",
            &format!("/api/templates/{id}"),
            Some(&cookie),
            Some(json!({"name": "Team service", "npm": "\"x\": \"git+https://h/x.git\""})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&out).contains("npm line 1"));

    let (status, _, _) = h
        .send(
            "PUT",
            &format!("/api/templates/{id}"),
            Some(&cookie),
            Some(json!({"name": "Team service v2", "npm": "\"zod\": \"latest\""})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let t = h.api(&cookie, &format!("/api/templates/{id}")).await;
    assert_eq!(t["name"], "Team service v2");
    assert_eq!(t["counts"]["npm"], 1);
    assert_eq!(t["counts"]["cargo"], 0);

    // Clone a built-in: the copy is editable and named so it's distinct.
    let list = h.api(&cookie, "/api/templates").await;
    let builtin = by_name(&list, "Python data")["id"].as_i64().unwrap();
    let (status, _, out) = h
        .send(
            "POST",
            &format!("/api/templates/{builtin}/clone"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let copy: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(copy["name"], "Python data (copy)");
    let (_, _, out) = h
        .send(
            "POST",
            &format!("/api/templates/{builtin}/clone"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&out).unwrap()["name"],
        "Python data (copy 2)"
    );
    let copy_id = copy["id"].as_i64().unwrap();
    let t = h.api(&cookie, &format!("/api/templates/{copy_id}")).await;
    assert_eq!(t["builtin"], false);
    assert_eq!(t["counts"]["pypi"], 5);

    let (status, _, _) = h
        .send(
            "DELETE",
            &format!("/api/templates/{copy_id}"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = h
        .send(
            "GET",
            &format!("/api/templates/{copy_id}"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Every change is in the audit log.
    let audit = h.api(&cookie, "/api/audit?action=template").await;
    let actions: Vec<&str> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    for a in [
        "template.create",
        "template.update",
        "template.clone",
        "template.delete",
    ] {
        assert!(actions.contains(&a), "{a} missing from {actions:?}");
    }
}

#[tokio::test]
async fn export_is_a_loadable_toml_file() {
    let (h, cookie) = open().await;
    let list = h.api(&cookie, "/api/templates").await;
    let id = by_name(&list, "Rust async service")["id"].as_i64().unwrap();
    let (status, headers, body) = h
        .send(
            "GET",
            &format!("/api/templates/{id}/export"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-disposition"]
            .to_str()
            .unwrap()
            .contains("rust-async-service.toml")
    );
    let t = crate::templates::TemplateInput::from_toml(&String::from_utf8(body).unwrap()).unwrap();
    assert_eq!(t.name, "Rust async service");
    assert!(t.validate().is_none());
    assert_eq!(t.deps(crate::templates::lines::Eco::Cargo).len(), 7);
}

#[tokio::test]
async fn check_and_import_for_the_editor() {
    let (h, cookie) = open().await;
    let (_, _, out) = h
        .send(
            "POST",
            "/api/templates/check",
            Some(&cookie),
            Some(json!({"name": "Rust CLI", "cargo": "serde = \"1\"\nnot a line", "maven": "a:b:1-SNAPSHOT"})),
        )
        .await;
    let d: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(d["ok"], false);
    assert_eq!(d["counts"]["cargo"], 1);
    assert_eq!(d["problems"]["lines"]["cargo"][0]["line"], 2);
    assert!(
        d["problems"]["lines"]["maven"][0]["message"]
            .as_str()
            .unwrap()
            .contains("snapshots")
    );
    assert!(
        d["problems"]["fields"][0]["message"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );

    let (status, _, out) = h
        .send(
            "POST",
            "/api/templates/import",
            Some(&cookie),
            Some(json!({"filename": "requirements.txt", "content": "fastapi\n-e .\n"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let d: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(d["ecosystem"], "pypi");
    assert_eq!(d["lines"], json!(["fastapi"]));
    assert_eq!(d["skipped"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn viewers_can_look_but_not_change() {
    let h = Harness::new(false, std::time::Duration::from_secs(300)).await;
    let viewer = h.viewer_login().await;
    let list = h.api(&viewer, "/api/templates").await;
    let id = by_name(&list, "Node API")["id"].as_i64().unwrap();
    h.api(&viewer, &format!("/api/templates/{id}")).await;
    let (status, _, _) = h
        .send(
            "GET",
            &format!("/api/templates/{id}/export"),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    for (method, path, body) in [
        (
            "POST",
            "/api/templates".to_string(),
            Some(json!({"name": "x", "pypi": "httpx"})),
        ),
        ("POST", format!("/api/templates/{id}/clone"), None),
        (
            "POST",
            "/api/templates/check".to_string(),
            Some(json!({"name": "x"})),
        ),
    ] {
        let (status, _, _) = h.send(method, &path, Some(&viewer), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
}

#[tokio::test]
async fn changes_need_the_csrf_header() {
    let (h, cookie) = open().await;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/templates")
        .header("cookie", &cookie)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({"name": "x", "pypi": "httpx"}).to_string(),
        ))
        .unwrap();
    use tower::ServiceExt;
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
