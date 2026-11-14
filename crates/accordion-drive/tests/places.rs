//! Apps' places beside My Drive, and moves that would share something
//! (decision 32).

mod common;

use accordion_core::storage::AppPlace;
use axum::body::Body;
use axum::http::StatusCode;
use common::{Person, Server, start};
use serde_json::{Value, json};

const PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><allprop/></propfind>"#;

async fn with_notes() -> Server {
    let server = start().await;
    server.storage.add_place(AppPlace {
        id: "notes",
        name: "Notes",
    });
    server
}

fn id(value: &Value) -> String {
    value["id"].as_str().unwrap().to_owned()
}

async fn folder(server: &Server, person: &Person, parent: &str, name: &str) -> String {
    let reply = server
        .api(
            person,
            "POST",
            &format!("/items/{parent}/children"),
            Some(json!({ "name": name })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    id(&reply.json())
}

async fn file(server: &Server, person: &Person, parent: &str, name: &str) -> String {
    let reply = server
        .send(
            "PUT",
            &format!("/api/drive/items/{parent}/children/{name}"),
            &[("authorization", &person.bearer)],
            "contents",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    id(&reply.json())
}

async fn share(server: &Server, owner: &Person, item: &str, with: &Person) {
    let reply = server
        .api(
            owner,
            "PUT",
            &format!("/items/{item}/shares/{}", with.id),
            Some(json!({ "level": "viewer" })),
        )
        .await;
    assert!(reply.status.is_success(), "{}", reply.text());
}

#[tokio::test]
async fn lists_places_and_makes_one_when_asked() {
    let server = with_notes().await;
    let alex = &server.alex;

    // Listing doesn't make anything.
    let places = server.api(alex, "GET", "/places", None).await.json();
    assert_eq!(
        places,
        json!([{ "place": "notes", "name": "Notes", "id": null, "empty": true }])
    );

    let notes = server.api(alex, "GET", "/places/notes", None).await.json();
    assert_eq!(notes["place"], "notes");
    assert_eq!(notes["name"], "Notes");
    assert_eq!(notes["parent"], Value::Null);
    let notes = id(&notes);
    let top = server.api(alex, "GET", "/top", None).await.json();
    assert_eq!(top["place"], "drive");
    assert_ne!(id(&top), notes);
    assert_eq!(
        id(&server.api(alex, "GET", "/places/drive", None).await.json()),
        id(&top)
    );

    file(&server, alex, &notes, "Soup.md").await;
    let places = server.api(alex, "GET", "/places", None).await.json();
    assert_eq!(places[0]["id"], notes.as_str());
    assert_eq!(places[0]["empty"], false);

    // Recent says which place a file is in.
    let recent = server.api(alex, "GET", "/recent", None).await.json();
    assert_eq!(recent[0]["name"], "Soup.md");
    assert_eq!(recent[0]["place"], "notes");

    // An app Accordion doesn't have.
    let reply = server.api(alex, "GET", "/places/sheets", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // Each person's own.
    let theirs = server
        .api(&server.sam, "GET", "/places/notes", None)
        .await
        .json();
    assert_ne!(id(&theirs), notes);
}

#[tokio::test]
async fn a_move_that_shares_asks_first() {
    let server = with_notes().await;
    let (alex, sam, kim) = (&server.alex, &server.sam, &server.kim);
    let top = id(&server.api(alex, "GET", "/top", None).await.json());
    let notes = id(&server.api(alex, "GET", "/places/notes", None).await.json());
    let garden = folder(&server, alex, &top, "Garden").await;
    share(&server, alex, &garden, sam).await;
    share(&server, alex, &garden, kim).await;
    let plan = file(&server, alex, &notes, "Plan.md").await;

    let check = server
        .api(
            alex,
            "POST",
            "/move-check",
            Some(json!({ "items": [plan], "into": garden })),
        )
        .await
        .json();
    let people: Vec<_> = check["people"]
        .as_array()
        .unwrap()
        .iter()
        .map(|person| person["user"].clone())
        .collect();
    assert_eq!(people.len(), 2);
    assert!(people.contains(&json!(sam.id)) && people.contains(&json!(kim.id)));
    assert_eq!(check["links"], 0);

    // Naming only Sam isn't enough.
    let refused = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{plan}"),
            Some(json!({ "parent": garden, "allow": [sam.id] })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    let refused = refused.json();
    assert_eq!(refused["code"], "would_share");
    assert_eq!(refused["people"].as_array().unwrap().len(), 2);

    // Naming both moves it.
    let moved = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{plan}"),
            Some(json!({ "parent": garden, "allow": [sam.id, kim.id] })),
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.text());
    assert_eq!(moved.json()["parent"], garden.as_str());
    assert_eq!(moved.json()["lost"], json!([]));

    // Moving it back out doesn't ask, and says who can't open it anymore.
    let back = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{plan}"),
            Some(json!({ "parent": notes, "allow": [] })),
        )
        .await
        .json();
    assert_eq!(back["lost"].as_array().unwrap().len(), 2);

    // Redo names the same people. Someone added to Garden since is asked
    // about.
    share(&server, alex, &garden, &server.lee).await;
    let refused = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{plan}"),
            Some(json!({ "parent": garden, "allow": [sam.id, kim.id] })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json()["people"].as_array().unwrap().len(), 3);

    // A device that can't ask leaves `allow` out, and the move goes ahead.
    let moved = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{plan}"),
            Some(json!({ "parent": garden })),
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK);

    // A rename alone never asks.
    let renamed = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{plan}"),
            Some(json!({ "name": "Garden plan.md", "allow": [] })),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::OK);
}

#[tokio::test]
async fn a_move_under_a_link_asks_about_the_link() {
    let server = with_notes().await;
    let alex = &server.alex;
    let top = id(&server.api(alex, "GET", "/top", None).await.json());
    let photos = folder(&server, alex, &top, "Photos").await;
    let reply = server
        .api(
            alex,
            "POST",
            &format!("/items/{photos}/links"),
            Some(json!({ "expires": "1d" })),
        )
        .await;
    assert!(reply.status.is_success(), "{}", reply.text());
    let beach = file(&server, alex, &top, "beach.jpg").await;

    let check = server
        .api(
            alex,
            "POST",
            "/move-check",
            Some(json!({ "items": [beach], "into": photos })),
        )
        .await
        .json();
    assert_eq!(check, json!({ "people": [], "links": 1 }));

    let refused = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{beach}"),
            Some(json!({ "parent": photos, "allow": [] })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json()["links"], 1);
    let moved = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{beach}"),
            Some(json!({ "parent": photos, "allow": [], "allow_links": true })),
        )
        .await;
    assert_eq!(moved.status, StatusCode::OK);
}

