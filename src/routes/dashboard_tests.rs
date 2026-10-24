use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{str::FromStr, sync::Arc};
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;

#[test]
fn validates_names_roles_and_template_input() {
    assert!(name(" ", 80).is_err());
    assert_eq!(name(" Board ", 80).unwrap(), "Board");
    assert!(name(&"a".repeat(81), 80).is_err());
    assert!(workspace_name("a").is_err());
    assert!(workspace_name(&"a".repeat(41)).is_err());
    assert!(member_role("OWNER", true).is_err());
    assert!(member_role("OWNER", false).is_err());
    assert!(member_role("admin", true).is_err());
    assert!(member_role("EDITOR", true).is_err());
    assert!(member_role("MEMBER", false).is_err());
    assert!(member_role("ADMIN", true).is_ok());
    assert!(member_role("VIEWER", false).is_ok());
    assert!(template_description(Some("x".repeat(201))).is_err());
    assert!(serde_json::from_value::<ImportTemplateInput>(json!({
        "name":"test","elements":[],"appState":{}
    }))
    .is_err());
    assert!(validate_template_elements(&[json!({"id":"x","type":"script"})]).is_err());
    assert!(validate_template_elements(&[
        json!({"id":"x","type":"text"}),
        json!({"id":"x","type":"text"})
    ])
    .is_err());
}

#[test]
fn actual_builtins_remint_ids_and_bindings() {
    let templates = builtin_templates();
    assert_eq!(templates.len(), 7);
    for template in templates {
        assert!(template.builtin);
        assert!(template.element_count > 0);
        assert!(template.id.starts_with("builtin:"));
        let old_ids: Vec<_> = template.elements.iter().map(|e| e["id"].clone()).collect();
        let new_elements = remint_elements(template.elements).unwrap();
        for element in new_elements {
            assert!(!old_ids.contains(&element["id"]));
        }
    }
    let result = remint_elements(vec![
        json!({"id":"shape","type":"rectangle","groupId":"group","frameId":"frame"}),
        json!({"id":"arrow","type":"arrow","groupId":"group","startBinding":{"elementId":"shape"}}),
        json!({"id":"frame","type":"frame"}),
    ])
    .unwrap();
    assert_eq!(result[0]["groupId"], result[1]["groupId"]);
    assert_eq!(result[0]["frameId"], result[2]["id"]);
    assert_eq!(result[1]["startBinding"]["elementId"], result[0]["id"]);
}

async fn request(
    app: &Router,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
    expected: u16,
) -> Value {
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
    let value: Value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| panic!("non-JSON response: {status}"));
    assert_eq!(status.as_u16(), expected, "{method} {path}: {value}");
    if expected >= 400 {
        assert!(value["error"].is_string(), "{value}");
    }
    value
}

async fn user(db: &PgPool, user_id: &str, plan: &str, expired: bool) {
    sqlx::query("INSERT INTO users(id,name,email,plan,plan_expires_at) VALUES($1,$1,$2,$3,CASE WHEN $4 THEN now()-interval '1 day' ELSE now()+interval '30 days' END)")
        .bind(user_id).bind(format!("{user_id}@test.invalid")).bind(plan).bind(expired).execute(db).await.unwrap();
}

