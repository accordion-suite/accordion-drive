//! Sharing through the JSON API, and the check that every address asks
//! core who may do what (decision 16).

mod common;

use std::collections::BTreeSet;

use axum::http::StatusCode;
use common::{Person, Reply, Server, start};
use serde_json::{Value, json};

/// The reply for something someone can't see, word for word, so a test can
/// tell it from other kinds of "not found", such as a missing thumbnail.
const HIDDEN: &str = "That doesn't exist, or you don't have access to it.";

fn hidden(reply: &Reply) -> bool {
    reply.status == StatusCode::NOT_FOUND
        && serde_json::from_slice::<Value>(&reply.body).is_ok_and(|json| json["error"] == HIDDEN)
}

/// Alex's files: a "Holiday" folder shared with Sam as an editor and Kim
/// as a viewer, with a photo that has two versions, and a deleted file. Lee
/// has no access, and "Private" is Alex's alone.
struct Fixture {
    top: String,
    holiday: String,
    private: String,
    photo: String,
    first_version: String,
    trashed: String,
    upload: String,
    kim: String,
    lee: String,
    /// A link Alex made to the holiday folder.
    link: String,
}

fn id(reply: Reply) -> String {
    assert!(
        reply.status.is_success(),
        "{}: {}",
        reply.status,
        reply.text()
    );
    reply.json()["id"].as_str().unwrap().to_owned()
}

async fn put_file(server: &Server, person: &Person, folder: &str, name: &str, body: &str) -> Reply {
    server
        .send(
            "PUT",
            &format!("/api/drive/items/{folder}/children/{name}"),
            &[("authorization", &person.bearer)],
            body.to_owned(),
        )
        .await
}

async fn share(server: &Server, item: &str, with: &Person, level: &str) -> Reply {
    server
        .api(
            &server.alex,
            "PUT",
            &format!("/items/{item}/shares/{}", with.id),
            Some(json!({ "level": level })),
        )
        .await
}

/// Alex's files for one test, in folders named with `n` so that tests
/// sharing a server don't meet.
async fn files(server: &Server, n: usize) -> Fixture {
    let alex = &server.alex;
    let top = id(server.api(alex, "GET", "/top", None).await);
    let new_folder = |name: String| {
        let top = top.clone();
        async move {
            id(server
                .api(
                    &server.alex,
                    "POST",
                    &format!("/items/{top}/children"),
                    Some(json!({ "name": name })),
                )
                .await)
        }
    };
    let holiday = new_folder(format!("Holiday {n}")).await;
    let private = new_folder(format!("Private {n}")).await;
    put_file(server, alex, &holiday, "beach.jpg", "first").await;
    let photo = id(put_file(server, alex, &holiday, "beach.jpg", "second").await);
    let versions = server
        .api(alex, "GET", &format!("/items/{photo}/versions"), None)
        .await
        .json();
    let first_version = versions[1]["id"].as_str().unwrap().to_owned();
    let trashed = id(put_file(server, alex, &holiday, "old.txt", "old").await);
    assert!(
        server
            .api(alex, "DELETE", &format!("/items/{trashed}"), None)
            .await
            .status
            .is_success()
    );
    let upload = id(server
        .api(alex, "POST", "/uploads", Some(json!({ "size": 2 })))
        .await);
    for (person, level) in [(&server.sam, "editor"), (&server.kim, "viewer")] {
        assert_eq!(
            share(server, &holiday, person, level).await.status,
            StatusCode::OK
        );
    }
    let link = server
        .api(
            alex,
            "POST",
            &format!("/items/{holiday}/links"),
            Some(json!({ "expires": "1d" })),
        )
        .await
        .json()["link"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    Fixture {
        link,
        kim: server.kim.id.to_string(),
        lee: server.lee.id.to_string(),
        top,
        holiday,
        private,
        photo,
        first_version,
        trashed,
        upload,
    }
}

/// Who may use an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Needs {
    Viewer,
    Editor,
    Owner,
    /// Only the owner, and hidden from everyone else, like what's in the
    /// trash.
    OwnerAlone,
    /// Everyone, each with their own: their settings, their trash, and so
    /// on.
    Own,
    /// Admins, which nobody here is.
    Admin,
}

struct Case {
    method: &'static str,
    /// The address as `routes` writes it, to check that every address has a
    /// case.
    route: &'static str,
    needs: Needs,
    request: fn(&Fixture) -> (String, Option<Value>),
}

