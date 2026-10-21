//! The JSON API that Accordion's apps use.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::start;
use serde_json::json;

#[tokio::test]
async fn manages_files_and_folders() {
    let server = start().await;
    let alex = &server.alex;

    let top = server.api(alex, "GET", "/top", None).await.json();
    assert_eq!(top["kind"], "folder");
    assert_eq!(top["parent"], serde_json::Value::Null);
    let top = top["id"].as_str().unwrap().to_owned();

    let reply = server
        .api(
            alex,
            "POST",
            &format!("/items/{top}/children"),
            Some(json!({ "name": "Documents" })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let docs = reply.json()["id"].as_str().unwrap().to_owned();

    let reply = server
        .send(
            "PUT",
            &format!("/api/drive/items/{docs}/children/Report%20%C3%A9t%C3%A9.txt"),
            &[("authorization", &alex.bearer)],
            "first draft",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let report = reply.json();
    assert_eq!(report["name"], "Report été.txt");
    assert_eq!(report["size"], 11);
    let report = report["id"].as_str().unwrap().to_owned();

    let listed = server
        .api(alex, "GET", &format!("/items/{docs}/children"), None)
        .await
        .json();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["id"], report.as_str());

    let download = server
        .api(alex, "GET", &format!("/items/{report}/content"), None)
        .await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.text(), "first draft");
    assert_eq!(download.headers["content-type"], "application/octet-stream");
    assert_eq!(download.headers["x-content-type-options"], "nosniff");
    assert_eq!(
        download.headers["content-disposition"],
        "attachment; filename=\"Report _t_.txt\"; filename*=UTF-8''Report%20%C3%A9t%C3%A9.txt"
    );

    // Renaming and moving.
    let moved = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{report}"),
            Some(json!({ "parent": top, "name": "Summer report.txt" })),
        )
        .await
        .json();
    assert_eq!(moved["parent"], top.as_str());
    assert_eq!(moved["name"], "Summer report.txt");
    let renamed = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{docs}"),
            Some(json!({ "name": "Papers" })),
        )
        .await
        .json();
    assert_eq!(renamed["name"], "Papers");
    assert_eq!(renamed["parent"], top.as_str());

    let refused = server
        .api(
            alex,
            "PATCH",
            &format!("/items/{top}"),
            Some(json!({ "name": "x" })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert!(
        refused.json()["error"]
            .as_str()
            .unwrap()
            .contains("top folder")
    );
}

#[tokio::test]
async fn keeps_versions() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let put = async |text: &'static str| {
        server
            .send(
                "PUT",
                &format!("/api/drive/items/{top}/children/notes.md"),
                &[("authorization", &alex.bearer)],
                text,
            )
            .await
    };
    let file = put("one").await.json()["id"].as_str().unwrap().to_owned();
    put("two").await;

    let versions = server
        .api(alex, "GET", &format!("/items/{file}/versions"), None)
        .await
        .json();
    assert_eq!(versions.as_array().unwrap().len(), 2);
    assert_eq!(versions[0]["number"], 2);
    let first = versions[1]["id"].as_str().unwrap();

    let old = server
        .api(
            alex,
            "GET",
            &format!("/items/{file}/versions/{first}/content"),
            None,
        )
        .await;
    assert_eq!(old.text(), "one");

    server
        .api(
            alex,
            "POST",
            &format!("/items/{file}/versions/{first}/restore"),
            None,
        )
        .await;
    let current = server
        .api(alex, "GET", &format!("/items/{file}/content"), None)
        .await;
    assert_eq!(current.text(), "one");
}

