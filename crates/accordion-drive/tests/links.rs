//! Links, from making one in the JSON API to opening it on the link pages
//! the way someone on the internet would (decision 22).

mod common;

use axum::http::StatusCode;
use common::{Person, Reply, Server, start};
use serde_json::{Value, json};

fn id(reply: Reply) -> String {
    assert!(
        reply.status.is_success(),
        "{} {}",
        reply.status,
        reply.text()
    );
    reply.json()["id"].as_str().unwrap().to_owned()
}

async fn folder(server: &Server, person: &Person, parent: &str, name: &str) -> String {
    id(server
        .api(
            person,
            "POST",
            &format!("/items/{parent}/children"),
            Some(json!({ "name": name })),
        )
        .await)
}

async fn file(server: &Server, person: &Person, parent: &str, name: &str, body: &str) -> String {
    id(server
        .send(
            "PUT",
            &format!("/api/drive/items/{parent}/children/{name}"),
            &[("authorization", &person.bearer)],
            body.to_owned(),
        )
        .await)
}

/// Makes a link, and returns its address's path, such as `/s/abc…`.
async fn link(server: &Server, person: &Person, item: &str, settings: Value) -> String {
    let reply = server
        .api(
            person,
            "POST",
            &format!("/items/{item}/links"),
            Some(settings),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let url = reply.json()["url"].as_str().unwrap().to_owned();
    let path = url
        .strip_prefix("https://f8q2m.accordion.example.com")
        .unwrap_or_else(|| panic!("the link {url} isn't on the links name"));
    assert_eq!(path.len(), "/s/".len() + 22, "{path}");
    path.to_owned()
}

fn doesnt_work(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text());
    assert!(
        reply.text().contains("This link doesn't work"),
        "{}",
        reply.text()
    );
}