fn cases() -> Vec<Case> {
    use Needs::*;
    macro_rules! case {
        ($method:literal, $route:literal, $needs:expr, |$f:ident| $path:expr) => {
            case!($method, $route, $needs, |$f| $path, None)
        };
        ($method:literal, $route:literal, $needs:expr, |$f:ident| $path:expr, $body:expr) => {
            Case {
                method: $method,
                route: $route,
                needs: $needs,
                request: |#[allow(unused_variables)] $f: &Fixture| ($path, $body),
            }
        };
    }
    vec![
        case!("GET", "/top", Own, |f| "/top".into()),
        case!("GET", "/items/{id}", Viewer, |f| format!(
            "/items/{}",
            f.photo
        )),
        case!(
            "PATCH",
            "/items/{id}",
            Editor,
            |f| format!("/items/{}", f.photo),
            Some(json!({ "name": "renamed.jpg" }))
        ),
        case!("DELETE", "/items/{id}", Editor, |f| format!(
            "/items/{}",
            f.photo
        )),
        case!("GET", "/items/{id}/children", Viewer, |f| format!(
            "/items/{}/children",
            f.holiday
        )),
        case!(
            "POST",
            "/items/{id}/children",
            Editor,
            |f| format!("/items/{}/children", f.holiday),
            Some(json!({ "name": "New folder" }))
        ),
        case!("PUT", "/items/{id}/children/{name}", Editor, |f| format!(
            "/items/{}/children/new.txt",
            f.holiday
        )),
        case!("GET", "/items/{id}/content", Viewer, |f| format!(
            "/items/{}/content",
            f.photo
        )),
        // Access is checked before If-Match, so without one, anyone allowed
        // gets 428 rather than 403 or "not found".
        case!("PUT", "/items/{id}/content", Editor, |f| format!(
            "/items/{}/content",
            f.photo
        )),
        case!("GET", "/items/{id}/preview", Viewer, |f| format!(
            "/items/{}/preview",
            f.photo
        )),
        case!("GET", "/items/{id}/thumbnail", Viewer, |f| format!(
            "/items/{}/thumbnail",
            f.photo
        )),
        case!("POST", "/items/{id}/restore", OwnerAlone, |f| format!(
            "/items/{}/restore",
            f.trashed
        )),
        case!("GET", "/items/{id}/preview/{name}", Viewer, |f| format!(
            "/items/{}/preview/beach.jpg",
            f.photo
        )),
        // A color is the reader's own, as a star is.
        case!(
            "PUT",
            "/items/{id}/color",
            Viewer,
            |f| format!("/items/{}/color", f.holiday),
            Some(json!({ "color": "blue" }))
        ),
        case!("DELETE", "/items/{id}/color", Viewer, |f| format!(
            "/items/{}/color",
            f.holiday
        )),
        case!("GET", "/items/{id}/description", Viewer, |f| format!(
            "/items/{}/description",
            f.photo
        )),
        case!(
            "PUT",
            "/items/{id}/description",
            Editor,
            |f| format!("/items/{}/description", f.photo),
            Some(json!({ "description": "The first day at the beach." }))
        ),
        case!("GET", "/largest", Own, |f| "/largest".into()),
        case!("PUT", "/items/{id}/star", Viewer, |f| format!(
            "/items/{}/star",
            f.photo
        )),
        case!("DELETE", "/items/{id}/star", Viewer, |f| format!(
            "/items/{}/star",
            f.photo
        )),
        case!(
            "POST",
            "/items/{id}/copy",
            Viewer,
            |f| format!("/items/{}/copy", f.photo),
            Some(json!({ "name": "Copy of beach.jpg" }))
        ),
        case!("GET", "/items/{id}/versions", Editor, |f| format!(
            "/items/{}/versions",
            f.photo
        )),
        case!(
            "GET",
            "/items/{id}/versions/{version}/content",
            Editor,
            |f| format!("/items/{}/versions/{}/content", f.photo, f.first_version)
        ),
        case!(
            "POST",
            "/items/{id}/versions/{version}/restore",
            Editor,
            |f| format!("/items/{}/versions/{}/restore", f.photo, f.first_version)
        ),
        case!("GET", "/items/{id}/shares", Owner, |f| format!(
            "/items/{}/shares",
            f.holiday
        )),
        case!(
            "PUT",
            "/items/{id}/shares/{user}",
            Owner,
            |f| format!("/items/{}/shares/{}", f.holiday, f.lee),
            Some(json!({ "level": "viewer" }))
        ),
        case!("DELETE", "/items/{id}/shares/{user}", Owner, |f| format!(
            "/items/{}/shares/{}",
            f.holiday, f.kim
        )),
        case!("DELETE", "/items/{id}/shares", Owner, |f| format!(
            "/items/{}/shares",
            f.holiday
        )),
        case!("GET", "/items/{id}/links", Editor, |f| format!(
            "/items/{}/links",
            f.holiday
        )),
        case!(
            "POST",
            "/items/{id}/links",
            Editor,
            |f| format!("/items/{}/links", f.holiday),
            Some(json!({ "expires": "1d" }))
        ),
        case!("GET", "/links", Admin, |f| "/links".into()),
        case!("DELETE", "/links/{id}", OwnerAlone, |f| format!(
            "/links/{}",
            f.link
        )),
        case!("GET", "/items/{id}/activity", Viewer, |f| format!(
            "/items/{}/activity",
            f.photo
        )),
        case!("GET", "/notifications", Own, |f| "/notifications".into()),
        case!("DELETE", "/notifications", Own, |f| {
            "/notifications?up_to=0".into()
        }),
        case!(
            "POST",
            "/notifications/seen",
            Own,
            |f| "/notifications/seen".into(),
            Some(json!({ "up_to": 0 }))
        ),
        case!(
            "POST",
            "/notifications/read",
            Own,
            |f| "/notifications/read".into(),
            Some(json!({ "ids": [] }))
        ),
        case!(
            "POST",
            "/notifications/remove",
            Own,
            |f| "/notifications/remove".into(),
            Some(json!({ "ids": [] }))
        ),
        case!("GET", "/shared", Own, |f| "/shared".into()),
        case!("GET", "/shared-by-me", Own, |f| "/shared-by-me".into()),
        case!("GET", "/trash", Own, |f| "/trash".into()),
        case!("DELETE", "/trash", Own, |f| "/trash".into()),
        case!("DELETE", "/trash/{id}", OwnerAlone, |f| format!(
            "/trash/{}",
            f.trashed
        )),
        case!("GET", "/usage", Own, |f| "/usage".into()),
        case!("GET", "/recent", Own, |f| "/recent".into()),
        case!("GET", "/starred", Own, |f| "/starred".into()),
        case!("GET", "/search", Own, |f| "/search?q=beach".into()),
        case!("GET", "/zip", Viewer, |f| format!("/zip?ids={}", f.photo)),
        case!("GET", "/settings", Own, |f| "/settings".into()),
        case!(
            "PUT",
            "/settings",
            Own,
            |f| "/settings".into(),
            Some(json!({ "start_page": "home", "thumbnails": true }))
        ),
        case!(
            "POST",
            "/uploads",
            Editor,
            |f| "/uploads".into(),
            Some(json!({ "size": 1, "parent": f.holiday }))
        ),
        // Uploads belong to whoever started them, here Alex.
        case!("GET", "/uploads/{id}", OwnerAlone, |f| format!(
            "/uploads/{}",
            f.upload
        )),
        case!("PATCH", "/uploads/{id}", OwnerAlone, |f| format!(
            "/uploads/{}",
            f.upload
        )),
        case!("DELETE", "/uploads/{id}", OwnerAlone, |f| format!(
            "/uploads/{}",
            f.upload
        )),
        case!(
            "POST",
            "/uploads/{id}/finish",
            OwnerAlone,
            |f| format!("/uploads/{}/finish", f.upload),
            Some(json!({ "parent": f.top, "name": "up.txt" }))
        ),
    ]
}

