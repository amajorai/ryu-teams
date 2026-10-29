//! The `/api/teams/*` HTTP status contract, driven through the real `routes(ctx)`
//! router with `tower::ServiceExt::oneshot` over an in-memory store: create
//! validation (400, nothing persisted), 201 on create, 404 for unknown ids on every
//! by-id verb, and the idempotent member add / remove endpoints.

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use ryu_teams::api::{routes, TeamsCtx};
use ryu_teams::TeamStore;
use serde_json::{json, Value};
use tower::ServiceExt;

fn app() -> (Router, TeamStore) {
    let store = TeamStore::open_in_memory().expect("open in-memory teams store");
    (routes(TeamsCtx::new(store.clone())), store)
}

async fn send(app: &Router, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("router is infallible");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn create(app: &Router, name: &str, members: &[&str]) -> String {
    let (status, body) = send(
        app,
        Method::POST,
        "/",
        Some(json!({ "name": name, "members": members })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["team"]["id"].as_str().expect("a team id").to_owned()
}

#[tokio::test]
async fn blank_or_whitespace_name_is_rejected_and_nothing_persists() {
    let (app, store) = app();
    for name in ["", "   ", "\t\n"] {
        let (status, body) = send(&app, Method::POST, "/", Some(json!({ "name": name }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "name {name:?}: {body}");
        assert_eq!(body["error"], "team name is required");
    }
    assert!(
        store.list().await.unwrap().is_empty(),
        "a rejected create persisted a row"
    );
    let (status, body) = send(&app, Method::GET, "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["teams"], json!([]));
}

#[tokio::test]
async fn valid_create_returns_201_with_the_persisted_team() {
    let (app, store) = app();
    let (status, body) = send(
        &app,
        Method::POST,
        "/",
        Some(json!({ "name": "Research", "members": ["a", "b"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["team"]["name"], "Research");
    assert_eq!(body["team"]["members"], json!(["a", "b"]));
    let id = body["team"]["id"].as_str().expect("an id");

    let stored = store
        .get(id)
        .await
        .unwrap()
        .expect("the team was persisted");
    assert_eq!(stored.name, "Research");
    let (status, fetched) = send(&app, Method::GET, &format!("/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["team"]["id"], json!(id));
}

#[tokio::test]
async fn unknown_id_is_404_naming_the_id_on_get_patch_and_delete() {
    let (app, store) = app();
    // A real team exists so a handler that ignored the id would not 404.
    let real = create(&app, "Existing", &["a"]).await;
    let missing = "no-such-team";
    let uri = format!("/{missing}");
    for (method, body) in [
        (Method::GET, None),
        (Method::PATCH, Some(json!({ "name": "Renamed" }))),
        (Method::DELETE, None),
    ] {
        let (status, response) = send(&app, method.clone(), &uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {response}");
        let error = response["error"].as_str().expect("an error message");
        assert!(
            error.contains(missing),
            "{method}: error omits the id: {error}"
        );
    }
    let untouched = store.get(&real).await.unwrap().expect("still there");
    assert_eq!(untouched.name, "Existing");
}

#[tokio::test]
async fn patch_and_delete_act_on_an_existing_team() {
    let (app, store) = app();
    let id = create(&app, "Before", &["a"]).await;
    let (status, body) = send(
        &app,
        Method::PATCH,
        &format!("/{id}"),
        Some(json!({ "name": "After" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["team"]["name"], "After");
    assert_eq!(store.get(&id).await.unwrap().unwrap().name, "After");

    let (status, body) = send(&app, Method::DELETE, &format!("/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["success"], true);
    assert!(store.get(&id).await.unwrap().is_none());
    let (status, _) = send(&app, Method::DELETE, &format!("/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a second delete must 404");
}

#[tokio::test]
async fn add_member_appends_and_a_re_add_is_idempotent() {
    let (app, store) = app();
    let id = create(&app, "Crew", &["a"]).await;
    let uri = format!("/{id}/members");
    for _ in 0..2 {
        let (status, body) = send(&app, Method::POST, &uri, Some(json!({ "agent_id": "b" }))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["team"]["members"], json!(["a", "b"]));
    }
    let stored = store.get(&id).await.unwrap().unwrap();
    assert_eq!(stored.members, ["a", "b"]);
}

#[tokio::test]
async fn remove_member_drops_the_agent_and_persists() {
    let (app, store) = app();
    let id = create(&app, "Crew", &["a", "b", "c"]).await;
    let (status, body) = send(&app, Method::DELETE, &format!("/{id}/members/b"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["team"]["members"], json!(["a", "c"]));
    assert_eq!(store.get(&id).await.unwrap().unwrap().members, ["a", "c"]);
}

#[tokio::test]
async fn member_endpoints_404_for_an_unknown_team() {
    let (app, store) = app();
    let (status, body) = send(
        &app,
        Method::POST,
        "/ghost/members",
        Some(json!({ "agent_id": "a" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"].as_str().unwrap().contains("ghost"), "{body}");

    let (status, body) = send(&app, Method::DELETE, "/ghost/members/a", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"].as_str().unwrap().contains("ghost"), "{body}");
    assert!(
        store.list().await.unwrap().is_empty(),
        "a member call minted a team"
    );
}
