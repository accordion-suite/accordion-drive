//! Sync's addresses, the way a desktop app uses them (decision 23).

mod common;

use axum::http::StatusCode;
use common::{Person, Server, start};
use serde_json::{Value, json};

impl Server {
    async fn json(&self, person: &Person, method: &str, path: &str, body: Option<Value>) -> Value {
        let reply = self.api(person, method, path, body).await;
        assert!(
            reply.status.is_success(),
            "{method} {path}: {} {}",
            reply.status,
            reply.text()
        );
        reply.json()
    }

    /// Everything that changed after `cursor`, and the next cursor.
    async fn changes(&self, person: &Person, cursor: &str) -> (Vec<Value>, String) {
        let mut cursor = cursor.to_owned();
        let mut all = Vec::new();
        loop {
            let page = self
                .json(
                    person,
                    "GET",
                    &format!("/sync/changes?after={cursor}"),
                    None,
                )
                .await;
            all.extend(page["changes"].as_array().unwrap().iter().cloned());
            cursor = page["cursor"].as_str().unwrap().to_owned();
            if page["more"] == false {
                return (all, cursor);
            }
        }
    }

    /// Uploads `contents` as a resumable upload, for `finish` to save.
    async fn upload(&self, person: &Person, contents: &'static str) -> String {
        let upload = self
            .json(
                person,
                "POST",
                "/uploads",
                Some(json!({ "size": contents.len() })),
            )
            .await;
        let id = upload["id"].as_str().unwrap().to_owned();
        let reply = self
            .send(
                "PATCH",
                &format!("/api/drive/uploads/{id}"),
                &[("authorization", &person.bearer), ("upload-offset", "0")],
                contents,
            )
            .await;
        assert!(reply.status.is_success(), "{}", reply.text());
        id
    }

    async fn finish(
        &self,
        person: &Person,
        upload: &str,
        headers: &[(&str, &str)],
        body: Value,
    ) -> common::Reply {
        let mut all = vec![
            ("authorization", person.bearer.as_str()),
            ("content-type", "application/json"),
        ];
        all.extend_from_slice(headers);
        self.send(
            "POST",
            &format!("/api/drive/uploads/{upload}/finish"),
            &all,
            body.to_string(),
        )
        .await
    }
}

#[tokio::test]
async fn a_device_lists_everything_then_follows_changes() {
    let server = start().await;
    let start = server.json(&server.alex, "GET", "/sync/start", None).await;
    let top = start["roots"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(start["roots"][0]["access"], "owner");
    assert_eq!(start["roots"][0]["parent"], Value::Null);
    let cursor = start["cursor"].as_str().unwrap().to_owned();

    let folder = server
        .json(
            &server.alex,
            "POST",
            &format!("/items/{top}/children"),
            Some(json!({ "name": "Recipes" })),
        )
        .await;
    let folder = folder["id"].as_str().unwrap().to_owned();
    let reply = server
        .send(
            "PUT",
            &format!("/api/drive/items/{folder}/children/soup.md"),
            &[("authorization", &server.alex.bearer)],
            "# Soup",
        )
        .await;
    assert!(reply.status.is_success(), "{}", reply.text());
    let soup = reply.json();

    let listed = server
        .json(&server.alex, "GET", &format!("/sync/folders/{top}"), None)
        .await;
    assert_eq!(listed["items"].as_array().unwrap().len(), 2);
    assert_eq!(listed["next"], Value::Null);

    let (changes, cursor) = server.changes(&server.alex, &cursor).await;
    let names: Vec<&str> = changes
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Recipes", "soup.md"]);
    let soup_change = &changes[1];
    assert_eq!(soup_change["parent"], folder.as_str());
    assert_eq!(soup_change["sha256"], soup["sha256"]);
    assert_eq!(soup_change["size"], 6);
    assert!(soup_change.get("whole").is_none(), "left out when false");

    // Saving over contents the device saw works; over others, it's refused.
    let id = soup["id"].as_str().unwrap();
    let seen = soup["sha256"].as_str().unwrap();
    let upload = server.upload(&server.alex, "# Soup, with salt").await;
    let saved = server
        .finish(
            &server.alex,
            &upload,
            &[("if-match", &format!("\"{seen}\""))],
            json!({ "file": id }),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.text());
    let stale = server.upload(&server.alex, "# Soup, no salt").await;
    let refused = server
        .finish(
            &server.alex,
            &stale,
            &[("if-match", &format!("\"{seen}\""))],
            json!({ "file": id }),
        )
        .await;
    assert_eq!(refused.status, StatusCode::PRECONDITION_FAILED);
    let unsaid = server
        .finish(&server.alex, &stale, &[], json!({ "file": id }))
        .await;
    assert_eq!(unsaid.status, StatusCode::PRECONDITION_REQUIRED);
    let taken = server
        .finish(
            &server.alex,
            &stale,
            &[("if-none-match", "*")],
            json!({ "parent": folder, "name": "soup.md" }),
        )
        .await;
    assert_eq!(taken.status, StatusCode::CONFLICT);

    let (changes, _) = server.changes(&server.alex, &cursor).await;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["sha256"], saved.json()["sha256"]);
}

#[tokio::test]
async fn a_shared_folder_arrives_whole_and_leaves_as_gone() {
    let server = start().await;
    let alexs = server.json(&server.alex, "GET", "/sync/start", None).await;
    let top = alexs["roots"][0]["id"].as_str().unwrap().to_owned();
    let folder = server
        .json(
            &server.alex,
            "POST",
            &format!("/items/{top}/children"),
            Some(json!({ "name": "Holiday" })),
        )
        .await;
    let folder = folder["id"].as_str().unwrap().to_owned();
    let sams = server.json(&server.sam, "GET", "/sync/start", None).await;
    let cursor = sams["cursor"].as_str().unwrap().to_owned();

    let sam = server.sam.id;
    server
        .json(
            &server.alex,
            "PUT",
            &format!("/items/{folder}/shares/{sam}"),
            Some(json!({ "level": "viewer" })),
        )
        .await;
    let (changes, cursor) = server.changes(&server.sam, &cursor).await;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["id"], folder.as_str());
    assert_eq!(changes[0]["whole"], true);
    assert_eq!(changes[0]["access"], "viewer");
    assert_eq!(changes[0]["parent"], Value::Null);

    let unshared = server
        .api(
            &server.alex,
            "DELETE",
            &format!("/items/{folder}/shares/{sam}"),
            None,
        )
        .await;
    assert_eq!(unshared.status, StatusCode::NO_CONTENT);
    let (changes, _) = server.changes(&server.sam, &cursor).await;
    assert_eq!(changes, [json!({ "id": folder, "gone": true })]);
    let listed = server
        .api(&server.sam, "GET", &format!("/sync/folders/{folder}"), None)
        .await;
    assert_eq!(listed.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_cursor_the_server_cant_follow_says_start_over() {
    let server = start().await;
    let reply = server
        .api(&server.alex, "GET", "/sync/changes?after=nonsense", None)
        .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert!(reply.json()["error"].as_str().unwrap().contains("compare"));
}