/// Every method and address that `routes` in `src/api.rs` serves.
fn served() -> BTreeSet<(String, String)> {
    let source = include_str!("../src/api.rs");
    let mut served = BTreeSet::new();
    for call in source.split(".route(").skip(1) {
        let call = call.split(".with_state").next().unwrap();
        let path = call.split('"').nth(1).unwrap();
        for method in ["get", "post", "put", "patch", "delete"] {
            let handler = format!("{method}(");
            let starts = call.match_indices(&handler).any(|(at, _)| {
                at == 0
                    || !call.as_bytes()[at - 1].is_ascii_alphanumeric()
                        && call.as_bytes()[at - 1] != b'_'
            });
            if starts {
                served.insert((method.to_uppercase(), path.to_owned()));
            }
        }
    }
    served
}

#[test]
fn every_address_has_a_case() {
    let listed: BTreeSet<(String, String)> = cases()
        .iter()
        .map(|case| (case.method.to_owned(), case.route.to_owned()))
        .collect();
    let served = served();
    assert!(served.len() > 30, "found only {served:?}");
    let missing: Vec<_> = served.difference(&listed).collect();
    assert!(
        missing.is_empty(),
        "Add a case to `cases` for each new address, saying who may use it: {missing:?}"
    );
    let extra: Vec<_> = listed.difference(&served).collect();
    assert!(extra.is_empty(), "These cases name no address: {extra:?}");
}