#[tokio::test]
async fn opens_a_linked_folder_and_nothing_beside_it() {
    let server = start().await;
    let alex = &server.alex;
    let top = id(server.api(alex, "GET", "/top", None).await);
    let holiday = folder(&server, alex, &top, "Holiday").await;
    let photo = file(&server, alex, &holiday, "beach.jpg", "not really a photo").await;
    let page = file(
        &server,
        alex,
        &holiday,
        "page.html",
        "<script>alert(1)</script>",
    )
    .await;
    let day_two = folder(&server, alex, &holiday, "Day 2").await;
    let notes = file(&server, alex, &day_two, "notes.txt", "sunny").await;
    let taxes = file(&server, alex, &top, "taxes.pdf", "%PDF private").await;

    let path = link(&server, alex, &holiday, json!({ "expires": "30d" })).await;

    let root = server.link("GET", &path, &[], "").await;
    assert_eq!(root.status, StatusCode::OK);
    let html = root.text();
    assert!(html.contains("<h1>Holiday</h1>"), "{html}");
    assert!(html.contains("alex shared this with you."));
    assert!(html.contains("beach.jpg") && html.contains("Day 2") && html.contains("page.html"));
    assert_eq!(root.headers["x-robots-tag"], "noindex, nofollow");
    assert_eq!(root.headers["referrer-policy"], "no-referrer");
    assert!(
        root.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .starts_with("default-src 'none'")
    );

    // A file inside a folder inside it, and its preview.
    let inner = server
        .link("GET", &format!("{path}/i/{notes}"), &[], "")
        .await;
    assert_eq!(inner.status, StatusCode::OK);
    assert!(inner.text().contains("<h1>notes.txt</h1>"));
    let photo_page = server
        .link("GET", &format!("{path}/i/{photo}"), &[], "")
        .await;
    assert!(
        photo_page
            .text()
            .contains(&format!("src=\"{path}/preview/{photo}\""))
    );
    let preview = server
        .link("GET", &format!("{path}/preview/{photo}"), &[], "")
        .await;
    assert_eq!(preview.status, StatusCode::OK);
    assert_eq!(preview.headers["content-type"], "image/jpeg");
    assert!(
        preview.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .starts_with("sandbox")
    );

    // An HTML file is never shown, only downloaded.
    doesnt_work(
        &server
            .link("GET", &format!("{path}/preview/{page}"), &[], "")
            .await,
    );
    let download = server
        .link("GET", &format!("{path}/download/{page}"), &[], "")
        .await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.headers["content-type"], "application/octet-stream");
    assert!(
        download.headers["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );

    // The whole folder, as one ZIP file.
    let zip = server
        .link("GET", &format!("{path}/download/{holiday}"), &[], "")
        .await;
    assert_eq!(zip.status, StatusCode::OK);
    assert_eq!(zip.headers["content-type"], "application/zip");
    assert!(zip.body.starts_with(b"PK"));

    // Nothing outside the linked folder, however it's asked for.
    for outside in [&taxes, &top] {
        for kind in ["i", "download", "preview", "thumbnail"] {
            doesnt_work(
                &server
                    .link("GET", &format!("{path}/{kind}/{outside}"), &[], "")
                    .await,
            );
        }
    }
    // Made-up links, and paths that aren't link pages, get the same page.
    doesnt_work(
        &server
            .link("GET", "/s/AAAAAAAAAAAAAAAAAAAAAA", &[], "")
            .await,
    );
    doesnt_work(&server.link("GET", "/api/drive/top", &[], "").await);
    assert_eq!(
        server.link("GET", "/robots.txt", &[], "").await.text(),
        "User-agent: *\nDisallow: /\n"
    );

    // Moving a file out of the folder takes it out of the link, and
    // trashing the folder ends the link.
    server
        .api(
            alex,
            "PATCH",
            &format!("/items/{notes}"),
            Some(json!({ "parent": top })),
        )
        .await;
    doesnt_work(
        &server
            .link("GET", &format!("{path}/i/{notes}"), &[], "")
            .await,
    );
    server
        .api(alex, "DELETE", &format!("/items/{holiday}"), None)
        .await;
    doesnt_work(&server.link("GET", &path, &[], "").await);

    // Its maker hears that it was opened.
    let notifications = server.api(alex, "GET", "/notifications", None).await.text();
    assert!(notifications.contains("link_opened"), "{notifications}");
}

#[tokio::test]
async fn asks_for_the_password_and_keeps_the_download_limit() {
    let server = start().await;
    let alex = &server.alex;
    let top = id(server.api(alex, "GET", "/top", None).await);
    let lease = file(&server, alex, &top, "lease.pdf", "%PDF lease").await;
    let path = link(
        &server,
        alex,
        &lease,
        json!({ "expires": "7d", "password": "open sesame", "download_limit": 1, "note": "For the landlord" }),
    )
    .await;

    // Nothing about the file shows before the password.
    let locked = server.link("GET", &path, &[], "").await;
    assert_eq!(locked.status, StatusCode::OK);
    assert!(locked.text().contains("This link needs a password"));
    assert!(!locked.text().contains("lease.pdf"));
    doesnt_work(
        &server
            .link("GET", &format!("{path}/download/{lease}"), &[], "")
            .await,
    );

    let form = [("content-type", "application/x-www-form-urlencoded")];
    let wrong = server.link("POST", &path, &form, "password=guess").await;
    assert!(wrong.text().contains("That password isn't right"));
    let right = server
        .link("POST", &path, &form, "password=open+sesame")
        .await;
    assert_eq!(right.status, StatusCode::SEE_OTHER);
    let cookie = right.headers["set-cookie"].to_str().unwrap();
    assert!(cookie.contains("HttpOnly") && cookie.contains("Secure"));
    let cookie = cookie.split(';').next().unwrap().to_owned();
    let with = [("cookie", cookie.as_str())];

    let opened = server.link("GET", &path, &with, "").await;
    assert!(
        opened.text().contains("<h1>lease.pdf</h1>"),
        "{}",
        opened.text()
    );
    let first = server
        .link("GET", &format!("{path}/download/{lease}"), &with, "")
        .await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.text(), "%PDF lease");
    // One download was allowed, so the link stops working.
    doesnt_work(
        &server
            .link("GET", &format!("{path}/download/{lease}"), &with, "")
            .await,
    );
    doesnt_work(&server.link("GET", &path, &with, "").await);

    // The owner sees it, with its note and counts.
    let links = server
        .api(alex, "GET", &format!("/items/{lease}/links"), None)
        .await
        .json();
    assert_eq!(links[0]["note"], "For the landlord");
    assert_eq!(links[0]["password"], true);
    assert_eq!(links[0]["downloads"], 1);
    assert_eq!(links[0]["opens"], 1);
}

