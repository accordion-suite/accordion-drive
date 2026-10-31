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
async fn previews_images_and_pdfs() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut files = Vec::new();
    for name in ["Beach.JPG", "Lease.pdf", "notes.txt", "logo.svg"] {
        let file = server
            .send(
                "PUT",
                &format!("/api/drive/items/{top}/children/{name}"),
                &[("authorization", &alex.bearer)],
                "contents",
            )
            .await
            .json();
        files.push((
            file["id"].as_str().unwrap().to_owned(),
            file["preview"].clone(),
        ));
    }
    let previews: Vec<_> = files.iter().map(|(_, preview)| preview.clone()).collect();
    assert_eq!(
        previews,
        [json!("image"), json!("pdf"), json!(null), json!(null)]
    );
    let top_folder = server.api(alex, "GET", "/top", None).await.json();
    assert_eq!(top_folder["preview"], json!(null));

    let image = server
        .api(alex, "GET", &format!("/items/{}/preview", files[0].0), None)
        .await;
    assert_eq!(image.status, StatusCode::OK);
    assert_eq!(image.text(), "contents");
    assert_eq!(image.headers["content-type"], "image/jpeg");
    assert_eq!(image.headers["x-content-type-options"], "nosniff");
    assert_eq!(
        image.headers["content-security-policy"],
        "sandbox; frame-ancestors 'self'"
    );
    assert_eq!(
        image.headers["content-disposition"],
        "inline; filename=\"Beach.JPG\"; filename*=UTF-8''Beach.JPG"
    );

    let pdf = server
        .api(alex, "GET", &format!("/items/{}/preview", files[1].0), None)
        .await;
    assert_eq!(pdf.status, StatusCode::OK);
    assert_eq!(pdf.headers["content-type"], "application/pdf");
    assert_eq!(pdf.headers["x-content-type-options"], "nosniff");
    assert_eq!(
        pdf.headers["content-security-policy"],
        "frame-ancestors 'self'"
    );

    for (id, _) in &files[2..] {
        let refused = server
            .api(alex, "GET", &format!("/items/{id}/preview"), None)
            .await;
        assert_eq!(refused.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            refused.json()["error"],
            "Accordion can't show this kind of file. Download it instead."
        );
    }
    let folder = server
        .api(alex, "GET", &format!("/items/{top}/preview"), None)
        .await;
    assert_eq!(folder.status, StatusCode::CONFLICT);
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
        ("GET", format!("/items/{file}/preview"), None),
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