#[tokio::test]
async fn every_address_checks_access() {
    let server = start().await;
    for (n, case) in cases().into_iter().enumerate() {
        let label = format!("{} {}", case.method, case.route);
        // Fresh files for each, so one person's change doesn't decide the
        // next one's answer. The owner goes last.
        let f = files(&server, n).await;
        let (path, body) = (case.request)(&f);
        let people = [
            ("lee", &server.lee, None),
            ("kim", &server.kim, Some(Needs::Viewer)),
            ("sam", &server.sam, Some(Needs::Editor)),
            ("alex", &server.alex, Some(Needs::OwnerAlone)),
        ];
        for (name, person, has) in people {
            let reply = if case.method == "PATCH" && case.route == "/uploads/{id}" {
                server
                    .send(
                        "PATCH",
                        &format!("/api/drive{path}"),
                        &[("authorization", &person.bearer), ("upload-offset", "0")],
                        "hi",
                    )
                    .await
            } else {
                server.api(person, case.method, &path, body.clone()).await
            };
            if case.needs == Needs::Admin {
                assert_eq!(reply.status, StatusCode::FORBIDDEN, "{label} as {name}");
                continue;
            }
            let allowed = match (case.needs, has) {
                (Needs::Own, _) => true,
                (_, None) => false,
                (Needs::OwnerAlone, Some(has)) => has == Needs::OwnerAlone,
                (Needs::Owner, Some(has)) => has == Needs::OwnerAlone,
                (needs, Some(has)) => has >= needs,
            };
            let status = reply.status;
            if allowed {
                assert!(
                    status != StatusCode::FORBIDDEN && !hidden(&reply),
                    "{label} as {name}: expected access, got {status}: {}",
                    reply.text()
                );
            } else if has.is_none() || case.needs == Needs::OwnerAlone {
                assert!(
                    hidden(&reply),
                    "{label} as {name}: expected \"not found\", got {status}: {}",
                    reply.text()
                );
            } else {
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "{label} as {name}: {}",
                    reply.text()
                );
            }
        }
    }
}

