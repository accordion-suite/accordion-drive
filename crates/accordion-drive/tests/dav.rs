//! WebDAV, as File Explorer, Finder, and other standard clients use it.

mod common;

use accordion_core::storage::ItemKind;
use axum::body::Body;
use axum::http::StatusCode;
use common::start;

const PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><allprop/></propfind>"#;

#[tokio::test]
async fn needs_an_app_password() {
    let server = start().await;
    let reply = server
        .send("PROPFIND", "/dav/drive/", &[("depth", "0")], Body::empty())
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.headers.contains_key("www-authenticate"));

    // A session token for the apps isn't an app password.
    let reply = server
        .send(
            "PROPFIND",
            "/dav/drive/",
            &[("authorization", &server.alex.bearer)],
            Body::empty(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn saves_lists_and_reads_files() {
    let server = start().await;
    let alex = &server.alex;

    let reply = server
        .dav(alex, "MKCOL", "/Photos", &[], Body::empty())
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let reply = server
        .dav(
            alex,
            "PUT",
            "/Photos/beach%20d%C3%ADa.jpg",
            &[("content-length", "4")],
            "jpeg",
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());

    let reply = server
        .dav(alex, "PROPFIND", "/Photos/", &[("depth", "1")], PROPFIND)
        .await;
    assert_eq!(reply.status, StatusCode::MULTI_STATUS);
    let listing = reply.text();
    assert!(
        listing.contains("/dav/drive/Photos/beach%20d%C3%ADa.jpg"),
        "{listing}"
    );
    assert!(listing.contains("<D:getcontentlength>4<"), "{listing}");

    let reply = server
        .dav(
            alex,
            "GET",
            "/Photos/beach%20d%C3%ADa.jpg",
            &[],
            Body::empty(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text(), "jpeg");

    // Saving again keeps the first version.
    let reply = server
        .dav(
            alex,
            "PUT",
            "/Photos/beach%20d%C3%ADa.jpg",
            &[("content-length", "5")],
            "jpeg2",
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let top = server.storage.top_folder(alex.id).await.unwrap();
    let photo = server
        .storage
        .find(top, vec!["Photos".into(), "beach día.jpg".into()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server.storage.versions(photo.id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn moves_and_copies() {
    let server = start().await;
    let alex = &server.alex;
    server
        .dav(alex, "MKCOL", "/Archive", &[], Body::empty())
        .await;
    server
        .dav(
            alex,
            "PUT",
            "/notes.md",
            &[("content-length", "5")],
            "notes",
        )
        .await;

    let reply = server
        .dav(
            alex,
            "MOVE",
            "/notes.md",
            &[(
                "destination",
                "http://accordion.test/dav/drive/Archive/old%20notes.md",
            )],
            Body::empty(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());

    let reply = server
        .dav(
            alex,
            "COPY",
            "/Archive/old%20notes.md",
            &[("destination", "/dav/drive/copy.md")],
            Body::empty(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());

    let top = server.storage.top_folder(alex.id).await.unwrap();
    let names: Vec<String> = server
        .storage
        .children(top)
        .await
        .unwrap()
        .into_iter()
        .map(|item| item.name)
        .collect();
    assert_eq!(names, ["Archive", "copy.md"]);
    let reply = server
        .dav(alex, "GET", "/Archive/old%20notes.md", &[], Body::empty())
        .await;
    assert_eq!(reply.text(), "notes");
}

#[tokio::test]
async fn deletes_folders_to_the_trash_whole() {
    let server = start().await;
    let alex = &server.alex;
    server.dav(alex, "MKCOL", "/Old", &[], Body::empty()).await;
    for name in ["a.txt", "b.txt"] {
        server
            .dav(alex, "PUT", &format!("/Old/{name}"), &[], "text")
            .await;
    }

    let reply = server
        .dav(alex, "DELETE", "/Old/", &[], Body::empty())
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let trash = server.storage.trash(alex.id).await.unwrap();
    assert_eq!(trash.len(), 1);
    assert_eq!(trash[0].name, "Old");
    assert_eq!(trash[0].kind, ItemKind::Folder);

    let reply = server.dav(alex, "DELETE", "/", &[], Body::empty()).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    let reply = server
        .dav(alex, "DELETE", "/missing", &[], Body::empty())
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn keeps_nothing_from_a_cut_off_upload() {
    let server = start().await;
    let alex = &server.alex;
    let reply = server
        .dav(
            alex,
            "PUT",
            "/big.bin",
            &[("content-length", "100")],
            "only part",
        )
        .await;
    assert!(!reply.status.is_success(), "{}", reply.status);
    let top = server.storage.top_folder(alex.id).await.unwrap();
    assert_eq!(server.storage.children(top).await.unwrap(), []);
}

#[tokio::test]
async fn locks_succeed_for_windows_and_macos() {
    let server = start().await;
    let alex = &server.alex;
    let reply = server
        .dav(
            alex,
            "LOCK",
            "/doc.txt",
            &[("timeout", "Second-60")],
            r#"<?xml version="1.0" encoding="utf-8"?>
<lockinfo xmlns="DAV:"><lockscope><exclusive/></lockscope><locktype><write/></locktype>
<owner>alex</owner></lockinfo>"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert!(reply.headers.contains_key("lock-token"));
    let top = server.storage.top_folder(alex.id).await.unwrap();
    let created = server
        .storage
        .find(top, vec!["doc.txt".into()])
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(created.kind, ItemKind::File { blob, .. } if blob.size == 0));

    let reply = server.dav(alex, "OPTIONS", "/", &[], Body::empty()).await;
    let dav = reply.headers["dav"].to_str().unwrap();
    assert!(dav.contains('2'), "{dav}");
}

#[tokio::test]
async fn each_person_sees_only_their_own_files() {
    let server = start().await;
    server
        .dav(&server.alex, "PUT", "/mine.txt", &[], "alex's")
        .await;
    let reply = server
        .dav(&server.sam, "GET", "/mine.txt", &[], Body::empty())
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = server
        .dav(&server.sam, "PROPFIND", "/", &[("depth", "1")], PROPFIND)
        .await;
    assert!(!reply.text().contains("mine.txt"));
}