#[tokio::test]
async fn deletes_to_the_trash() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let folder = server
        .api(
            alex,
            "POST",
            &format!("/items/{top}/children"),
            Some(json!({ "name": "Old" })),
        )
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let trashed = server
        .api(alex, "DELETE", &format!("/items/{folder}"), None)
        .await
        .json();
    assert!(trashed["trashed_at"].is_string());
    let trash = server.api(alex, "GET", "/trash", None).await.json();
    assert_eq!(trash[0]["id"], folder.as_str());

    server
        .api(alex, "POST", &format!("/items/{folder}/restore"), None)
        .await;
    assert_eq!(
        server.api(alex, "GET", "/trash", None).await.json(),
        json!([])
    );

    server
        .api(alex, "DELETE", &format!("/items/{folder}"), None)
        .await;
    let forever = server
        .api(alex, "DELETE", &format!("/trash/{folder}"), None)
        .await;
    assert_eq!(forever.status, StatusCode::NO_CONTENT);
    let gone = server
        .api(alex, "GET", &format!("/items/{folder}"), None)
        .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    let emptied = server.api(alex, "DELETE", "/trash", None).await;
    assert_eq!(emptied.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn resumes_uploads() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let reply = server
        .api(alex, "POST", "/uploads", Some(json!({ "size": 11 })))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let upload = reply.json()["id"].as_str().unwrap().to_owned();
    let path = format!("/api/drive/uploads/{upload}");

    let piece = async |offset: &'static str, text: &'static str| {
        server
            .send(
                "PATCH",
                &path,
                &[("authorization", &alex.bearer), ("upload-offset", offset)],
                text,
            )
            .await
    };
    assert_eq!(piece("0", "hello ").await.json()["received"], 6);
    let wrong = piece("0", "again").await;
    assert_eq!(wrong.status, StatusCode::CONFLICT);
    let missing = server
        .send("PATCH", &path, &[("authorization", &alex.bearer)], "world")
        .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);

    let progress = server
        .api(alex, "GET", &format!("/uploads/{upload}"), None)
        .await
        .json();
    assert_eq!(progress["received"], 6);
    piece("6", "world").await;

    let file = server
        .api(
            alex,
            "POST",
            &format!("/uploads/{upload}/finish"),
            Some(json!({ "parent": top, "name": "hello.txt" })),
        )
        .await
        .json();
    assert_eq!(file["size"], 11);
    let id = file["id"].as_str().unwrap();
    let contents = server
        .api(alex, "GET", &format!("/items/{id}/content"), None)
        .await;
    assert_eq!(contents.text(), "hello world");

    let usage = server.api(alex, "GET", "/usage", None).await.json();
    assert_eq!(usage, json!({ "used": 11, "uploading": 0, "quota": null }));
}

#[tokio::test]
async fn nobody_reaches_anyone_elses_files() {
    let server = start().await;
    let (alex, sam) = (&server.alex, &server.sam);
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let file = server
        .send(
            "PUT",
            &format!("/api/drive/items/{top}/children/secret.txt"),
            &[("authorization", &alex.bearer)],
            "private",
        )
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let upload = server
        .api(alex, "POST", "/uploads", Some(json!({ "size": 5 })))
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let sams_top = server.api(sam, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    for (method, path, body) in [
        ("GET", format!("/items/{file}"), None),
        ("GET", format!("/items/{file}/content"), None),
        ("GET", format!("/items/{top}/children"), None),
        ("DELETE", format!("/items/{file}"), None),
        (
            "PATCH",
            format!("/items/{file}"),
            Some(json!({ "parent": sams_top })),
        ),
        (
            "PATCH",
            format!("/items/{sams_top}"),
            Some(json!({ "parent": top })),
        ),
        ("GET", format!("/uploads/{upload}"), None),
        ("DELETE", format!("/uploads/{upload}"), None),
    ] {
        let reply = server.api(sam, method, &path, body).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {path}");
    }
    let put = server
        .send(
            "PUT",
            &format!("/api/drive/items/{top}/children/planted.txt"),
            &[("authorization", &sam.bearer)],
            "boo",
        )
        .await;
    assert_eq!(put.status, StatusCode::NOT_FOUND);

    let signed_out = server
        .send("GET", "/api/drive/top", &[], Body::empty())
        .await;
    assert_eq!(signed_out.status, StatusCode::UNAUTHORIZED);
}