#[tokio::test]
async fn webdav_has_a_folder_for_each_place() {
    let server = with_notes().await;
    let alex = &server.alex;
    let notes = id(&server.api(alex, "GET", "/places/notes", None).await.json());
    file(&server, alex, &notes, "Soup.md").await;
    let basic = alex.basic.as_str();

    let reply = server
        .send(
            "PROPFIND",
            "/dav/places/",
            &[("authorization", basic), ("depth", "1")],
            PROPFIND,
        )
        .await;
    assert_eq!(reply.status, StatusCode::MULTI_STATUS);
    assert!(
        reply.text().contains("/dav/places/Notes/"),
        "{}",
        reply.text()
    );

    let reply = server
        .send(
            "PUT",
            "/dav/places/Notes/Bread.md",
            &[("authorization", basic), ("content-length", "5")],
            "flour",
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let reply = server
        .send(
            "GET",
            "/dav/places/Notes/Soup.md",
            &[("authorization", basic)],
            Body::empty(),
        )
        .await;
    assert_eq!(reply.text(), "contents");

    // Nothing goes at the top, beside the places.
    let reply = server
        .send(
            "PUT",
            "/dav/places/Stray.md",
            &[("authorization", basic), ("content-length", "1")],
            "x",
        )
        .await;
    assert!(reply.status.is_client_error(), "{}", reply.status);

    // My Drive doesn't show Notes.
    let reply = server
        .send(
            "PROPFIND",
            "/dav/drive/",
            &[("authorization", basic), ("depth", "1")],
            PROPFIND,
        )
        .await;
    assert!(!reply.text().contains("Soup.md"));
}
