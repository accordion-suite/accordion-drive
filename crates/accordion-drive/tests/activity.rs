//! Activity and notifications through the JSON API (decision 18).

mod common;

use axum::http::StatusCode;
use common::{Person, Reply, Server, start};
use serde_json::{Value, json};

fn ok(reply: Reply) -> Value {
    assert!(
        reply.status.is_success(),
        "{}: {}",
        reply.status,
        reply.text()
    );
    if reply.body.is_empty() {
        Value::Null
    } else {
        reply.json()
    }
}

async fn top(server: &Server, person: &Person) -> String {
    ok(server.api(person, "GET", "/top", None).await)["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn folder(server: &Server, person: &Person, parent: &str, name: &str) -> String {
    ok(server
        .api(
            person,
            "POST",
            &format!("/items/{parent}/children"),
            Some(json!({ "name": name })),
        )
        .await)["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn put_file(server: &Server, person: &Person, folder: &str, name: &str) -> String {
    ok(server
        .send(
            "PUT",
            &format!("/api/drive/items/{folder}/children/{name}"),
            &[("authorization", &person.bearer)],
            "contents",
        )
        .await)["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn activity(server: &Server, person: &Person, item: &str) -> Vec<Value> {
    ok(server
        .api(person, "GET", &format!("/items/{item}/activity"), None)
        .await)["events"]
        .as_array()
        .unwrap()
        .clone()
}

fn actions(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["action"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn activity_names_people_and_folders_the_reader_can_open() {
    let server = start().await;
    let (alex, sam) = (&server.alex, &server.sam);
    let top = top(&server, alex).await;
    let holiday = folder(&server, alex, &top, "Holiday").await;
    let private = folder(&server, alex, &top, "Private").await;
    let photo = put_file(&server, alex, &private, "beach.jpg").await;
    ok(server
        .api(
            alex,
            "PUT",
            &format!("/items/{holiday}/shares/{}", sam.id),
            Some(json!({ "level": "editor" })),
        )
        .await);
    ok(server
        .api(
            alex,
            "PATCH",
            &format!("/items/{photo}"),
            Some(json!({ "parent": holiday, "name": "beach.jpg" })),
        )
        .await);
    ok(server
        .api(
            alex,
            "PUT",
            &format!("/items/{photo}/description"),
            Some(json!({ "description": "The first day." })),
        )
        .await);
    // Saving the same description again changes nothing.
    ok(server
        .api(
            alex,
            "PUT",
            &format!("/items/{photo}/description"),
            Some(json!({ "description": "The first day." })),
        )
        .await);

    let alexs = activity(&server, alex, &photo).await;
    assert_eq!(actions(&alexs), ["description_changed", "moved", "added"]);
    assert_eq!(alexs[1]["actor"]["name"], "alex");
    assert_eq!(alexs[1]["via"], "app");
    assert_eq!(alexs[1]["item"]["name"], "beach.jpg");
    assert_eq!(alexs[1]["details"]["from"]["name"], "Private");
    assert_eq!(alexs[1]["details"]["to"]["name"], "Holiday");

    // Sam can't open Private, so it isn't named.
    let sams = activity(&server, sam, &photo).await;
    assert_eq!(sams[1]["details"]["from"], Value::Null);
    assert_eq!(sams[1]["details"]["to"]["name"], "Holiday");

    // A change over WebDAV says so.
    let reply = server
        .shared_dav(sam, "PUT", "/Holiday/notes.txt", &[], "hello")
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let latest = &activity(&server, alex, &holiday).await[0];
    assert_eq!(latest["action"], "added");
    assert_eq!(latest["via"], "webdav");
    assert_eq!(latest["actor"]["name"], "sam");
    assert_eq!(latest["item"]["name"], "notes.txt");

    // Lee has no access.
    let reply = server
        .api(
            &server.lee,
            "GET",
            &format!("/items/{photo}/activity"),
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn notifications_group_changes_and_mark_read() {
    let server = start().await;
    let (alex, sam) = (&server.alex, &server.sam);
    let top = top(&server, alex).await;
    let holiday = folder(&server, alex, &top, "Holiday").await;
    ok(server
        .api(
            alex,
            "PUT",
            &format!("/items/{holiday}/shares/{}", sam.id),
            Some(json!({ "level": "editor" })),
        )
        .await);

    let sams = ok(server.api(sam, "GET", "/notifications", None).await);
    assert_eq!(sams["new"], 1);
    let shared = &sams["notifications"][0];
    assert_eq!(shared["action"], "shared");
    assert_eq!(shared["actor"]["name"], "alex");
    assert_eq!(shared["item"]["name"], "Holiday");
    assert_eq!(shared["details"]["level"], "editor");

    for name in ["one.jpg", "two.jpg", "three.jpg"] {
        put_file(&server, sam, &holiday, name).await;
    }
    let alexs = ok(server.api(alex, "GET", "/notifications", None).await);
    assert_eq!(alexs["new"], 1);
    let added = &alexs["notifications"][0];
    assert_eq!(added["action"], "added");
    assert_eq!(added["count"], 3);
    assert_eq!(added["ids"].as_array().unwrap().len(), 3);
    assert_eq!(added["item"]["name"], "one.jpg");
    assert_eq!(added["folder"]["name"], "Holiday");

    // Opening the list clears the count, and the group stays highlighted
    // until it's selected.
    ok(server
        .api(
            alex,
            "POST",
            "/notifications/seen",
            Some(json!({ "up_to": added["id"] })),
        )
        .await);
    let alexs = ok(server.api(alex, "GET", "/notifications", None).await);
    assert_eq!(alexs["new"], 0);
    assert_eq!(alexs["notifications"][0]["seen"], true);
    assert_eq!(alexs["notifications"][0]["read"], false);
    let ids = alexs["notifications"][0]["ids"].clone();
    ok(server
        .api(
            alex,
            "POST",
            "/notifications/read",
            Some(json!({ "ids": ids })),
        )
        .await);
    let alexs = ok(server.api(alex, "GET", "/notifications", None).await);
    assert_eq!(alexs["notifications"][0]["read"], true);

    // Removing takes the whole group away, for Alex only.
    ok(server
        .api(
            alex,
            "POST",
            "/notifications/remove",
            Some(json!({ "ids": ids })),
        )
        .await);
    let alexs = ok(server.api(alex, "GET", "/notifications", None).await);
    assert_eq!(alexs["notifications"], json!([]));

    // Clearing all keeps nothing up to what was shown.
    let newest = sams["notifications"][0]["id"].clone();
    ok(server
        .api(
            sam,
            "DELETE",
            &format!("/notifications?up_to={newest}"),
            None,
        )
        .await);
    let sams = ok(server.api(sam, "GET", "/notifications", None).await);
    assert_eq!(sams["notifications"], json!([]));
}
