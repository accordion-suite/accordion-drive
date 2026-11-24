//! Importing Google Drive's files from Google Takeout (decision 34), from
//! made-up archives shaped like real exports, read in the sandbox as on a
//! server.

mod common;

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use accordion_core::takeout::Take;
use accordion_core::takeout::imports::{Import, State};
use accordion_types::{ItemId, UserId};
use common::{Server, start};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;
use zip::write::SimpleFileOptions;

/// An `-info.json` as Takeout writes it.
fn info(title: &str, id: &str, mime: &str, extra: Value) -> Vec<u8> {
    let mut info = json!({
        "title": title,
        "drive_id": id,
        "mime_type": mime,
        "starred": false,
        "created": "2023-05-01T08:00:00.000Z",
        "content_last_modified": "2023-05-06T09:30:00.000Z",
        "permissions": [{
            "type": "user",
            "role": "owner",
            "email_address": "sam@example.com",
        }],
        "download_url": format!("https://drive.google.com/file/d/{id}/view"),
    });
    for (key, value) in extra.as_object().unwrap() {
        info[key] = value.clone();
    }
    info.to_string().into_bytes()
}

const PDF: &str = "application/pdf";
const TEXT: &str = "text/plain";
const FOLDER: &str = "application/vnd.google-apps.folder";

/// The first part: most of the files, their infos, Takeout's folder of
/// kept versions, and the trash.
fn first_part() -> Vec<(&'static str, Vec<u8>)> {
    let starred = json!({
        "starred": true,
        "description": "Q3 numbers",
        "permissions": [
            { "type": "user", "role": "owner", "email_address": "sam@example.com" },
            { "type": "user", "role": "writer", "email_address": "alex@example.com" },
            { "type": "anyone", "role": "reader" },
        ],
    });
    vec![
        ("Takeout/Drive/Report.pdf", b"%PDF report".to_vec()),
        (
            "Takeout/Drive/Report.pdf-info.json",
            info("Report.pdf", "r1", PDF, starred),
        ),
        // Two files with one name, numbered by Takeout.
        (
            "Takeout/Drive/Report(1).pdf",
            b"%PDF another report".to_vec(),
        ),
        (
            "Takeout/Drive/Report.pdf-info(1).json",
            info("Report.pdf", "r2", PDF, json!({})),
        ),
        // A Google Doc, as Word, with Google's page of its comments.
        ("Takeout/Drive/Resume.docx", b"PK word".to_vec()),
        (
            "Takeout/Drive/Resume-info.json",
            info(
                "Resume",
                "d1",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                json!({ "download_url": "https://docs.google.com/document/d/d1/edit" }),
            ),
        ),
        (
            "Takeout/Drive/Resume-comments.html",
            b"<html>comments</html>".to_vec(),
        ),
        // Comments beside a file that isn't Word.
        ("Takeout/Drive/Notes.txt", b"notes".to_vec()),
        (
            "Takeout/Drive/Notes.txt-info.json",
            info("Notes.txt", "n1", TEXT, json!({})),
        ),
        (
            "Takeout/Drive/Notes.txt-comments.html",
            b"<html>comments</html>".to_vec(),
        ),
        // A name a `.tgz` cut short, with its info cut the same way.
        ("Takeout/Drive/A very long title th.txt", b"long".to_vec()),
        (
            "Takeout/Drive/A very long title th.json",
            info(
                "A very long title that a tgz cut short.txt",
                "l1",
                TEXT,
                json!({}),
            ),
        ),
        // No info at all.
        ("Takeout/Drive/loose.bin", b"loose".to_vec()),
        // A shortcut, and an info whose file isn't there.
        (
            "Takeout/Drive/Link.jpg.html",
            b"<html>bookmark</html>".to_vec(),
        ),
        (
            "Takeout/Drive/Link.jpg-info.json",
            info(
                "Link.jpg",
                "s1",
                "application/vnd.google-apps.shortcut",
                json!({ "download_url": "https://drive.google.com/file/d/s1/view" }),
            ),
        ),
        (
            "Takeout/Drive/Gone.pdf-info.json",
            info("Gone.pdf", "g1", PDF, json!({})),
        ),
        // Folders: one with files, one empty, which archives leave out.
        (
            "Takeout/Drive/Trip-info.json",
            info(
                "Trip",
                "t1",
                FOLDER,
                json!({ "created": "2020-01-02T03:04:05.000Z" }),
            ),
        ),
        ("Takeout/Drive/Trip/plan.txt", b"plan, final".to_vec()),
        (
            "Takeout/Drive/Trip/plan.txt-info.json",
            info("plan.txt", "p1", TEXT, json!({})),
        ),
        (
            "Takeout/Drive/Trip/plan-at-2023-05-02T10_00_00.000Z-pinned.txt",
            b"plan, first draft".to_vec(),
        ),
        (
            "Takeout/Drive/Trip/photo-at-2023-05-02T10_00_00.000Z-pinned.jpg",
            b"\xff\xd8\xff photo".to_vec(),
        ),
        (
            "Takeout/Drive/Trip/photo.jpg-info.json",
            info("photo.jpg", "ph1", "image/jpeg", json!({})),
        ),
        (
            "Takeout/Drive/Empty-info.json",
            info("Empty", "e1", FOLDER, json!({})),
        ),
        // Takeout's folder of kept versions: one of the first report, and
        // one whose file isn't in the export.
        (
            "Takeout/Drive/Unorganized/Report-at-2023-05-03T00_00_00.000Z-pinned.pdf",
            b"%PDF report, early".to_vec(),
        ),
        (
            "Takeout/Drive/Unorganized/Lost-at-2023-05-03T00_00_00.000Z-pinned.txt",
            b"lost".to_vec(),
        ),
        // The trash: a file, and a folder with a file in it.
        ("Takeout/Drive/Trash/Old.txt", b"old".to_vec()),
        (
            "Takeout/Drive/Trash/Old.txt-info.json",
            info("Old.txt", "o1", TEXT, json!({})),
        ),
        (
            "Takeout/Drive/Trash/Box-info.json",
            info("Box", "b1", FOLDER, json!({})),
        ),
        ("Takeout/Drive/Trash/Box/inner.txt", b"inner".to_vec()),
        (
            "Takeout/Drive/Trash/Box/inner.txt-info.json",
            info("inner.txt", "i1", TEXT, json!({})),
        ),
    ]
}