#[tokio::test]
async fn only_owners_and_editors_make_links_and_removing_one_ends_it() {
    let server = start().await;
    let (alex, sam, kim) = (&server.alex, &server.sam, &server.kim);
    let top = id(server.api(alex, "GET", "/top", None).await);
    let shared = folder(&server, alex, &top, "School").await;
    for (person, level) in [(sam, "viewer"), (kim, "editor")] {
        let reply = server
            .api(
                alex,
                "PUT",
                &format!("/items/{shared}/shares/{}", person.id),
                Some(json!({ "level": level })),
            )
            .await;
        assert!(reply.status.is_success());
    }

    let by_viewer = server
        .api(
            sam,
            "POST",
            &format!("/items/{shared}/links"),
            Some(json!({ "expires": "1d" })),
        )
        .await;
    assert_eq!(by_viewer.status, StatusCode::FORBIDDEN);
    let path = link(&server, kim, &shared, json!({ "expires": "1y" })).await;
    assert_eq!(
        server.link("GET", &path, &[], "").await.status,
        StatusCode::OK
    );

    // My Drive itself can't be linked.
    let top_link = server
        .api(
            alex,
            "POST",
            &format!("/items/{top}/links"),
            Some(json!({ "expires": "1d" })),
        )
        .await;
    assert_eq!(top_link.status, StatusCode::BAD_REQUEST);

    // The owner removes the editor's link, and it stops at once.
    let links = server
        .api(alex, "GET", &format!("/items/{shared}/links"), None)
        .await
        .json();
    let link_id = links[0]["id"].as_str().unwrap();
    assert_eq!(links[0]["made_by_name"], "kim");
    let reply = server
        .api(sam, "DELETE", &format!("/links/{link_id}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = server
        .api(alex, "DELETE", &format!("/links/{link_id}"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    doesnt_work(&server.link("GET", &path, &[], "").await);

    // With link sharing off, there are no new links.
    server.link_origin.send_replace(None);
    let off = server
        .api(
            alex,
            "POST",
            &format!("/items/{shared}/links"),
            Some(json!({ "expires": "1d" })),
        )
        .await;
    assert_eq!(off.status, StatusCode::CONFLICT);
    // Only admins see everyone's links.
    let all = server.api(alex, "GET", "/links", None).await;
    assert_eq!(all.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn editors_see_links_and_shared_by_me_lists_linked_items() {
    let server = start().await;
    let (alex, kim) = (&server.alex, &server.kim);
    let top = id(server.api(alex, "GET", "/top", None).await);
    let school = folder(&server, alex, &top, "School").await;
    let taxes = file(&server, alex, &top, "taxes.pdf", "%PDF").await;
    let lease = file(&server, alex, &top, "lease.txt", "one year").await;
    let reply = server
        .api(
            alex,
            "PUT",
            &format!("/items/{school}/shares/{}", kim.id),
            Some(json!({ "level": "editor" })),
        )
        .await;
    assert!(reply.status.is_success());

    // An editor sees every link to the item, and may remove only their own.
    let kims_path = link(&server, kim, &school, json!({ "expires": "7d" })).await;
    let alexs_path = link(&server, alex, &school, json!({ "expires": "7d" })).await;
    let seen = server
        .api(kim, "GET", &format!("/items/{school}/links"), None)
        .await
        .json();
    let removable: Vec<(&str, bool)> = seen
        .as_array()
        .unwrap()
        .iter()
        .map(|link| {
            (
                link["made_by_name"].as_str().unwrap(),
                link["removable"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(removable, [("alex", false), ("kim", true)]);
    let owners = server
        .api(alex, "GET", &format!("/items/{school}/links"), None)
        .await
        .json();
    assert!(
        owners
            .as_array()
            .unwrap()
            .iter()
            .all(|link| link["removable"] == true)
    );

    // A link with no downloads left isn't sharing anything.
    link(&server, alex, &taxes, json!({ "expires": "30d" })).await;
    let used = link(
        &server,
        alex,
        &lease,
        json!({ "expires": "30d", "download_limit": 1 }),
    )
    .await;
    let download = server
        .link("GET", &format!("{used}/download/{lease}"), &[], "")
        .await;
    assert_eq!(download.status, StatusCode::OK);

    let out = server.api(alex, "GET", "/shared-by-me", None).await.json();
    let listed: Vec<(&str, usize, u64)> = out
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["name"].as_str().unwrap(),
                item["shared_with"].as_array().unwrap().len(),
                item["links"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(listed.contains(&("School", 1, 2)), "{listed:?}");
    assert!(listed.contains(&("taxes.pdf", 0, 1)), "{listed:?}");
    // Kim made a link, but to Alex's folder, so it's not Kim's to list.
    let kims = server.api(kim, "GET", "/shared-by-me", None).await.json();
    assert_eq!(kims.as_array().unwrap().len(), 0, "{kims}");

    // The owner's list marks a linked item as shared.
    let children = server
        .api(alex, "GET", &format!("/items/{top}/children"), None)
        .await
        .json();
    let shared: Vec<(&str, bool)> = children
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["name"].as_str().unwrap(), item["shared"] == true))
        .collect();
    assert!(shared.contains(&("taxes.pdf", true)), "{shared:?}");
    assert!(shared.contains(&("lease.txt", false)), "{shared:?}");

    // Stop sharing ends the people's access and every link, whoever made it.
    let reply = server
        .api(alex, "DELETE", &format!("/items/{school}/shares"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    doesnt_work(&server.link("GET", &kims_path, &[], "").await);
    doesnt_work(&server.link("GET", &alexs_path, &[], "").await);
    let out = server.api(alex, "GET", "/shared-by-me", None).await.json();
    assert_eq!(out.as_array().unwrap().len(), 1, "{out}");
    assert_eq!(out[0]["name"], "taxes.pdf");
}
