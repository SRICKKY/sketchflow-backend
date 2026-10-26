use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{str::FromStr, sync::Arc};
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;

async fn request(
    app: &Router,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header("cookie", format!("sketchflow_session={token}"));
    }
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("editor API returns JSON")
    };
    (status, value)
}

fn rectangle(x: i64) -> Value {
    json!({
        "id": "element-1",
        "type": "rectangle",
        "x": x,
        "y": 2,
        "width": 10,
        "height": 20,
        "rotation": 0,
        "strokeColor": "#000",
        "backgroundColor": "transparent",
        "strokeWidth": 2,
        "opacity": 1,
        "fillStyle": "solid",
        "strokeStyle": "solid",
        "roundness": "soft",
        "roughness": 0,
        "seed": 42,
        "groupId": null,
        "frameId": null,
        "zIndex": 3,
        "locked": false
    })
}

#[tokio::test]
#[ignore = "requires explicit loopback DASHBOARD_TEST_DATABASE_URL ending in _test"]
async fn editor_http_flows_and_revision_checks() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("sketchflow_backend=debug")
        .try_init();
    let url =
        std::env::var("DASHBOARD_TEST_DATABASE_URL").expect("set DASHBOARD_TEST_DATABASE_URL");
    let options = PgConnectOptions::from_str(&url).unwrap();
    assert!(matches!(
        options.get_host(),
        "127.0.0.1" | "localhost" | "::1"
    ));
    assert!(options.get_database().unwrap().ends_with("_test"));
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .unwrap();
    let schema = format!("editor_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let db = PgPoolOptions::new()
        .max_connections(20)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&db).await.unwrap();

    let config = crate::config::AppConfig::from_env();
    let secret = config.jwt_secret.clone();
    let state = AppState {
        db: db.clone(),
        config: Arc::new(config),
        collaboration: crate::routes::collaboration::CollaborationHub::default(),
    };
    let app = router().layer(CookieManagerLayer::new()).with_state(state);
    for (id, email) in [
        ("editor_owner", "owner@test.invalid"),
        ("editor_viewer", "viewer@test.invalid"),
    ] {
        sqlx::query("INSERT INTO users (id, email) VALUES ($1, $2)")
            .bind(id)
            .bind(email)
            .execute(&db)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO boards (id, name, owner_id) VALUES ('editor-board', 'Editor board', 'editor_owner')")
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO board_members (id, board_id, user_id, role)
         VALUES ('editor-membership', 'editor-board', 'editor_viewer', 'VIEWER')",
    )
    .execute(&db)
    .await
    .unwrap();
    let token = |id| crate::auth::jwt::issue_token(id, &secret, 1).unwrap();
    let owner = token("editor_owner");
    let viewer = token("editor_viewer");
    let board = "/api/boards/editor-board";

    let (status, detail) =
        request(&app, Some(&owner), "GET", &format!("{board}/detail"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["revision"], 0);
    assert_eq!(detail["elements"], json!([]));

    let (status, result) = request(
        &app,
        Some(&owner),
        "PATCH",
        &format!("{board}/elements"),
        Some(json!({
            "baseRevision": 0,
            "clientOperationId": "element-insert",
            "upserts": [rectangle(1)],
            "deletedIds": []
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["revision"], 1);

    let (status, result) = request(
        &app,
        Some(&owner),
        "PATCH",
        &format!("{board}/elements"),
        Some(json!({
            "baseRevision": 0,
            "clientOperationId": "stale-element-edit",
            "upserts": [rectangle(99)],
            "deletedIds": []
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(result["code"], "REVISION_CONFLICT");
    assert_eq!(result["revision"], 1);

    let (status, result) = request(
        &app,
        Some(&owner),
        "PATCH",
        &format!("{board}/presentation"),
        Some(json!({
            "baseRevision": 1,
            "clientOperationId": "presentation-save",
            "presentation": {"frames": []}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["revision"], 2);

    let (status, result) = request(
        &app,
        Some(&owner),
        "POST",
        &format!("{board}/versions"),
        Some(json!({"label": "Before restore"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{result}");
    let (status, versions) = request(
        &app,
        Some(&owner),
        "GET",
        &format!("{board}/versions"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let version_id = versions
        .as_array()
        .unwrap()
        .iter()
        .find(|version| version["label"] == "Before restore")
        .unwrap()["id"]
        .as_str()
        .unwrap();

    let (status, result) = request(
        &app,
        Some(&owner),
        "PATCH",
        &format!("{board}/elements"),
        Some(json!({
            "baseRevision": 2,
            "clientOperationId": "element-move",
            "upserts": [rectangle(20)],
            "deletedIds": []
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["revision"], 3);

    let (status, restored) = request(
        &app,
        Some(&owner),
        "POST",
        &format!("{board}/versions/{version_id}/restore"),
        Some(json!({
            "baseRevision": 3,
            "clientOperationId": "restore-version"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert_eq!(restored["revision"], 4);
    assert_eq!(restored["elements"][0]["x"], 1);

    let (status, detail) =
        request(&app, Some(&owner), "GET", &format!("{board}/detail"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["revision"], 4);
    assert_eq!(detail["presentation"], json!({"frames": []}));
    assert_eq!(detail["elements"][0]["x"], 1);

    let document = json!({
        "baseRevision": 4,
        "clientOperationId": "document-save",
        "elements": [rectangle(2)],
        "presentation": {"frames": []}
    });
    let (status, result) = request(
        &app,
        Some(&owner),
        "PUT",
        &format!("{board}/document"),
        Some(document.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["revision"], 5);
    let (status, replay) = request(
        &app,
        Some(&owner),
        "PUT",
        &format!("{board}/document"),
        Some(document),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["revision"], 5);

    let (status, comment) = request(
        &app,
        Some(&owner),
        "POST",
        &format!("{board}/comments"),
        Some(json!({"body": "Review this", "x": 10, "y": 20})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{comment}");
    assert_eq!(comment["canEdit"], true);
    let comment_id = comment["id"].as_str().unwrap();

    let (status, reactions) = request(
        &app,
        Some(&owner),
        "POST",
        &format!("{board}/comments/{comment_id}/reactions"),
        Some(json!({"emoji": "👍"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reactions[0]["count"], 1);
    assert_eq!(reactions[0]["reactedByMe"], true);

    let (status, comments) = request(
        &app,
        Some(&viewer),
        "GET",
        &format!("{board}/comments"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(comments[0]["canEdit"], false);
    assert_eq!(comments[0]["canDelete"], false);

    let (status, error) = request(
        &app,
        Some(&viewer),
        "PATCH",
        &format!("{board}/comments/{comment_id}"),
        Some(json!({"body": "not mine"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(error["error"].is_string());

    db.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