/// The second part, with a picture whose info is in the first.
fn second_part() -> Vec<(&'static str, Vec<u8>)> {
    vec![(
        "Takeout/Drive/Trip/photo.jpg",
        b"\xff\xd8\xff photo".to_vec(),
    )]
}

fn zip_part(path: &Path, files: &[(&str, Vec<u8>)]) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, contents) in files {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(contents).unwrap();
    }
    zip.finish().unwrap();
}

struct Imported {
    server: Server,
    person: UserId,
    parts: Vec<ItemId>,
    _dir: tempfile::TempDir,
}

async fn with_parts() -> Imported {
    let server = start().await;
    let person = server.sam.id;
    let dir = tempfile::tempdir().unwrap();
    let top = server.storage.top_folder(person).await.unwrap();
    let mut parts = Vec::new();
    for (name, files) in [
        ("takeout-1.zip", first_part()),
        ("takeout-2.zip", second_part()),
    ] {
        let path = dir.path().join(name);
        zip_part(&path, &files);
        let file = tokio::fs::File::open(&path).await.unwrap();
        parts.push(
            server
                .storage
                .create_file(top, name, file)
                .await
                .unwrap()
                .id,
        );
    }
    Imported {
        server,
        person,
        parts,
        _dir: dir,
    }
}

impl Imported {
    async fn import(&self) -> Import {
        let imports = &self.server.imports;
        imports
            .start(self.person, &self.parts, &[Take::Product("drive".into())])
            .await
            .unwrap();
        let (ended, end) = oneshot::channel::<()>();
        let runner = tokio::spawn(imports.clone().run(async move {
            let _ = end.await;
        }));
        let latest = loop {
            let latest = imports.list(self.person, 1).await.unwrap().remove(0);
            if !matches!(latest.state, State::Running | State::Canceling) {
                break latest;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let _ = ended.send(());
        tokio::time::timeout(Duration::from_secs(60), runner)
            .await
            .unwrap()
            .unwrap();
        latest
    }

    async fn find(&self, path: &[&str]) -> accordion_core::storage::Item {
        let top = self.server.storage.top_folder(self.person).await.unwrap();
        self.server
            .storage
            .find(top, path.iter().map(|name| (*name).to_owned()).collect())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{path:?} isn't there"))
    }

    async fn names(&self, path: &[&str]) -> Vec<String> {
        let folder = self.find(path).await;
        let mut names: Vec<String> = self
            .server
            .storage
            .children(folder.id)
            .await
            .unwrap()
            .into_iter()
            .map(|item| item.name)
            .collect();
        names.sort();
        names
    }

    async fn text(&self, id: ItemId) -> String {
        let mut text = String::new();
        self.server
            .storage
            .open_file(id)
            .await
            .unwrap()
            .read_to_string(&mut text)
            .await
            .unwrap();
        text
    }
}

#[tokio::test]
async fn imports_google_drives_files_into_drive() {
    let imported = with_parts().await;
    let import = imported.import().await;
    assert_eq!(import.state, State::Done, "{:?}", import.error);

    assert_eq!(
        imported.names(&["From Google Drive"]).await,
        [
            "A very long title that a tgz cut short.txt",
            "Empty",
            "Notes.txt",
            "Report (2).pdf",
            "Report.pdf",
            "Resume.docx",
            "Trash",
            "Trip",
            "loose.bin",
        ]
    );
    assert_eq!(
        imported.names(&["From Google Drive", "Trip"]).await,
        ["photo.jpg", "plan.txt"]
    );
    // The trash's files are in Accordion's trash.
    assert!(
        imported
            .names(&["From Google Drive", "Trash"])
            .await
            .is_empty()
    );
    let trash: Vec<String> = imported
        .server
        .storage
        .trash(imported.person)
        .await
        .unwrap()
        .into_iter()
        .map(|item| item.name)
        .collect();
    assert!(trash.contains(&"Old.txt".to_owned()), "{trash:?}");
    assert!(trash.contains(&"Box".to_owned()), "{trash:?}");

    // Names, dates, stars, and descriptions from the infos.
    let report = imported.find(&["From Google Drive", "Report.pdf"]).await;
    assert!(
        report.modified_at.starts_with("2023-05-06"),
        "{}",
        report.modified_at
    );
    assert!(
        report.created_at.starts_with("2023-05-01"),
        "{}",
        report.created_at
    );
    let trip = imported.find(&["From Google Drive", "Trip"]).await;
    assert!(
        trip.created_at.starts_with("2020-01-02"),
        "{}",
        trip.created_at
    );
    let sam = &imported.server.sam;
    let starred = imported
        .server
        .api(sam, "GET", "/starred", None)
        .await
        .json();
    assert_eq!(starred.as_array().unwrap().len(), 1, "{starred}");
    let description = imported
        .server
        .api(
            sam,
            "GET",
            &format!("/items/{}/description", report.id),
            None,
        )
        .await
        .json();
    assert_eq!(description["description"], "Q3 numbers");

    // Kept versions that differ are older versions; those that match the
    // file are dropped.
    let storage = &imported.server.storage;
    let plan = imported
        .find(&["From Google Drive", "Trip", "plan.txt"])
        .await;
    let photo = imported
        .find(&["From Google Drive", "Trip", "photo.jpg"])
        .await;
    let photo_versions = storage.versions(photo.id).await.unwrap().len();
    assert_eq!(
        storage.versions(plan.id).await.unwrap().len(),
        photo_versions + 1
    );
    assert_eq!(
        storage.versions(report.id).await.unwrap().len(),
        photo_versions + 1
    );
    assert_eq!(imported.text(plan.id).await, "plan, final");

    assert_eq!(
        imported
            .names(&["Google Takeout", "Unconverted", "Drive"])
            .await,
        ["Notes.txt-comments.html", "Unorganized"]
    );
    assert_eq!(
        imported
            .names(&["Google Takeout", "Unconverted", "Drive", "Unorganized"])
            .await,
        ["Lost-at-2023-05-03T00_00_00.000Z-pinned.txt"]
    );

    let report = imported.text(import.report.unwrap()).await;
    for line in [
        "files and folders from Google Drive, in **From Google Drive** in Drive\n",
        "- Notes.txt-comments.html: it's Google's page of comments on a file that isn't a Word",
        "- Unorganized/Lost-at-2023-05-03T00\\_00\\_00.000Z-pinned.txt: it's a kept version of a file",
        "## Files missing from the archives\n\n- Gone.pdf\n",
        "## Shortcuts\n\n- Link.jpg: <https://drive.google.com/file/d/s1/view>\n",
        "## Files shared with you\n\n- Takeout leaves out files",
        "## Files that were shared\n\n- Report.pdf: alex@example.com (editor), anyone with the \
         link (viewer)\n",
        "## Google Docs, Sheets, and Slides\n\n- Resume.docx, a Google Doc\n",
        "## Comments\n\n- Resume.docx: its comments are in the Word file",
        "## Files without details from Google\n\n- loose.bin: it keeps its name",
    ] {
        assert!(report.contains(line), "{line:?} isn't in:\n{report}");
    }

    // Importing again brings nothing twice.
    let again = imported.import().await;
    assert_eq!(again.state, State::Done);
    let report = imported.text(again.report.unwrap()).await;
    assert!(report.contains("Nothing new came over."), "{report}");
    assert_eq!(imported.names(&["From Google Drive"]).await.len(), 9);
}