#[tokio::test]
#[ignore = "requires explicit loopback DASHBOARD_TEST_DATABASE_URL ending in _test"]
async fn dashboard_http_flows_and_authorization() {
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
    let schema = format!("dashboard_test_{}", uuid::Uuid::new_v4().simple());
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
        collaboration: super::super::collaboration::CollaborationHub::default(),
    };
    let app = router()
        .route(
            "/api/boards/{board_id}/snapshot",
            get(super::super::collaboration::snapshot),
        )
        .layer(CookieManagerLayer::new())
        .with_state(state.clone());
    for (user_id, plan, expired) in [
        ("free", "free", false),
        ("guest", "free", false),
        ("other", "free", false),
        ("pro", "pro", false),
        ("admin", "pro", false),
        ("member", "pro", false),
        ("expired", "pro", true),
    ] {
        user(&db, user_id, plan, expired).await;
    }
    let token = |user_id| crate::auth::jwt::issue_token(user_id, &secret, 1).unwrap();
    let free = token("free");
    let guest = token("guest");
    let other = token("other");
    let pro = token("pro");
    let admin_token = token("admin");
    let member_token = token("member");
    let expired = token("expired");
    request(&app, None, "GET", "/api/dashboard", None, 401).await;
    request(&app, Some("invalid"), "GET", "/api/boards", None, 401).await;
    let caps = request(&app, Some(&free), "GET", "/api/dashboard", None, 200).await;
    assert_eq!(caps["capabilities"]["maxOwnedBoards"], 5);
    assert_eq!(caps["capabilities"]["canCreateBoard"], true);
    request(
        &app,
        Some(&free),
        "GET",
        "/api/boards?scope=invalid",
        None,
        400,
    )
    .await;
    request(
        &app,
        Some(&free),
        "POST",
        "/api/workspaces",
        Some(json!({"name":"Team"})),
        403,
    )
    .await;
    request(
        &app,
        Some(&expired),
        "POST",
        "/api/workspaces",
        Some(json!({"name":"Team"})),
        403,
    )
    .await;
    let board = request(
        &app,
        Some(&free),
        "POST",
        "/api/boards",
        Some(json!({"name":"Alpha % board"})),
        201,
    )
    .await;
    let board_id = board["id"].as_str().unwrap();
    assert_eq!(board["role"], "OWNER");
    assert_eq!(board["workspaceId"], Value::Null);
    let path = format!("/api/boards/{board_id}");
    request(&app, Some(&other), "GET", &path, None, 404).await;
    request(
        &app,
        Some(&free),
        "PATCH",
        &path,
        Some(json!({"name":"Alpha board"})),
        200,
    )
    .await;
    let board_member = request(
        &app,
        Some(&free),
        "POST",
        &format!("{path}/members"),
        Some(json!({"email":" GUEST@test.invalid ","role":"VIEWER"})),
        201,
    )
    .await;
    let member_id = board_member["id"].as_str().unwrap();
    assert_eq!(board_member["userId"], "guest");
    request(
        &app,
        Some(&free),
        "POST",
        &format!("{path}/members"),
        Some(json!({"email":"guest@test.invalid","role":"VIEWER"})),
        409,
    )
    .await;
    request(
        &app,
        Some(&free),
        "POST",
        &format!("{path}/members"),
        Some(json!({"email":"other@test.invalid","role":"EDITOR"})),
        403,
    )
    .await;
    request(
        &app,
        Some(&guest),
        "PATCH",
        &path,
        Some(json!({"name":"attack"})),
        403,
    )
    .await;
    request(&app, Some(&guest), "DELETE", &path, None, 403).await;
    request(
        &app,
        Some(&guest),
        "POST",
        &format!("{path}/members"),
        Some(json!({"email":"other@test.invalid","role":"EDITOR"})),
        403,
    )
    .await;
    let shared = request(
        &app,
        Some(&guest),
        "GET",
        "/api/boards?scope=shared&q=Alpha",
        None,
        200,
    )
    .await;
    assert_eq!(shared["items"].as_array().unwrap().len(), 1);
    let owner_members = request(
        &app,
        Some(&free),
        "GET",
        &format!("{path}/members"),
        None,
        200,
    )
    .await;
    let owner_member = owner_members["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "OWNER")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    request(
        &app,
        Some(&free),
        "DELETE",
        &format!("{path}/members/{owner_member}"),
        None,
        403,
    )
    .await;
    request(
        &app,
        Some(&free),
        "PATCH",
        &format!("{path}/members/{member_id}"),
        Some(json!({"role":"EDITOR"})),
        200,
    )
    .await;
    request(
        &app,
        Some(&free),
        "PATCH",
        &format!("{path}/members/{member_id}"),
        Some(json!({"role":"OWNER"})),
        400,
    )
    .await;
    request(
        &app,
        Some(&free),
        "DELETE",
        &format!("{path}/members/{member_id}"),
        None,
        200,
    )
    .await;
    request(&app, Some(&guest), "GET", &path, None, 404).await;
    let searches = request(&app, Some(&free), "GET", "/api/boards?q=%25", None, 200).await;
    assert_eq!(searches["items"].as_array().unwrap().len(), 0);
    for _ in 0..3 {
        request(
            &app,
            Some(&free),
            "POST",
            "/api/boards",
            Some(json!({})),
            201,
        )
        .await;
    }
    let (one, two) = tokio::join!(
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/boards")
                .header("cookie", format!("sketchflow_session={free}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap()
        ),
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/boards")
                .header("cookie", format!("sketchflow_session={free}"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap()
        )
    );
    let mut statuses = [
        one.unwrap().status().as_u16(),
        two.unwrap().status().as_u16(),
    ];
    statuses.sort();
    assert_eq!(statuses, [201, 403]);
    let caps = request(&app, Some(&free), "GET", "/api/dashboard", None, 200).await;
    assert_eq!(caps["capabilities"]["ownedBoards"], 5);
    assert_eq!(caps["capabilities"]["canCreateBoard"], false);
    request(
        &app,
        Some(&free),
        "POST",
        &format!("{path}/duplicate"),
        Some(json!({})),
        403,
    )
    .await;

    let templates = request(&app, Some(&pro), "GET", "/api/templates", None, 200).await;
    assert_eq!(templates["items"].as_array().unwrap().len(), 7);
    let from_template = request(
        &app,
        Some(&pro),
        "POST",
        "/api/boards",
        Some(json!({"templateId":"builtin:system-design"})),
        201,
    )
    .await;
    let template_board_id = from_template["id"].as_str().unwrap();
    let snapshot = request(
        &app,
        Some(&pro),
        "GET",
        &format!("/api/boards/{template_board_id}/snapshot"),
        None,
        200,
    )
    .await;
    assert_eq!(snapshot["revision"], 0);
    assert_eq!(snapshot["elements"].as_array().unwrap().len(), 21);
    let duplicate = request(
        &app,
        Some(&pro),
        "POST",
        &format!("/api/boards/{template_board_id}/duplicate"),
        Some(json!({})),
        201,
    )
    .await;
    assert_eq!(duplicate["elementCount"], 21);
    let duplicate_snapshot = request(
        &app,
        Some(&pro),
        "GET",
        &format!("/api/boards/{}/snapshot", duplicate["id"].as_str().unwrap()),
        None,
        200,
    )
    .await;
    assert_ne!(
        duplicate_snapshot["elements"][0]["id"],
        snapshot["elements"][0]["id"]
    );
    let saved = request(
        &app,
        Some(&pro),
        "POST",
        &format!("/api/boards/{template_board_id}/template"),
        Some(json!({"name":"Saved design"})),
        201,
    )
    .await;
    let saved_id = saved["id"].as_str().unwrap();
    request(
        &app,
        Some(&free),
        "POST",
        &format!("{path}/template"),
        Some(json!({"name":"No"})),
        403,
    )
    .await;
    request(
        &app,
        Some(&guest),
        "POST",
        "/api/boards",
        Some(json!({"templateId":saved_id})),
        404,
    )
    .await;
    request(
        &app,
        Some(&pro),
        "POST",
        "/api/boards",
        Some(json!({"templateId":saved_id})),
        201,
    )
    .await;
    request(
        &app,
        Some(&pro),
        "POST",
        "/api/templates",
        Some(json!({"name":"Bad","elements":[{"id":"x","type":"script"}]})),
        400,
    )
    .await;
    request(
        &app,
        Some(&pro),
        "POST",
        "/api/templates",
        Some(json!({"name":"Bad","elements":[],"appState":{}})),
        400,
    )
    .await;
    let imported = request(
        &app,
        Some(&pro),
        "POST",
        "/api/templates",
        Some(json!({"name":"Imported","elements":[{"id":"x","type":"text","text":"Hello"}]})),
        201,
    )
    .await;
    assert_eq!(imported["elementCount"], 1);
    request(
        &app,
        Some(&guest),
        "DELETE",
        &format!("/api/templates/{saved_id}"),
        None,
        404,
    )
    .await;
    request(
        &app,
        Some(&pro),
        "DELETE",
        &format!("/api/templates/{saved_id}"),
        None,
        200,
    )
    .await;
    request(
        &app,
        Some(&pro),
        "DELETE",
        "/api/templates/builtin:flowchart",
        None,
        400,
    )
    .await;

    let workspace = request(
        &app,
        Some(&pro),
        "POST",
        "/api/workspaces",
        Some(json!({"name":"Our Team"})),
        201,
    )
    .await;
    let workspace_id = workspace["id"].as_str().unwrap();
    let wp = format!("/api/workspaces/{workspace_id}");
    request(&app, Some(&free), "GET", &wp, None, 403).await;
    request(&app, Some(&admin_token), "GET", &wp, None, 404).await;
    request(
        &app,
        Some(&pro),
        "POST",
        &format!("{wp}/members"),
        Some(json!({"email":"guest@test.invalid","role":"MEMBER"})),
        403,
    )
    .await;
    request(
        &app,
        Some(&pro),
        "POST",
        &format!("{wp}/members"),
        Some(json!({"email":"expired@test.invalid","role":"MEMBER"})),
        403,
    )
    .await;
    let admin_member = request(
        &app,
        Some(&pro),
        "POST",
        &format!("{wp}/members"),
        Some(json!({"email":"admin@test.invalid","role":"ADMIN"})),
        201,
    )
    .await;
    let added = request(
        &app,
        Some(&pro),
        "POST",
        &format!("{wp}/members"),
        Some(json!({"email":"member@test.invalid","role":"MEMBER"})),
        201,
    )
    .await;
    let added_id = added["id"].as_str().unwrap();
    request(
        &app,
        Some(&pro),
        "POST",
        &format!("{wp}/members"),
        Some(json!({"email":"member@test.invalid","role":"OWNER"})),
        400,
    )
    .await;
    let detail = request(&app, Some(&member_token), "GET", &wp, None, 200).await;
    assert_eq!(detail["members"].as_array().unwrap().len(), 3);
    assert_eq!(detail["role"], "MEMBER");
    let owner_member = detail["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "OWNER")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    request(
        &app,
        Some(&admin_token),
        "DELETE",
        &format!("{wp}/members/{owner_member}"),
        None,
        403,
    )
    .await;
    request(
        &app,
        Some(&member_token),
        "PATCH",
        &wp,
        Some(json!({"name":"No"})),
        403,
    )
    .await;
    request(
        &app,
        Some(&admin_token),
        "PATCH",
        &wp,
        Some(json!({"name":"Renamed Team"})),
        200,
    )
    .await;
    request(&app, Some(&admin_token), "DELETE", &wp, None, 403).await;
    request(
        &app,
        Some(&admin_token),
        "PATCH",
        &format!("{wp}/members/{added_id}"),
        Some(json!({"role":"ADMIN"})),
        200,
    )
    .await;
    request(
        &app,
        Some(&pro),
        "PATCH",
        &format!("{wp}/members/{added_id}"),
        Some(json!({"role":"MEMBER"})),
        200,
    )
    .await;
    let team_board=request(&app,Some(&member_token),"POST","/api/boards",Some(json!({"name":"Team board","workspaceId":workspace_id,"templateId":"builtin:flowchart"})),201).await;
    let team_board_id = team_board["id"].as_str().unwrap();
    let team_list = request(
        &app,
        Some(&pro),
        "GET",
        &format!("/api/boards?workspaceId={workspace_id}"),
        None,
        200,
    )
    .await;
    assert_eq!(team_list["items"][0]["role"], "EDITOR");
    assert_eq!(team_list["items"][0]["elementCount"], 18);
    request(
        &app,
        Some(&free),
        "POST",
        "/api/boards",
        Some(json!({"workspaceId":workspace_id})),
        403,
    )
    .await;
    request(&app, Some(&pro), "POST", &format!("{wp}/leave"), None, 403).await;
    request(
        &app,
        Some(&member_token),
        "POST",
        &format!("{wp}/leave"),
        None,
        200,
    )
    .await;
    request(&app, Some(&member_token), "GET", &wp, None, 404).await;
    request(
        &app,
        Some(&pro),
        "DELETE",
        &format!("{wp}/members/{}", admin_member["id"].as_str().unwrap()),
        None,
        200,
    )
    .await;
    request(&app, Some(&admin_token), "GET", &wp, None, 404).await;
    request(&app, Some(&pro), "DELETE", &wp, None, 200).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM boards WHERE id=$1")
            .bind(team_board_id)
            .fetch_one(&db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM board_collaboration_state WHERE board_id=$1"
        )
        .bind(team_board_id)
        .fetch_one(&db)
        .await
        .unwrap(),
        0
    );
    request(&app, Some(&free), "DELETE", &path, None, 200).await;
    let caps = request(&app, Some(&free), "GET", "/api/dashboard", None, 200).await;
    assert_eq!(caps["capabilities"]["canCreateBoard"], true);
    db.close().await;
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
