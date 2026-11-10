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
async fn previews_what_browsers_show_themselves() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut files = Vec::new();
    for name in [
        "Beach.JPG",
        "Lease.pdf",
        "Holiday.MOV",
        "Clip.webm",
        "Song.mp3",
        "notes.txt",
        "logo.svg",
        // Browsers can't play a camcorder's files.
        "Camcorder.mts",
    ] {
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
        [
            json!("image"),
            json!("pdf"),
            json!("video"),
            json!("video"),
            json!("audio"),
            json!(null),
            json!(null),
            json!(null)
        ]
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

    for (index, content_type) in [(2, "video/mp4"), (3, "video/webm"), (4, "audio/mpeg")] {
        let video = server
            .api(
                alex,
                "GET",
                &format!("/items/{}/preview", files[index].0),
                None,
            )
            .await;
        assert_eq!(video.status, StatusCode::OK);
        assert_eq!(video.headers["content-type"], content_type);
        assert_eq!(video.headers["accept-ranges"], "bytes");
        assert_eq!(
            video.headers["content-security-policy"],
            "sandbox; frame-ancestors 'self'"
        );
    }

    for (id, _) in &files[5..] {
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
async fn says_how_long_trash_and_versions_are_kept() {
    let server = start().await;
    let retention = server
        .api(&server.alex, "GET", "/retention", None)
        .await
        .json();
    assert_eq!(retention, json!({ "trash_days": 30, "version_days": 30 }));
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
#[tokio::test]
async fn sends_the_part_of_a_file_a_player_asks_for() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = write(&server, &top, "digits.txt", "0123456789").await;
    let etag = server
        .api(alex, "GET", &format!("/items/{id}/content"), None)
        .await
        .headers["etag"]
        .to_str()
        .unwrap()
        .to_owned();
    let get = |range: &'static str, if_range: Option<String>| {
        let path = format!("/api/drive/items/{id}/content");
        let server = &server;
        async move {
            let mut headers = vec![("authorization", alex.bearer.as_str()), ("range", range)];
            let if_range = if_range.unwrap_or_default();
            if !if_range.is_empty() {
                headers.push(("if-range", &if_range));
            }
            server.send("GET", &path, &headers, "").await
        }
    };

    for (range, part, content_range) in [
        ("bytes=2-5", "2345", "bytes 2-5/10"),
        ("bytes=7-", "789", "bytes 7-9/10"),
        ("bytes=-3", "789", "bytes 7-9/10"),
        // Past the end is cut to the end.
        ("bytes=8-100", "89", "bytes 8-9/10"),
        ("bytes=-100", "0123456789", "bytes 0-9/10"),
    ] {
        let reply = get(range, None).await;
        assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(reply.text(), part, "{range}");
        assert_eq!(reply.headers["content-range"], content_range, "{range}");
        assert_eq!(reply.headers["content-length"], part.len().to_string());
    }

    let reply = get("bytes=10-", None).await;
    assert_eq!(reply.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(reply.headers["content-range"], "bytes */10");

    // Several ranges at once, nonsense, and a range of an older version
    // get the whole file.
    for (range, if_range) in [
        ("bytes=0-1,4-5", None),
        ("bytes=5-2", None),
        ("lines=1-2", None),
        ("bytes=2-5", Some("\"an older version\"".to_owned())),
    ] {
        let reply = get(range, if_range).await;
        assert_eq!(reply.status, StatusCode::OK, "{range}");
        assert_eq!(reply.text(), "0123456789", "{range}");
        assert_eq!(reply.headers["accept-ranges"], "bytes");
    }
    let reply = get("bytes=2-5", Some(etag)).await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(reply.text(), "2345");
}

#[tokio::test]
async fn downloads_folders_and_selections_as_a_zip() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let folder = |parent: String, name: &'static str| {
        let server = &server;
        async move {
            server
                .api(
                    alex,
                    "POST",
                    &format!("/items/{parent}/children"),
                    Some(json!({ "name": name })),
                )
                .await
                .json()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let trip = folder(top.clone(), "Trip").await;
    let photos = folder(trip.clone(), "Photos").await;
    folder(trip.clone(), "Empty").await;
    let notes = write(&server, &trip, "notes.txt", "Pack the tent.").await;
    write(&server, &photos, "lake.txt", "A photo of the lake.").await;
    let other_notes = write(&server, &top, "notes.txt", "Pick up the bread.").await;

    let zip = |ids: String| {
        let server = &server;
        async move {
            server
                .api(alex, "GET", &format!("/zip?ids={ids}"), None)
                .await
        }
    };
    let read = |body: &[u8]| {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(body.to_vec())).unwrap();
        (0..archive.len())
            .map(|index| {
                let mut entry = archive.by_index(index).unwrap();
                let mut text = String::new();
                std::io::Read::read_to_string(&mut entry, &mut text).unwrap();
                (entry.name().unwrap().into_owned(), text)
            })
            .collect::<Vec<_>>()
    };

    // A folder, with everything in it, under its own name.
    let reply = zip(trip.clone()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["content-type"], "application/zip");
    assert_eq!(
        reply.headers["content-disposition"],
        "attachment; filename=\"Trip.zip\"; filename*=UTF-8''Trip.zip"
    );
    assert_eq!(
        read(&reply.body),
        [
            ("Trip/".to_owned(), String::new()),
            ("Trip/Empty/".to_owned(), String::new()),
            ("Trip/Photos/".to_owned(), String::new()),
            ("Trip/notes.txt".to_owned(), "Pack the tent.".to_owned()),
            (
                "Trip/Photos/lake.txt".to_owned(),
                "A photo of the lake.".to_owned()
            ),
        ]
    );

    // Two files with the same name, from different folders.
    let reply = zip(format!("{notes},{other_notes}")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.headers["content-disposition"],
        "attachment; filename=\"Drive download.zip\"; filename*=UTF-8''Drive%20download.zip"
    );
    assert_eq!(
        read(&reply.body),
        [
            ("notes.txt".to_owned(), "Pack the tent.".to_owned()),
            ("notes (1).txt".to_owned(), "Pick up the bread.".to_owned()),
        ]
    );

    // Nobody else's files, nothing at all, or nonsense.
    let reply = server
        .api(&server.sam, "GET", &format!("/zip?ids={trip}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(zip(String::new()).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(zip("not-an-id".into()).await.status, StatusCode::NOT_FOUND);

    // Nor what's in the trash.
    server
        .api(alex, "DELETE", &format!("/items/{trip}"), None)
        .await;
    assert_eq!(zip(notes).await.status, StatusCode::NOT_FOUND);
}

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

/// Reads the text of the file with `id` now, for search.
async fn read_text(server: &common::Server, id: &str) {
    let item = server
        .storage
        .item(id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    let accordion_core::storage::ItemKind::File { blob, .. } = item.kind else {
        panic!("{id} isn't a file");
    };
    server.texts.make_now(blob.hash).await.unwrap();
}

#[tokio::test]
async fn searches_inside_files_with_every_filter() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let named = write(&server, &top, "Budget.txt", "Groceries and fuel").await;
    let notes = write(
        &server,
        &top,
        "notes.txt",
        "Plans for the lake. The budget for fuel is tight.",
    )
    .await;
    let photo = write(&server, &top, "Lake.jpg", "not really a photo").await;
    for id in [&named, &notes, &photo] {
        read_text(&server, id).await;
    }
    let search = async |query: &str| {
        let reply = server.api(alex, "GET", query, None).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.text());
        reply.json()
    };

    // Names first, then text, each saying how it matched.
    let found = search("/search?q=budget").await;
    assert_eq!(names(&found), ["Budget.txt", "notes.txt"]);
    assert_eq!(found[0]["matched"], "name");
    assert_eq!(found[0]["snippet"], serde_json::Value::Null);
    assert_eq!(found[1]["matched"], "text");
    let matched: Vec<&str> = found[1]["snippet"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|piece| piece["matched"] == true)
        .map(|piece| piece["text"].as_str().unwrap())
        .collect();
    assert_eq!(matched, ["budget"]);
    assert_eq!(names(&search("/search?words=budget").await), ["notes.txt"]);
    assert_eq!(names(&search("/search?name=budget").await), ["Budget.txt"]);
    assert_eq!(
        names(&search("/search?words=fuel&name=notes").await),
        ["notes.txt"]
    );
    // Without words, nothing says how it matched.
    let found = search("/search?type=photos").await;
    assert_eq!(names(&found), ["Lake.jpg"]);
    assert_eq!(found[0]["matched"], serde_json::Value::Null);

    // Each filter.
    let reply = server
        .api(alex, "PUT", &format!("/items/{notes}/star"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    assert_eq!(
        names(&search("/search?q=fuel&starred=true").await),
        ["notes.txt"]
    );
    assert_eq!(names(&search("/search?q=lake&owner=me").await).len(), 2);
    assert!(names(&search("/search?q=lake&owner=others").await).is_empty());
    assert_eq!(names(&search("/search?q=lake&in=my-drive").await).len(), 2);
    assert!(names(&search("/search?q=lake&in=shared").await).is_empty());
    let reply = server
        .api(
            alex,
            "PUT",
            &format!("/items/{notes}/shares/{}", server.sam.id),
            Some(json!({ "level": "viewer" })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        names(&search("/search?shared_with=sam").await),
        ["notes.txt"]
    );
    assert!(names(&search("/search?shared_with=kim").await).is_empty());
    let sams = server
        .api(&server.sam, "GET", "/search?q=fuel", None)
        .await
        .json();
    assert_eq!(names(&sams), ["notes.txt"]);
    let sams = server
        .api(&server.sam, "GET", "/search?q=fuel&owner=alex", None)
        .await
        .json();
    assert_eq!(names(&sams), ["notes.txt"]);
    let reply = server
        .api(alex, "DELETE", &format!("/items/{photo}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert!(names(&search("/search?q=lake.jpg").await).is_empty());
    assert_eq!(
        names(&search("/search?q=lake.jpg&trash=true").await),
        ["Lake.jpg"]
    );
    let folder = server
        .api(
            alex,
            "POST",
            &format!("/items/{top}/children"),
            Some(json!({ "name": "Trips" })),
        )
        .await
        .json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    write(&server, &folder, "lake%20plan.txt", "x").await;
    assert_eq!(
        names(&search(&format!("/search?q=lake&in={folder}")).await),
        ["Trips/lake plan.txt"]
    );

    for query in [
        "/search?owner=nobody",
        "/search?shared_with=nobody",
        "/search?in=somewhere",
    ] {
        let reply = server.api(alex, "GET", query, None).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
    }
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
    let defaults = json!({ "start_page": "home", "thumbnails": true, "click": "select" });
    assert_eq!(
        server.api(alex, "GET", "/settings", None).await.json(),
        defaults
    );

    let changed = json!({ "start_page": "my-drive", "thumbnails": false, "click": "open" });
    let reply = server
        .api(alex, "PUT", "/settings", Some(changed.clone()))
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(
        server.api(alex, "GET", "/settings", None).await.json(),
        changed
    );
    // Changing them again replaces the row rather than adding one.
    let again = json!({ "start_page": "home", "thumbnails": false, "click": "download" });
    server
        .api(alex, "PUT", "/settings", Some(again.clone()))
        .await;
    assert_eq!(
        server.api(alex, "GET", "/settings", None).await.json(),
        again
    );
    // Apps from before the click setting leave it as it was.
    server
        .api(
            alex,
            "PUT",
            "/settings",
            Some(json!({ "start_page": "home", "thumbnails": true })),
        )
        .await;
    assert_eq!(
        server.api(alex, "GET", "/settings", None).await.json()["click"],
        "download"
    );
    // Someone else's are their own.
    assert_eq!(
        server.api(sam, "GET", "/settings", None).await.json(),
        defaults
    );

    for wrong in [
        json!({ "start_page": "trash", "thumbnails": true }),
        json!({ "start_page": "home" }),
        json!({ "start_page": "home", "thumbnails": true, "click": "delete" }),
    ] {
        let reply = server.api(alex, "PUT", "/settings", Some(wrong)).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[tokio::test]
async fn sends_thumbnails_once_core_makes_them() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // A real photo, made with libvips outside the sandbox.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("beach.jpg");
    let made = std::process::Command::new("vips")
        .args(["xyz", path.to_str().unwrap(), "1600", "1200"])
        .status()
        .unwrap();
    assert!(made.success());
    let photo = server
        .send(
            "PUT",
            &format!("/api/drive/items/{top}/children/beach.jpg"),
            &[("authorization", &alex.bearer)],
            std::fs::read(&path).unwrap(),
        )
        .await
        .json();
    let id = photo["id"].as_str().unwrap().to_owned();
    let notes = write(&server, &top, "notes.txt", "Pick up the bread.").await;

    // Core hasn't made it yet.
    assert_eq!(photo["thumbnail"], json!(null));
    let reply = server
        .api(alex, "GET", &format!("/items/{id}/thumbnail"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    let hash =
        accordion_core::storage::ContentHash::from_hex(photo["sha256"].as_str().unwrap()).unwrap();
    assert!(server.thumbnails.make_now(hash).await.unwrap());
    let item = server
        .api(alex, "GET", &format!("/items/{id}"), None)
        .await
        .json();
    assert_eq!(item["thumbnail"], json!("picture"));
    // Browsers keep thumbnails for a year, so the web app puts the version
    // in the address, and one made again gets a new one.
    let version = item["thumbnail_version"].as_str().unwrap().to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(server.thumbnails.make_now(hash).await.unwrap());
    let again = server
        .api(alex, "GET", &format!("/items/{id}"), None)
        .await
        .json();
    assert_ne!(again["thumbnail_version"], json!(version));
    let reply = server
        .api(alex, "GET", &format!("/items/{id}/thumbnail"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["content-type"], "image/webp");
    assert_eq!(
        reply.headers["cache-control"],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(&reply.body[8..12], b"WEBP");

    // Lists say so too, and files core hasn't reached have none yet.
    let children = server
        .api(alex, "GET", &format!("/items/{top}/children"), None)
        .await
        .json();
    let has: Vec<_> = children
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["name"].clone(), item["thumbnail"].clone()))
        .collect();
    assert_eq!(
        has,
        [
            (json!("beach.jpg"), json!("picture")),
            (json!("notes.txt"), json!(null))
        ]
    );
    let reply = server
        .api(alex, "GET", &format!("/items/{notes}/thumbnail"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // A text file's thumbnail is its first lines, as text.
    let item = server
        .api(alex, "GET", &format!("/items/{notes}"), None)
        .await
        .json();
    let hash =
        accordion_core::storage::ContentHash::from_hex(item["sha256"].as_str().unwrap()).unwrap();
    assert!(server.thumbnails.make_now(hash).await.unwrap());
    let reply = server
        .api(alex, "GET", &format!("/items/{notes}/thumbnail"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(reply.headers["content-security-policy"], "sandbox");
    assert_eq!(&reply.body[..], b"Pick up the bread.");

    // Unless its name says it's something else, such as a song.
    let song = write(&server, &top, "song.mp3", "Not really a song.").await;
    let item = server
        .api(alex, "GET", &format!("/items/{song}"), None)
        .await
        .json();
    let hash =
        accordion_core::storage::ContentHash::from_hex(item["sha256"].as_str().unwrap()).unwrap();
    assert!(server.thumbnails.make_now(hash).await.unwrap());
    let item = server
        .api(alex, "GET", &format!("/items/{song}"), None)
        .await
        .json();
    assert_eq!(item["thumbnail"], json!(null));

    // Nobody else reaches it.
    let reply = server
        .api(&server.sam, "GET", &format!("/items/{id}/thumbnail"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn previews_office_documents_once_core_makes_them() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // A real document, made with LibreOffice outside the sandbox.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("budget.csv");
    std::fs::write(&source, "Month,Food\nJanuary,420\n").unwrap();
    let made = std::process::Command::new("soffice")
        .arg(format!(
            "-env:UserInstallation=file://{}",
            dir.path().join("libreoffice").display()
        ))
        .args([
            "--headless",
            "--norestore",
            "--convert-to",
            "xlsx",
            "--outdir",
        ])
        .arg(dir.path())
        .arg(&source)
        .output()
        .unwrap();
    assert!(made.status.success());
    let budget = server
        .send(
            "PUT",
            &format!("/api/drive/items/{top}/children/Budget%202026.xlsx"),
            &[("authorization", &alex.bearer)],
            std::fs::read(dir.path().join("budget.xlsx")).unwrap(),
        )
        .await
        .json();
    let id = budget["id"].as_str().unwrap().to_owned();

    // Core hasn't made it yet.
    assert_eq!(budget["preview"], json!(null));
    let reply = server
        .api(alex, "GET", &format!("/items/{id}/preview"), None)
        .await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let hash =
        accordion_core::storage::ContentHash::from_hex(budget["sha256"].as_str().unwrap()).unwrap();
    assert!(server.thumbnails.make_now(hash).await.unwrap());
    let item = server
        .api(alex, "GET", &format!("/items/{id}"), None)
        .await
        .json();
    assert_eq!(item["preview"], json!("document"));
    assert_eq!(item["preview_partial"], json!(false));
    assert_eq!(item["thumbnail"], json!("picture"));

    let reply = server
        .api(alex, "GET", &format!("/items/{id}/preview"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["content-type"], "application/pdf");
    assert_eq!(
        reply.headers["content-disposition"],
        "inline; filename=\"Budget 2026.pdf\"; filename*=UTF-8''Budget%202026.pdf"
    );
    assert_eq!(
        reply.headers["content-security-policy"],
        "frame-ancestors 'self'"
    );
    assert!(reply.body.starts_with(b"%PDF-"));

    // At an address that ends in the name, for the PDF viewer's title.
    let reply = server
        .api(
            alex,
            "GET",
            &format!("/items/{id}/preview/Budget%202026.xlsx"),
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.starts_with(b"%PDF-"));

    // Browsers' PDF viewers ask for parts of long ones.
    let reply = server
        .send(
            "GET",
            &format!("/api/drive/items/{id}/preview"),
            &[("authorization", &alex.bearer), ("range", "bytes=0-3")],
            Vec::new(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&reply.body[..], b"%PDF");

    // Nobody else reaches it.
    let reply = server
        .api(&server.sam, "GET", &format!("/items/{id}/preview"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plays_sound_core_converts_for_the_browser() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // A Windows Media song, made with ffmpeg outside the sandbox.
    let dir = tempfile::tempdir().unwrap();
    let song = dir.path().join("song.wma");
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i", "sine=duration=2"])
        .args(["-c:a", "wmav2"])
        .arg(&song)
        .status()
        .unwrap();
    assert!(made.success());
    let song = server
        .send(
            "PUT",
            &format!("/api/drive/items/{top}/children/Song.wma"),
            &[("authorization", &alex.bearer)],
            std::fs::read(&song).unwrap(),
        )
        .await
        .json();
    let id = song["id"].as_str().unwrap().to_owned();
    // Browsers don't play Windows Media, so there's nothing yet.
    assert_eq!(song["preview"], json!(null));

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let worker = tokio::spawn(server.thumbnails.clone().run(async {
        let _ = stopped.await;
    }));
    let item = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let item = server
                .api(alex, "GET", &format!("/items/{id}"), None)
                .await
                .json();
            if item["preview"] == json!("audio") {
                break item;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("core should convert the song");
    stop.send(()).unwrap();
    worker.await.unwrap();
    assert_eq!(item["type"], json!("audio"));

    let reply = server
        .api(alex, "GET", &format!("/items/{id}/preview"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["content-type"], "audio/mp4");
    assert_eq!(
        reply.headers["content-disposition"],
        "inline; filename=\"Song.m4a\"; filename*=UTF-8''Song.m4a"
    );
    assert_eq!(&reply.body[4..8], b"ftyp");
    // Players ask for parts as they seek.
    let reply = server
        .send(
            "GET",
            &format!("/api/drive/items/{id}/preview"),
            &[("authorization", &alex.bearer), ("range", "bytes=4-7")],
            Vec::new(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&reply.body[..], b"ftyp");
    // The download is still the song itself.
    let reply = server
        .api(alex, "GET", &format!("/items/{id}/content"), None)
        .await;
    assert!(reply.body.starts_with(b"\x30\x26\xb2\x75"));
}

#[tokio::test]
async fn colors_describes_and_sizes_folders() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let folder = |name: &'static str, parent: String| {
        let server = &server;
        async move {
            server
                .api(
                    &server.alex,
                    "POST",
                    &format!("/items/{parent}/children"),
                    Some(json!({ "name": name })),
                )
                .await
                .json()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let trip = folder("Trip", top.clone()).await;
    let day = folder("Day 1", trip.clone()).await;
    let notes = write(&server, &top, "notes.txt", "Pick up the bread.").await;
    write(&server, &trip, "map.txt", "North, then east.").await;
    write(&server, &day, "beach.txt", "Sand, and a lot of it.").await;

    // A folder's size is everything in it, however deep.
    let children = server
        .api(alex, "GET", &format!("/items/{top}/children"), None)
        .await
        .json();
    let trip_view = children
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "Trip")
        .unwrap();
    assert_eq!(trip_view["size"], json!(17 + 22));
    assert_eq!(trip_view["color"], json!(null));

    // Colors are for folders, from a set list, and each person's own.
    let reply = server
        .api(
            alex,
            "PUT",
            &format!("/items/{trip}/color"),
            Some(json!({ "color": "teal" })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let item = server
        .api(alex, "GET", &format!("/items/{trip}"), None)
        .await
        .json();
    assert_eq!(item["color"], json!("teal"));
    for (id, color) in [(&trip, "plaid"), (&notes, "teal")] {
        let reply = server
            .api(
                alex,
                "PUT",
                &format!("/items/{id}/color"),
                Some(json!({ "color": color })),
            )
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{color}");
    }
    let reply = server
        .api(alex, "DELETE", &format!("/items/{trip}/color"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let item = server
        .api(alex, "GET", &format!("/items/{trip}"), None)
        .await
        .json();
    assert_eq!(item["color"], json!(null));

    // A description is the item's, trimmed, and an empty one removes it.
    let address = format!("/items/{notes}/description");
    let reply = server
        .api(
            alex,
            "PUT",
            &address,
            Some(json!({ "description": "  For Saturday's shop.  " })),
        )
        .await;
    assert_eq!(reply.json()["description"], "For Saturday's shop.");
    let reply = server.api(alex, "GET", &address, None).await;
    assert_eq!(reply.json()["description"], "For Saturday's shop.");
    let long = "a".repeat(2001);
    let reply = server
        .api(alex, "PUT", &address, Some(json!({ "description": long })))
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    server
        .api(alex, "PUT", &address, Some(json!({ "description": "" })))
        .await;
    let reply = server.api(alex, "GET", &address, None).await;
    assert_eq!(reply.json()["description"], "");

    // The storage page lists the largest files first.
    let largest = server.api(alex, "GET", "/largest", None).await.json();
    assert_eq!(
        names(&largest),
        ["Trip/Day 1/beach.txt", "notes.txt", "Trip/map.txt"]
    );
}

#[tokio::test]
async fn the_text_editor_saves_only_over_what_it_opened() {
    let server = start().await;
    let (alex, kim) = (&server.alex, &server.kim);
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let new = |name: &'static str| {
        let path = format!("/api/drive/items/{top}/children/{name}");
        let bearer = alex.bearer.clone();
        let server = &server;
        async move {
            server
                .send(
                    "PUT",
                    &path,
                    &[("authorization", &bearer), ("if-none-match", "*")],
                    Body::empty(),
                )
                .await
        }
    };
    // A new text file takes a free name only.
    let reply = new("list.md").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let file = reply.json();
    let id = file["id"].as_str().unwrap().to_owned();
    let opened = file["sha256"].as_str().unwrap().to_owned();
    assert_eq!(new("list.md").await.status, StatusCode::CONFLICT);

    let save = |person: &common::Person, base: Option<String>, text: &'static str| {
        let path = format!("/api/drive/items/{id}/content");
        let bearer = person.bearer.clone();
        let server = &server;
        async move {
            let quoted = base.map(|base| format!("\"{base}\""));
            let mut headers = vec![("authorization", bearer.as_str())];
            if let Some(quoted) = &quoted {
                headers.push(("if-match", quoted));
            }
            server.send("PUT", &path, &headers, text).await
        }
    };
    let reply = save(alex, Some(opened.clone()), "# Milk").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["size"], 6);
    let saved = reply.json()["sha256"].as_str().unwrap().to_owned();

    // A save from the old contents would lose the one before it.
    let reply = save(alex, Some(opened), "# Eggs").await;
    assert_eq!(reply.status, StatusCode::PRECONDITION_FAILED);
    assert!(
        reply.text().contains("Someone changed this file"),
        "{}",
        reply.text()
    );
    assert_eq!(
        save(alex, None, "# Eggs").await.status,
        StatusCode::PRECONDITION_REQUIRED
    );

    // Viewers can't save. Editors can.
    let reply = server
        .api(
            alex,
            "PUT",
            &format!("/items/{id}/shares/{}", kim.id),
            Some(json!({ "level": "viewer" })),
        )
        .await;
    assert!(reply.status.is_success());
    assert_eq!(
        save(kim, Some(saved.clone()), "# Kim").await.status,
        StatusCode::FORBIDDEN
    );
    server
        .api(
            alex,
            "PUT",
            &format!("/items/{id}/shares/{}", kim.id),
            Some(json!({ "level": "editor" })),
        )
        .await;
    let reply = save(kim, Some(saved), "# Kim").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let content = server
        .api(alex, "GET", &format!("/items/{id}/content"), None)
        .await;
    assert_eq!(content.text(), "# Kim");
}

#[tokio::test]
async fn calls_a_file_audio_once_core_finds_sound_in_it() {
    let server = start().await;
    let alex = &server.alex;
    let top = server.api(alex, "GET", "/top", None).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Ensoniq PARIS audio, made with SoX. Most `.paf` files are family
    // trees, so the name alone doesn't say it's sound.
    let dir = tempfile::tempdir().unwrap();
    let recording = dir.path().join("recording.paf");
    let made = std::process::Command::new("sox")
        .args(["-V1", "-n", "-c", "1"])
        .arg(&recording)
        .args(["synth", "2", "sine", "440"])
        .status()
        .unwrap();
    assert!(made.success());
    let recording = server
        .send(
            "PUT",
            &format!("/api/drive/items/{top}/children/Recording.paf"),
            &[("authorization", &alex.bearer)],
            std::fs::read(&recording).unwrap(),
        )
        .await
        .json();
    let id = recording["id"].as_str().unwrap().to_owned();
    assert_eq!(recording["type"], json!(null));

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let worker = tokio::spawn(server.thumbnails.clone().run(async {
        let _ = stopped.await;
    }));
    let item = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let item = server
                .api(alex, "GET", &format!("/items/{id}"), None)
                .await
                .json();
            if item["preview"] == json!("audio") {
                break item;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("core should convert the recording");
    stop.send(()).unwrap();
    worker.await.unwrap();
    assert_eq!(item["type"], json!("audio"));
}