#[tokio::test]
async fn shares_and_lists_whats_shared() {
    let s = &start().await;
    let f = files(s, 0).await;

    let shares = s
        .api(
            &s.alex,
            "GET",
            &format!("/items/{}/shares", f.holiday),
            None,
        )
        .await
        .json();
    let people: Vec<_> = shares
        .as_array()
        .unwrap()
        .iter()
        .map(|share| (share["username"].clone(), share["level"].clone()))
        .collect();
    assert_eq!(
        people,
        [
            (json!("sam"), json!("editor")),
            (json!("kim"), json!("viewer"))
        ]
    );

    // Alex sees which of their items are shared.
    let mine = s
        .api(&s.alex, "GET", &format!("/items/{}/children", f.top), None)
        .await
        .json();
    let holiday = mine
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "Holiday 0")
        .unwrap();
    assert_eq!(holiday["shared"], true);
    assert_eq!(holiday["access"], "owner");
    assert_eq!(holiday["owner"], Value::Null);

    // Kim sees it in Shared with me, with Alex as the owner.
    let shared = s.api(&s.kim, "GET", "/shared", None).await.json();
    assert_eq!(shared.as_array().unwrap().len(), 1);
    assert_eq!(shared[0]["name"], "Holiday 0");
    assert_eq!(shared[0]["access"], "viewer");
    assert_eq!(shared[0]["owner"], "alex");
    assert_eq!(shared[0]["shared"], false);
    assert_eq!(shared[0]["folders"], json!([]));
    let inside = s
        .api(
            &s.kim,
            "GET",
            &format!("/items/{}/children", f.holiday),
            None,
        )
        .await
        .json();
    assert_eq!(inside[0]["name"], "beach.jpg");
    assert_eq!(inside[0]["access"], "viewer");
    // A share on the photo itself gives Kim more there.
    assert_eq!(
        share(s, &f.photo, &s.kim, "editor").await.status,
        StatusCode::OK
    );
    let inside = s
        .api(
            &s.kim,
            "GET",
            &format!("/items/{}/children", f.holiday),
            None,
        )
        .await
        .json();
    assert_eq!(inside[0]["access"], "editor");

    // Search and Recent include it, starting from the shared folder.
    let found = s.api(&s.kim, "GET", "/search?q=beach", None).await.json();
    assert_eq!(found[0]["folders"], json!(["Holiday 0"]));
    assert_eq!(found[0]["owner"], "alex");
    let recent = s.api(&s.sam, "GET", "/recent", None).await.json();
    assert_eq!(recent[0]["name"], "beach.jpg");
    assert_eq!(recent[0]["access"], "editor");

    // Starred keeps only what Kim can still reach.
    s.api(&s.kim, "PUT", &format!("/items/{}/star", f.photo), None)
        .await;
    assert_eq!(
        s.api(&s.kim, "GET", "/starred", None).await.json()[0]["name"],
        "beach.jpg"
    );
    for item in [&f.photo, &f.holiday] {
        let reply = s
            .api(
                &s.alex,
                "DELETE",
                &format!("/items/{item}/shares/{}", s.kim.id),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
    }
    assert_eq!(
        s.api(&s.kim, "GET", "/starred", None).await.json(),
        json!([])
    );
    assert_eq!(
        s.api(&s.kim, "GET", "/shared", None).await.json(),
        json!([])
    );

    // Alex's Shared by me lists the folder, with Sam still on it.
    let out = s.api(&s.alex, "GET", "/shared-by-me", None).await.json();
    assert_eq!(out.as_array().unwrap().len(), 1);
    assert_eq!(out[0]["name"], "Holiday 0");
    assert_eq!(out[0]["folders"], json!([]));
    assert_eq!(out[0]["shared"], true);
    assert_eq!(out[0]["shared_with"][0]["username"], "sam");
    assert_eq!(out[0]["shared_with"][0]["level"], "editor");
    assert!(out[0]["shared_at"].is_string());
    assert_eq!(
        s.api(&s.kim, "GET", "/shared-by-me", None).await.json(),
        json!([])
    );
    // Stop sharing removes everyone at once.
    let reply = s
        .api(
            &s.alex,
            "DELETE",
            &format!("/items/{}/shares", f.holiday),
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.api(&s.alex, "GET", "/shared-by-me", None).await.json(),
        json!([])
    );
    assert_eq!(
        s.api(&s.sam, "GET", "/shared", None).await.json(),
        json!([])
    );
    let reply = s
        .api(&s.kim, "GET", &format!("/items/{}", f.photo), None)
        .await;
    assert!(hidden(&reply));
}