/// Saves `name` with `text` in `folder`, and returns its ID.
async fn write(server: &common::Server, folder: &str, name: &str, text: &str) -> String {
    let reply = server
        .send(
            "PUT",
            &format!("/api/drive/items/{folder}/children/{name}"),
            &[("authorization", &server.alex.bearer)],
            text.to_owned(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    reply.json()["id"].as_str().unwrap().to_owned()
}

fn names(list: &serde_json::Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| {
            let mut path: Vec<String> = item["folders"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap().to_owned())
                .collect();
            path.push(item["name"].as_str().unwrap().to_owned());
            path.join("/")
        })
        .collect()
}

#[tokio::test]
async fn stars_items() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let photos = server
        .api(
            alex,
            "POST",
            &format!("/items/{top}/children"),
            Some(json!({ "name": "Photos" })),
        )
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let beach = write(&server, &photos, "Beach.jpg", "sand").await;
    let notes = write(&server, &top, "Notes.txt", "hi").await;

    for id in [&beach, &photos, &notes] {
        let reply = server
            .api(alex, "PUT", &format!("/items/{id}/star"), None)
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        // Times count milliseconds, so this keeps the stars in order.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Starring twice changes nothing.
    server
        .api(alex, "PUT", &format!("/items/{beach}/star"), None)
        .await;
    let starred = server.api(alex, "GET", "/starred", None).await.json();
    assert_eq!(names(&starred), ["Notes.txt", "Photos", "Photos/Beach.jpg"]);
    assert_eq!(starred[0]["starred"], true);
    let listed = server
        .api(alex, "GET", &format!("/items/{top}/children"), None)
        .await
        .json();
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["starred"] == true)
    );

    // A starred item in the trash leaves the list until it's restored.
    server
        .api(alex, "DELETE", &format!("/items/{photos}"), None)
        .await;
    assert_eq!(
        names(&server.api(alex, "GET", "/starred", None).await.json()),
        ["Notes.txt"]
    );
    server
        .api(alex, "POST", &format!("/items/{photos}/restore"), None)
        .await;
    assert_eq!(
        server
            .api(alex, "GET", "/starred", None)
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let reply = server
        .api(alex, "DELETE", &format!("/items/{notes}/star"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let item = server
        .api(alex, "GET", &format!("/items/{notes}"), None)
        .await
        .json();
    assert_eq!(item["starred"], false);

    // Nobody else can star someone's items, or see their stars.
    let reply = server
        .api(&server.sam, "PUT", &format!("/items/{beach}/star"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let theirs = server
        .api(&server.sam, "GET", "/starred", None)
        .await
        .json();
    assert!(theirs.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn lists_recent_files_and_searches() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let taxes = server
        .api(
            alex,
            "POST",
            &format!("/items/{top}/children"),
            Some(json!({ "name": "Taxes" })),
        )
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    write(&server, &taxes, "Return.pdf", "pdf").await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    write(&server, &top, "Tax%20notes.txt", "notes").await;

    let recent = server.api(alex, "GET", "/recent", None).await.json();
    assert_eq!(names(&recent), ["Tax notes.txt", "Taxes/Return.pdf"]);

    let search = |query: &'static str| server.api(alex, "GET", query, None);
    assert_eq!(
        names(&search("/search?q=tax").await.json()),
        ["Tax notes.txt", "Taxes"]
    );
    assert_eq!(
        names(&search("/search?type=pdfs").await.json()),
        ["Taxes/Return.pdf"]
    );
    assert_eq!(
        names(&search("/search?q=tax&type=folders").await.json()),
        ["Taxes"]
    );
    let later = search("/search?after=2999-01-01T00:00:00.000Z")
        .await
        .json();
    assert!(later.as_array().unwrap().is_empty());
    let earlier = search("/search?before=2999-01-01T00:00:00.000Z")
        .await
        .json();
    assert_eq!(earlier.as_array().unwrap().len(), 3);

    let reply = search("/search?type=spells").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = search("/search?after=yesterday").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let theirs = server
        .api(&server.sam, "GET", "/search?q=tax", None)
        .await
        .json();
    assert!(theirs.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn copies_files() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let beach = write(&server, &top, "Beach.jpg", "sand").await;

    let reply = server
        .api(
            alex,
            "POST",
            &format!("/items/{beach}/copy"),
            Some(json!({ "name": "Copy of Beach.jpg" })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let copy = reply.json();
    assert_eq!(copy["parent"], top.as_str());
    assert_ne!(copy["id"], beach.as_str());
    let content = server
        .api(
            alex,
            "GET",
            &format!("/items/{}/content", copy["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(content.text(), "sand");

    let taken = server
        .api(
            alex,
            "POST",
            &format!("/items/{beach}/copy"),
            Some(json!({ "name": "Beach.jpg" })),
        )
        .await;
    assert_eq!(taken.status, StatusCode::CONFLICT);
    let folder = server
        .api(
            alex,
            "POST",
            &format!("/items/{top}/copy"),
            Some(json!({ "name": "Top" })),
        )
        .await;
    assert_eq!(folder.status, StatusCode::CONFLICT);
    let theirs = server
        .api(
            &server.sam,
            "POST",
            &format!("/items/{beach}/copy"),
            Some(json!({ "name": "Mine.jpg" })),
        )
        .await;
    assert_eq!(theirs.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn keeps_each_persons_settings() {
    let server = start().await;
    let (alex, sam) = (&server.alex, &server.sam);
    let defaults = json!({ "start_page": "home", "thumbnails": true });
    assert_eq!(
        server.api(alex, "GET", "/settings", None).await.json(),
        defaults
    );

    let changed = json!({ "start_page": "my-drive", "thumbnails": false });
    let reply = server
        .api(alex, "PUT", "/settings", Some(changed.clone()))
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(
        server.api(alex, "GET", "/settings", None).await.json(),
        changed
    );
    // Changing them again replaces the row rather than adding one.
    let again = json!({ "start_page": "home", "thumbnails": false });
    server
        .api(alex, "PUT", "/settings", Some(again.clone()))
        .await;
    assert_eq!(
        server.api(alex, "GET", "/settings", None).await.json(),
        again
    );
    // Someone else's are their own.
    assert_eq!(
        server.api(sam, "GET", "/settings", None).await.json(),
        defaults
    );

    for wrong in [
        json!({ "start_page": "trash", "thumbnails": true }),
        json!({ "start_page": "home" }),
    ] {
        let reply = server.api(alex, "PUT", "/settings", Some(wrong)).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}