#[tokio::test]
async fn refuses_shares_that_make_no_sense() {
    let s = &start().await;
    let f = files(s, 0).await;
    let reply = share(s, &f.holiday, &s.alex, "viewer").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = share(s, &f.top, &s.lee, "viewer").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = share(s, &f.holiday, &s.lee, "owner").await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    // An editor can't share.
    let reply = s
        .api(
            &s.sam,
            "PUT",
            &format!("/items/{}/shares/{}", f.holiday, s.lee.id),
            Some(json!({ "level": "viewer" })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn editors_work_inside_the_owners_folder() {
    let s = &start().await;
    let f = files(s, 0).await;
    let sam = &s.sam;

    // What Sam adds belongs to Alex, and counts toward Alex's space.
    let added = put_file(s, sam, &f.holiday, "from%20sam.txt", "hello")
        .await
        .json();
    assert_eq!(added["access"], "editor");
    assert_eq!(added["owner"], "alex");
    assert_eq!(s.api(sam, "GET", "/usage", None).await.json()["used"], 0);

    // A resumable upload too.
    let upload = id(s
        .api(
            sam,
            "POST",
            "/uploads",
            Some(json!({ "size": 2, "parent": f.holiday })),
        )
        .await);
    s.send(
        "PATCH",
        &format!("/api/drive/uploads/{upload}"),
        &[("authorization", &sam.bearer), ("upload-offset", "0")],
        "hi",
    )
    .await;
    let finished = s
        .api(
            sam,
            "POST",
            &format!("/uploads/{upload}/finish"),
            Some(json!({ "parent": f.holiday, "name": "hi.txt" })),
        )
        .await;
    assert_eq!(finished.status, StatusCode::OK, "{}", finished.text());

    // Sam can't move things out of the shared folder, or into their own
    // Drive.
    let sams = id(s.api(sam, "GET", "/top", None).await);
    for to in [&f.private, &sams] {
        let reply = s
            .api(
                sam,
                "PATCH",
                &format!("/items/{}", f.photo),
                Some(json!({ "parent": to })),
            )
            .await;
        assert!(
            hidden(&reply) || reply.status == StatusCode::CONFLICT,
            "{}: {}",
            reply.status,
            reply.text()
        );
    }

    // What Sam deletes goes to Alex's trash.
    s.api(sam, "DELETE", &format!("/items/{}", f.photo), None)
        .await;
    assert_eq!(s.api(sam, "GET", "/trash", None).await.json(), json!([]));
    let trash = s.api(&s.alex, "GET", "/trash", None).await.json();
    assert!(
        trash
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == f.photo.as_str())
    );

    // When Alex runs out of space, the message says so.
    s.storage.set_quota(s.alex.id, Some(1)).await.unwrap();
    let reply = put_file(s, sam, &f.holiday, "more.txt", "more").await;
    assert_eq!(reply.status, StatusCode::INSUFFICIENT_STORAGE);
    assert!(reply.json()["error"].as_str().unwrap().contains("owner"));
}

#[tokio::test]
async fn viewers_copy_into_their_own_drive() {
    let s = &start().await;
    let f = files(s, 0).await;
    let kim = &s.kim;
    for expected in ["Copy of beach.jpg", "Copy of beach (1).jpg"] {
        let copy = s
            .api(
                kim,
                "POST",
                &format!("/items/{}/copy", f.photo),
                Some(json!({ "name": "Copy of beach.jpg" })),
            )
            .await;
        assert_eq!(copy.status, StatusCode::CREATED, "{}", copy.text());
        let copy = copy.json();
        assert_eq!(copy["name"], expected);
        assert_eq!(copy["access"], "owner");
    }
    let kims = id(s.api(kim, "GET", "/top", None).await);
    let mine = s
        .api(kim, "GET", &format!("/items/{kims}/children"), None)
        .await
        .json();
    assert_eq!(mine.as_array().unwrap().len(), 2);
    assert_eq!(s.api(kim, "GET", "/usage", None).await.json()["used"], 12);
}

#[tokio::test]
async fn webdav_my_drive_shows_only_your_own_files() {
    let s = &start().await;
    files(s, 0).await;
    let reply = s
        .dav(&s.sam, "PROPFIND", "/Holiday%200/", &[("depth", "1")], "")
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = s
        .dav(&s.sam, "GET", "/Holiday%200/beach.jpg", &[], "")
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

/// The names in a PROPFIND reply, decoded, without the folder itself.
fn listed(reply: &Reply, folder: &str) -> Vec<String> {
    let text = reply.text();
    let mut names: Vec<String> = text
        .split("<D:href>")
        .skip(1)
        .filter_map(|rest| rest.split("</D:href>").next())
        .filter(|href| href.trim_end_matches('/') != folder.trim_end_matches('/'))
        .map(|href| {
            let name = href.trim_end_matches('/').rsplit('/').next().unwrap();
            percent_decode(name)
        })
        .collect();
    names.sort();
    names
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 {
            out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn webdav_shared_lists_whats_shared_with_you() {
    let s = &start().await;
    let f = files(s, 0).await;
    let reply = s
        .shared_dav(&s.kim, "PROPFIND", "/", &[("depth", "1")], "")
        .await;
    assert_eq!(reply.status, StatusCode::MULTI_STATUS, "{}", reply.text());
    assert_eq!(listed(&reply, "/dav/shared/"), ["Holiday 0"]);
    // Lee has nothing shared, and Alex's own files aren't here.
    for person in [&s.lee, &s.alex] {
        let reply = s
            .shared_dav(person, "PROPFIND", "/", &[("depth", "1")], "")
            .await;
        assert_eq!(listed(&reply, "/dav/shared/"), Vec::<String>::new());
    }

    // Sam shares a folder with the same name with Kim. The older share
    // keeps its name, and the newer one gets its owner's.
    let sams = id(s.api(&s.sam, "GET", "/top", None).await);
    let theirs = id(s
        .api(
            &s.sam,
            "POST",
            &format!("/items/{sams}/children"),
            Some(json!({ "name": "Holiday 0" })),
        )
        .await);
    let reply = s
        .api(
            &s.sam,
            "PUT",
            &format!("/items/{theirs}/shares/{}", s.kim.id),
            Some(json!({ "level": "viewer" })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = s
        .shared_dav(&s.kim, "PROPFIND", "/", &[("depth", "1")], "")
        .await;
    assert_eq!(
        listed(&reply, "/dav/shared/"),
        ["Holiday 0", "Holiday 0 (sam)"]
    );

    // Inside, the folder's own files.
    let reply = s
        .shared_dav(&s.kim, "PROPFIND", "/Holiday%200/", &[("depth", "1")], "")
        .await;
    assert_eq!(listed(&reply, "/dav/shared/Holiday%200/"), ["beach.jpg"]);
    let reply = s
        .shared_dav(&s.kim, "GET", "/Holiday%200/beach.jpg", &[], "")
        .await;
    assert_eq!(reply.text(), "second");

    // Removing Kim works on the next request.
    s.api(
        &s.alex,
        "DELETE",
        &format!("/items/{}/shares/{}", f.holiday, s.kim.id),
        None,
    )
    .await;
    let reply = s
        .shared_dav(&s.kim, "GET", "/Holiday%200/beach.jpg", &[], "")
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webdav_shared_follows_each_persons_access() {
    let s = &start().await;
    let f = files(s, 0).await;
    let photo = "/Holiday%200/beach.jpg";

    // Kim, a viewer, can read but change nothing.
    let kim = &s.kim;
    let refused = [
        s.shared_dav(kim, "PUT", "/Holiday%200/new.txt", &[], "new")
            .await,
        s.shared_dav(kim, "PUT", photo, &[], "changed").await,
        s.shared_dav(kim, "MKCOL", "/Holiday%200/New", &[], "")
            .await,
        s.shared_dav(kim, "DELETE", photo, &[], "").await,
        s.shared_dav(
            kim,
            "MOVE",
            photo,
            &[("destination", "/dav/shared/Holiday%200/moved.jpg")],
            "",
        )
        .await,
    ];
    for reply in refused {
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    }
    assert_eq!(
        s.shared_dav(kim, "GET", photo, &[], "").await.text(),
        "second"
    );

    // Sam, an editor, can add, rename, and delete inside, but can't add to
    // the list of shares itself.
    let sam = &s.sam;
    let reply = s
        .shared_dav(sam, "PUT", "/Holiday%200/from%20sam.txt", &[], "hello")
        .await;
    assert!(
        reply.status.is_success(),
        "{}: {}",
        reply.status,
        reply.text()
    );
    let reply = s
        .shared_dav(sam, "MKCOL", "/Holiday%200/Sam's", &[], "")
        .await;
    assert!(reply.status.is_success(), "{}", reply.status);
    let reply = s
        .shared_dav(
            sam,
            "MOVE",
            "/Holiday%200/from%20sam.txt",
            &[("destination", "/dav/shared/Holiday%200/Sam's/hello.txt")],
            "",
        )
        .await;
    assert!(reply.status.is_success(), "{}", reply.status);
    let reply = s.shared_dav(sam, "MKCOL", "/Elsewhere", &[], "").await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = s.shared_dav(sam, "PUT", "/loose.txt", &[], "x").await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);

    // What Sam adds is Alex's, and what Sam deletes goes to Alex's trash.
    let inside = s
        .api(
            &s.alex,
            "GET",
            &format!("/items/{}/children", f.holiday),
            None,
        )
        .await
        .json();
    let names: Vec<_> = inside
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["Sam's", "beach.jpg"]);
    let reply = s.shared_dav(sam, "DELETE", photo, &[], "").await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let trash = s.api(&s.alex, "GET", "/trash", None).await.json();
    assert!(
        trash
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == f.photo.as_str())
    );

    // Nothing moves from the shared folder into Sam's own Drive.
    let reply = s
        .shared_dav(
            sam,
            "MOVE",
            "/Holiday%200/Sam's",
            &[("destination", "/dav/drive/Sam's")],
            "",
        )
        .await;
    assert!(!reply.status.is_success(), "{}", reply.status);

    // Lee sees nothing at all.
    let reply = s.shared_dav(&s.lee, "GET", photo, &[], "").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}
