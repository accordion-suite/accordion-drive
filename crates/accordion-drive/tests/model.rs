//! Model tests for sync (decision 23): three devices and the browser change
//! one person's files at random, devices sync in random order, requests
//! fail, sometimes after the server did the work, and devices crash partway
//! through a pass. At the end, with nothing failing, every device must match
//! the server, and everything a device synced or the browser saved must
//! still be in Drive: as a file, an old version, or in the trash.
//!
//! The devices run the real engine and client from `accordion-sync`,
//! against Drive's real addresses over loopback. Each bug a seed finds gets
//! a test of its own at the end of this file, so it stays fixed even if the
//! random runs stop reaching it.
//!
//! Each run is repeatable from its seed. To run more, or longer:
//!
//! ```sh
//! ACCORDION_MODEL_SEEDS=1-20 ACCORDION_MODEL_STEPS=2000 \
//!     cargo test --test model -- --nocapture
//! ```

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use accordion_core::storage::{ItemKind, Storage};
use accordion_sync::client::Client;
use accordion_sync::engine::{Engine, MY_DRIVE, hash_file};
use accordion_sync::state::State;
use accordion_types::ItemId;
use axum::body::Body;
use axum::extract::{Request, State as Extract};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use common::{Person, Server, start};
use serde_json::json;

/// A small, seeded random number generator (SplitMix64), so a failing run
/// can be repeated exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn percent(&mut self, chance: u64) -> bool {
        self.next() % 100 < chance
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            Some(&items[self.below(items.len())])
        }
    }
}

/// Names from a short list, so devices and the browser often pick the same
/// one and have to sort it out.
const FILES: &[&str] = &["notes.txt", "list.md", "photo.jpg", "a.txt", "b.txt"];
const FOLDERS: &[&str] = &["Work", "Trips", "Old", "Recipes"];

/// What makes requests fail while it's on.
struct Faults {
    on: AtomicBool,
    rng: Mutex<Rng>,
}

/// In one request in 12, the device never hears back: half the time before
/// the server does anything, and half the time after it's done the work.
async fn flaky(Extract(faults): Extract<Arc<Faults>>, request: Request, next: Next) -> Response {
    let roll = if faults.on.load(Ordering::Relaxed) {
        faults.rng.lock().unwrap().below(24)
    } else {
        24
    };
    if roll == 0 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let response = next.run(request).await;
    if roll == 1 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    response
}

/// Stops a future after it's been polled `left` times, the way a crash
/// stops the app wherever it was.
struct Crash<F> {
    inner: Pin<Box<F>>,
    left: u32,
}

impl<F: Future> Future for Crash<F> {
    type Output = Option<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.left == 0 {
            return Poll::Ready(None);
        }
        self.left -= 1;
        self.inner.as_mut().poll(cx).map(Some)
    }
}

struct Device {
    name: String,
    root: PathBuf,
    db: PathBuf,
    engine: Option<Engine>,
}

impl Device {
    fn engine(&mut self, origin: &str, token: &str) -> &mut Engine {
        self.engine.get_or_insert_with(|| {
            Engine::new(
                Client::new(origin, Some(token.to_owned())).unwrap(),
                State::open(&self.db).unwrap(),
                self.root.clone(),
                self.name.clone(),
            )
        })
    }

    fn my_drive(&self) -> PathBuf {
        self.root.join(MY_DRIVE)
    }
}

/// What's in a folder tree: each path, and the hash of each file.
type Tree = BTreeMap<String, Option<String>>;

/// A device's My Drive, leaving out the engine's own files.
fn local_tree(root: &Path) -> Tree {
    let mut tree = Tree::new();
    fn walk(at: &Path, prefix: &str, tree: &mut Tree) {
        let Ok(entries) = fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".accordion-") {
                continue;
            }
            let path = format!("{prefix}{name}");
            if entry.file_type().unwrap().is_dir() {
                tree.insert(path.clone(), None);
                walk(&entry.path(), &format!("{path}/"), tree);
            } else {
                tree.insert(path, Some(hash_file(&entry.path()).unwrap()));
            }
        }
    }
    walk(root, "", &mut tree);
    tree
}

/// The server's My Drive.
async fn server_tree(storage: &Storage, top: ItemId) -> Tree {
    let mut tree = Tree::new();
    let mut folders = vec![(top, String::new())];
    while let Some((folder, prefix)) = folders.pop() {
        for child in storage.children(folder).await.unwrap() {
            let path = format!("{prefix}{}", child.name);
            match child.kind {
                ItemKind::Folder => {
                    tree.insert(path.clone(), None);
                    folders.push((child.id, format!("{path}/")));
                }
                ItemKind::File { blob, .. } => {
                    tree.insert(path, Some(blob.hash.to_hex()));
                }
            }
        }
    }
    tree
}

/// Every contents Drive still holds for the person: each version of each
/// file, in My Drive or the trash.
async fn kept(storage: &Storage, server: &Server, top: ItemId) -> BTreeSet<String> {
    let mut kept = BTreeSet::new();
    let mut folders = vec![top];
    let mut files = Vec::new();
    for item in storage.trash(server.alex.id).await.unwrap() {
        match item.kind {
            ItemKind::Folder => folders.push(item.id),
            ItemKind::File { .. } => files.push(item.id),
        }
    }
    while let Some(folder) = folders.pop() {
        for child in storage.children(folder).await.unwrap() {
            match child.kind {
                ItemKind::Folder => folders.push(child.id),
                ItemKind::File { .. } => files.push(child.id),
            }
        }
    }
    for file in files {
        for version in storage.versions(file).await.unwrap() {
            kept.insert(version.blob.hash.to_hex());
        }
    }
    kept
}

/// What differs between a device and the server, with what the device's
/// state says about each, and where the server has it now.
async fn differences(storage: &Storage, device: &Device, tree: &Tree, expected: &Tree) -> String {
    let state = State::open(&device.db).unwrap();
    let mut lines = Vec::new();
    for (path, hash) in tree {
        let side = match expected.get(path) {
            None => "only on the device",
            Some(other) if other != hash => "different contents",
            Some(_) => continue,
        };
        let known = state.at_path(&format!("{MY_DRIVE}/{path}")).unwrap();
        let server = match &known {
            Some(known) => where_on_server(storage, known.id.parse().unwrap()).await,
            None => "not known".into(),
        };
        lines.push(format!(
            "{path}: {side}; state {known:?}; server has it at {server}"
        ));
        // Everything the device knows in the same folder.
        let folder = path.rsplit_once('/').map_or("", |(folder, _)| folder);
        for item in state.all().unwrap() {
            let inside = item
                .path
                .strip_prefix(&format!("{MY_DRIVE}/"))
                .unwrap_or(&item.path);
            if inside.rsplit_once('/').map_or("", |(folder, _)| folder) == folder {
                let server = where_on_server(storage, item.id.parse().unwrap()).await;
                lines.push(format!(
                    "  known here: {} ({}) is {server}",
                    item.path, item.id
                ));
            }
        }
    }
    for path in expected.keys() {
        if !tree.contains_key(path) {
            lines.push(format!("{path}: only on the server"));
        }
    }
    lines.join("\n")
}

/// An item's path on the server, marking anything in the trash.
async fn where_on_server(storage: &Storage, id: ItemId) -> String {
    let mut names = Vec::new();
    let mut next = Some(id);
    while let Some(id) = next {
        let Some(item) = storage.item(id).await.unwrap() else {
            names.push("(deleted for good)".to_owned());
            break;
        };
        let mark = if item.trashed_at.is_some() {
            " (trashed)"
        } else {
            ""
        };
        names.push(format!("{}{mark}", item.name));
        next = item.parent;
    }
    names.reverse();
    names.join("/")
}

/// Everything in a device's My Drive, as paths relative to it.
fn entries(root: &Path) -> (Vec<String>, Vec<String>) {
    let tree = local_tree(root);
    let folders = tree
        .iter()
        .filter(|(_, hash)| hash.is_none())
        .map(|(path, _)| path.clone())
        .collect();
    let files = tree
        .iter()
        .filter(|(_, hash)| hash.is_some())
        .map(|(path, _)| path.clone())
        .collect();
    (folders, files)
}

/// Writes a file the way an app saves one. Its time is set a little in the
/// past, as if it was saved a moment ago, so the engine doesn't wait for it
/// to settle.
fn write(at: &Path, contents: &str) {
    fs::write(at, contents).unwrap();
    let file = fs::File::options().write(true).open(at).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(5))
        .unwrap();
}

/// Contents that no other write uses, and of varied length.
fn contents(rng: &mut Rng, seed: u64, step: usize, who: &str) -> String {
    let padding = "x".repeat(rng.below(40));
    format!("seed {seed}, step {step}, {who} {padding}\n")
}

/// A folder in My Drive to put something in: My Drive itself, or a folder
/// in it.
fn some_folder(rng: &mut Rng, folders: &[String]) -> String {
    if rng.percent(40) {
        String::new()
    } else {
        rng.pick(folders).cloned().unwrap_or_default()
    }
}

fn join(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.to_owned()
    } else {
        format!("{folder}/{name}")
    }
}

/// One change on a device, made while it isn't syncing.
fn change_locally(rng: &mut Rng, device: &Device, seed: u64, step: usize) -> String {
    let root = device.my_drive();
    if !root.exists() {
        return "nothing yet".into();
    }
    let (folders, files) = entries(&root);
    match rng.below(100) {
        // A new file, or one saved over another of the same name.
        0..30 => {
            let path = join(&some_folder(rng, &folders), rng.pick(FILES).unwrap());
            let at = root.join(&path);
            if at.is_dir() {
                return format!("didn't save over the folder {path}");
            }
            let text = contents(rng, seed, step, &device.name);
            write(&at, &text);
            format!("saved {path} ({})", &hash_of(&text)[..8])
        }
        // An edit.
        30..60 => {
            let Some(file) = rng.pick(&files) else {
                return "no file to edit".into();
            };
            let text = contents(rng, seed, step, &device.name);
            write(&root.join(file), &text);
            format!("edited {file} ({})", &hash_of(&text)[..8])
        }
        60..70 => {
            let path = join(&some_folder(rng, &folders), rng.pick(FOLDERS).unwrap());
            match fs::create_dir(root.join(&path)) {
                Ok(()) => format!("made the folder {path}"),
                Err(_) => format!("didn't make {path}, which is there"),
            }
        }
        // A rename or a move, of a file or a folder.
        70..85 => {
            let all: Vec<String> = folders.iter().chain(&files).cloned().collect();
            let Some(from) = rng.pick(&all).cloned() else {
                return "nothing to move".into();
            };
            let into = some_folder(rng, &folders);
            if into == from || into.starts_with(&format!("{from}/")) {
                return format!("didn't move {from} into itself");
            }
            let name = if folders.contains(&from) {
                rng.pick(FOLDERS).unwrap()
            } else {
                rng.pick(FILES).unwrap()
            };
            let to = join(&into, name);
            if root.join(&to).exists() {
                return format!("didn't move {from} to {to}, which is there");
            }
            fs::rename(root.join(&from), root.join(&to)).unwrap();
            format!("moved {from} to {to}")
        }
        85..95 => {
            let Some(file) = rng.pick(&files) else {
                return "no file to delete".into();
            };
            fs::remove_file(root.join(file)).unwrap();
            format!("deleted {file}")
        }
        _ => {
            let Some(folder) = rng.pick(&folders) else {
                return "no folder to delete".into();
            };
            fs::remove_dir_all(root.join(folder)).unwrap();
            format!("deleted the folder {folder}")
        }
    }
}

/// One change in the browser. The server may refuse some, such as a name
/// that's taken, as it would in the browser.
async fn change_in_browser(
    rng: &mut Rng,
    server: &Server,
    top: ItemId,
    seed: u64,
    step: usize,
    saved: &mut BTreeSet<String>,
) -> String {
    let tree = server_tree(&server.storage, top).await;
    let mut folders = vec![(top, String::new())];
    let mut files = Vec::new();
    let mut paths = BTreeMap::new();
    for (path, hash) in &tree {
        let found = server
            .storage
            .find(top, path.split('/').map(str::to_owned).collect())
            .await
            .unwrap()
            .unwrap();
        paths.insert(found.id, path.clone());
        if hash.is_none() {
            folders.push((found.id, path.clone()));
        } else {
            files.push(found.id);
        }
    }
    let alex: &Person = &server.alex;
    match rng.below(100) {
        0..30 => {
            let (folder, at) = rng.pick(&folders).cloned().unwrap();
            let name = rng.pick(FILES).unwrap();
            let text = contents(rng, seed, step, "browser");
            let reply = server
                .send(
                    "PUT",
                    &format!("/api/drive/items/{folder}/children/{name}"),
                    &[("authorization", &alex.bearer)],
                    Body::from(text.clone()),
                )
                .await;
            if reply.status.is_success() {
                saved.insert(hash_of(&text));
            }
            format!(
                "saved {} ({}): {}",
                join(&at, name),
                &hash_of(&text)[..8],
                reply.status
            )
        }
        30..60 => {
            let Some(file) = rng.pick(&files) else {
                return "no file to edit".into();
            };
            let text = contents(rng, seed, step, "browser");
            let reply = server
                .send(
                    "PUT",
                    &format!("/api/drive/items/{file}/content"),
                    &[("authorization", &alex.bearer)],
                    Body::from(text.clone()),
                )
                .await;
            if reply.status.is_success() {
                saved.insert(hash_of(&text));
            }
            format!(
                "edited {} ({}): {}",
                paths[file],
                &hash_of(&text)[..8],
                reply.status
            )
        }
        60..70 => {
            let (folder, at) = rng.pick(&folders).cloned().unwrap();
            let name = rng.pick(FOLDERS).unwrap();
            let reply = server
                .api(
                    alex,
                    "POST",
                    &format!("/items/{folder}/children"),
                    Some(json!({ "name": name })),
                )
                .await;
            format!("made the folder {}: {}", join(&at, name), reply.status)
        }
        70..85 => {
            let movable: Vec<ItemId> = folders
                .iter()
                .skip(1)
                .map(|(id, _)| *id)
                .chain(files.iter().copied())
                .collect();
            let Some(item) = rng.pick(&movable) else {
                return "nothing to move".into();
            };
            {
                let (into, at) = rng.pick(&folders).cloned().unwrap();
                let is_folder = folders.iter().any(|(id, _)| id == item);
                let name = if is_folder {
                    rng.pick(FOLDERS).unwrap()
                } else {
                    rng.pick(FILES).unwrap()
                };
                // The server refuses moving a folder into itself.
                let reply = server
                    .api(
                        alex,
                        "PATCH",
                        &format!("/items/{item}"),
                        Some(json!({ "name": name, "parent": into.to_string() })),
                    )
                    .await;
                format!(
                    "moved {} to {}: {}",
                    paths[item],
                    join(&at, name),
                    reply.status
                )
            }
        }
        _ => {
            let all: Vec<ItemId> = folders
                .iter()
                .skip(1)
                .map(|(id, _)| *id)
                .chain(files.iter().copied())
                .collect();
            let Some(item) = rng.pick(&all) else {
                return "nothing to delete".into();
            };
            let reply = server
                .api(alex, "DELETE", &format!("/items/{item}"), None)
                .await;
            format!("deleted {}: {}", paths[item], reply.status)
        }
    }
}

fn hash_of(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The seeds and steps to run, from the environment or a short default for
/// CI.
fn plan() -> (Vec<u64>, usize) {
    let seeds = std::env::var("ACCORDION_MODEL_SEEDS").unwrap_or_else(|_| "1-4".into());
    let seeds = match seeds.split_once('-') {
        Some((first, last)) => (first.parse().unwrap()..=last.parse().unwrap()).collect(),
        None => seeds.split(',').map(|seed| seed.parse().unwrap()).collect(),
    };
    let steps = std::env::var("ACCORDION_MODEL_STEPS")
        .ok()
        .map_or(250, |steps| steps.parse().unwrap());
    (seeds, steps)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn devices_end_up_matching_and_nothing_synced_is_lost() {
    let (seeds, steps) = plan();
    for seed in seeds {
        run(seed, steps).await;
    }
}

async fn run(seed: u64, steps: usize) {
    let server = start().await;
    let faults = Arc::new(Faults {
        on: AtomicBool::new(false),
        rng: Mutex::new(Rng(seed ^ 0xfa17)),
    });
    let app = server
        .router
        .clone()
        .layer(middleware::from_fn_with_state(faults.clone(), flaky));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let token = server
        .alex
        .bearer
        .strip_prefix("Bearer ")
        .unwrap()
        .to_owned();
    let top = server.storage.top_folder(server.alex.id).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let mut devices: Vec<Device> = ["Laptop", "Desktop", "Tablet"]
        .iter()
        .map(|name| Device {
            name: (*name).to_owned(),
            root: dir.path().join(name),
            db: dir.path().join(format!("{name}.db")),
            engine: None,
        })
        .collect();

    let mut rng = Rng(seed);
    // Contents that a device had when a pass finished cleanly, or that the
    // browser saved.
    let mut saved = BTreeSet::new();
    let mut log = Vec::new();
    for step in 0..steps {
        let which = rng.below(devices.len());
        match rng.below(100) {
            0..35 => {
                let device = &mut devices[which];
                let before = local_tree(&device.my_drive());
                faults.on.store(rng.percent(50), Ordering::Relaxed);
                let crash = rng.percent(15).then(|| 1 + rng.below(60) as u32);
                let engine = device.engine(&origin, &token);
                let pass = Crash {
                    inner: Box::pin(engine.sync()),
                    left: crash.unwrap_or(u32::MAX),
                };
                let outcome = pass.await;
                faults.on.store(false, Ordering::Relaxed);
                match outcome {
                    None => {
                        log.push(format!("{step}: {} crashed", device.name));
                        // It starts again from what it saved.
                        device.engine = None;
                    }
                    Some(Err(err)) => log.push(format!("{step}: {} failed: {err}", device.name)),
                    Some(Ok(report)) => {
                        log.push(format!(
                            "{step}: {} synced: {:?} {:?}",
                            device.name, report.done, report.notices
                        ));
                        if report
                            .notices
                            .iter()
                            .all(|notice| !notice.starts_with("Couldn't"))
                            && report.paused_deletes == 0
                        {
                            saved.extend(before.into_values().flatten());
                        }
                    }
                }
            }
            35..85 => {
                let did = change_locally(&mut rng, &devices[which], seed, step);
                log.push(format!("{step}: {} {did}", devices[which].name));
            }
            _ => {
                let did = change_in_browser(&mut rng, &server, top, seed, step, &mut saved).await;
                log.push(format!("{step}: the browser {did}"));
            }
        }
    }

    // With nothing failing, every device catches up, until a whole round
    // changes nothing. The engine leaves files changed in the last couple
    // of seconds for its next pass, including ones it just downloaded, so
    // the devices wait that long first, as they would between passes.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut settled = false;
    for _ in 0..8 {
        let mut quiet = true;
        for device in &mut devices {
            let report = device
                .engine(&origin, &token)
                .sync()
                .await
                .unwrap_or_else(|err| panic!("seed {seed}: {} couldn't sync: {err}", device.name));
            log.push(format!(
                "end: {} synced: {:?} {:?}",
                device.name, report.done, report.notices
            ));
            if !report.done.is_empty() || report.paused_deletes > 0 {
                quiet = false;
            }
        }
        if quiet {
            settled = true;
            break;
        }
    }
    // The whole log, to follow how a failure came about.
    let tail = log.join("\n");
    assert!(
        settled,
        "seed {seed}: the devices kept changing things after 8 rounds.\n{tail}"
    );

    let expected = server_tree(&server.storage, top).await;
    for device in &devices {
        let tree = local_tree(&device.my_drive());
        if tree != expected {
            let report = differences(&server.storage, device, &tree, &expected).await;
            panic!(
                "seed {seed}: {} doesn't match the server.\n{report}\n{tail}",
                device.name
            );
        }
    }

    let kept = kept(&server.storage, &server, top).await;
    let lost: Vec<&String> = saved.difference(&kept).collect();
    assert!(
        lost.is_empty(),
        "seed {seed}: {} saved contents aren't anywhere in Drive: {lost:?}\n{tail}",
        lost.len()
    );
    println!(
        "seed {seed}: {steps} steps, {} items, {} contents kept",
        expected.len(),
        kept.len()
    );
}

/// Serves Drive on a port on this computer, for devices to sync with.
async fn serve(server: &Server) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = server.router.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    origin
}

fn device(dir: &Path, name: &str) -> Device {
    Device {
        name: name.to_owned(),
        root: dir.join(name),
        db: dir.join(format!("{name}.db")),
        engine: None,
    }
}

impl Server {
    async fn folder(&self, parent: ItemId, name: &str) -> ItemId {
        let reply = self
            .api(
                &self.alex,
                "POST",
                &format!("/items/{parent}/children"),
                Some(json!({ "name": name })),
            )
            .await;
        assert!(reply.status.is_success(), "{}", reply.text());
        reply.json()["id"].as_str().unwrap().parse().unwrap()
    }

    async fn file(&self, parent: ItemId, name: &str, contents: &'static str) {
        let reply = self
            .send(
                "PUT",
                &format!("/api/drive/items/{parent}/children/{name}"),
                &[("authorization", &self.alex.bearer)],
                contents,
            )
            .await;
        assert!(reply.status.is_success(), "{}", reply.text());
    }
}

/// Found by seed 1: a folder deleted on a device, while the browser moved
/// it and added a folder inside, came back on the device at its old place.
#[tokio::test]
async fn a_folder_deleted_here_stays_deleted_when_it_moved_in_accordion() {
    let server = start().await;
    let origin = serve(&server).await;
    let token = server.alex.bearer["Bearer ".len()..].to_owned();
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = device(dir.path(), "Laptop");

    let trips = server.folder(top, "Trips").await;
    let work = server.folder(trips, "Work").await;
    laptop.engine(&origin, &token).sync().await.unwrap();
    assert!(laptop.my_drive().join("Trips/Work").is_dir());

    let reply = server
        .api(
            &server.alex,
            "PATCH",
            &format!("/items/{trips}"),
            Some(json!({ "name": "Old" })),
        )
        .await;
    assert!(reply.status.is_success(), "{}", reply.text());
    server.folder(work, "New").await;
    fs::remove_dir_all(laptop.my_drive().join("Trips")).unwrap();

    for _ in 0..3 {
        laptop.engine(&origin, &token).sync().await.unwrap();
    }
    assert!(!laptop.my_drive().join("Trips").exists());
    assert_eq!(
        local_tree(&laptop.my_drive()),
        server_tree(&server.storage, top).await
    );
}

/// Found by seed 4: a folder deleted in Accordion, and a new one with the
/// same name, became one folder on a device that synced both at once, which
/// kept the old folder's files.
#[tokio::test]
async fn a_new_folder_with_a_deleted_folders_name_stays_separate() {
    let server = start().await;
    let origin = serve(&server).await;
    let token = server.alex.bearer["Bearer ".len()..].to_owned();
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut desktop = device(dir.path(), "Desktop");

    let trips = server.folder(top, "Trips").await;
    server.file(trips, "notes.txt", "packing list").await;
    desktop.engine(&origin, &token).sync().await.unwrap();
    assert!(desktop.my_drive().join("Trips/notes.txt").is_file());

    let reply = server
        .api(&server.alex, "DELETE", &format!("/items/{trips}"), None)
        .await;
    assert!(reply.status.is_success(), "{}", reply.text());
    let again = server.folder(top, "Trips").await;
    server.file(again, "photo.jpg", "a photo").await;

    for _ in 0..3 {
        desktop.engine(&origin, &token).sync().await.unwrap();
    }
    assert!(!desktop.my_drive().join("Trips/notes.txt").exists());
    assert_eq!(
        local_tree(&desktop.my_drive()),
        server_tree(&server.storage, top).await
    );
}

/// Found by hand on November 5, 2026: a file in Accordion with a name this
/// device ignores, such as a note Windows adds to downloaded files, was
/// downloaded, then read as deleted here, and moved to the trash.
#[tokio::test]
async fn a_file_with_a_name_this_device_ignores_stays_in_accordion() {
    let server = start().await;
    let origin = serve(&server).await;
    let token = server.alex.bearer["Bearer ".len()..].to_owned();
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = device(dir.path(), "Laptop");

    server.file(top, "photo.jpg", "a photo").await;
    server
        .file(top, "photo.jpg:Zone.Identifier", "[ZoneTransfer]")
        .await;
    for _ in 0..3 {
        laptop.engine(&origin, &token).sync().await.unwrap();
    }
    assert!(laptop.my_drive().join("photo.jpg").is_file());
    assert!(!laptop.my_drive().join("photo.jpg:Zone.Identifier").exists());
    assert!(
        server
            .storage
            .find(top, vec!["photo.jpg:Zone.Identifier".into()])
            .await
            .unwrap()
            .is_some(),
        "the note was moved to the trash"
    );
}

/// Found by seed 28 at 1,000 steps: a file moved out of a folder in
/// Accordion, while a device moved the folder, stayed in the folder on the
/// device, which thought the server had moved it there too.
#[tokio::test]
async fn a_file_moved_out_of_a_folder_moved_here_follows_this_device() {
    let server = start().await;
    let origin = serve(&server).await;
    let token = server.alex.bearer["Bearer ".len()..].to_owned();
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut tablet = device(dir.path(), "Tablet");

    let old = server.folder(top, "Old").await;
    let work = server.folder(old, "Work").await;
    let recipes = server.folder(top, "Recipes").await;
    server.file(work, "b.txt", "soup").await;
    tablet.engine(&origin, &token).sync().await.unwrap();

    let file = server
        .storage
        .find(work, vec!["b.txt".into()])
        .await
        .unwrap()
        .unwrap();
    let reply = server
        .api(
            &server.alex,
            "PATCH",
            &format!("/items/{}", file.id),
            Some(json!({ "name": "notes.txt", "parent": recipes.to_string() })),
        )
        .await;
    assert!(reply.status.is_success(), "{}", reply.text());
    fs::rename(
        tablet.my_drive().join("Old/Work"),
        tablet.my_drive().join("Trips"),
    )
    .unwrap();

    for _ in 0..3 {
        tablet.engine(&origin, &token).sync().await.unwrap();
    }
    assert_eq!(
        local_tree(&tablet.my_drive()),
        server_tree(&server.storage, top).await
    );
}

/// A folder made again here, around a file edited in Accordion after the
/// folder was deleted here, still moves as itself later, rather than as a
/// new folder with the old one's contents moved into it.
#[tokio::test]
async fn a_folder_brought_back_here_still_moves_as_itself() {
    let server = start().await;
    let origin = serve(&server).await;
    let token = server.alex.bearer["Bearer ".len()..].to_owned();
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = device(dir.path(), "Laptop");

    let trips = server.folder(top, "Trips").await;
    server.file(trips, "notes.txt", "packing list").await;
    laptop.engine(&origin, &token).sync().await.unwrap();

    fs::remove_dir_all(laptop.my_drive().join("Trips")).unwrap();
    server
        .file(trips, "notes.txt", "packing list, with socks")
        .await;
    laptop.engine(&origin, &token).sync().await.unwrap();
    assert!(laptop.my_drive().join("Trips/notes.txt").is_file());

    fs::rename(
        laptop.my_drive().join("Trips"),
        laptop.my_drive().join("Old trips"),
    )
    .unwrap();
    for _ in 0..2 {
        laptop.engine(&origin, &token).sync().await.unwrap();
    }
    let moved = server
        .storage
        .find(top, vec!["Old trips".into()])
        .await
        .unwrap()
        .expect("the folder is in My Drive under its new name");
    assert_eq!(moved.id, trips);
    assert_eq!(
        local_tree(&laptop.my_drive()),
        server_tree(&server.storage, top).await
    );
}

/// Found by seed 7 at 1,000 steps: a folder deleted here, whose inode the
/// system gave to another folder, seemed to have moved into that folder's
/// place. Both claimed it, and every pass moved files back and forth.
#[tokio::test]
async fn a_reused_inode_doesnt_make_two_folders_claim_one_place() {
    let server = start().await;
    let origin = serve(&server).await;
    let token = server.alex.bearer["Bearer ".len()..].to_owned();
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = device(dir.path(), "Laptop");

    server.folder(top, "Old").await;
    let work = server.folder(top, "Work").await;
    server.file(work, "b.txt", "notes").await;
    laptop.engine(&origin, &token).sync().await.unwrap();

    // Old is deleted here, and the system reuses its inode for Work.
    fs::remove_dir_all(laptop.my_drive().join("Old")).unwrap();
    laptop.engine = None;
    let state = State::open(&laptop.db).unwrap();
    let mut old = state.at_path(&format!("{MY_DRIVE}/Old")).unwrap().unwrap();
    old.inode = state
        .at_path(&format!("{MY_DRIVE}/Work"))
        .unwrap()
        .unwrap()
        .inode;
    state.put(&old).unwrap();
    drop(state);

    laptop.engine(&origin, &token).sync().await.unwrap();
    let again = laptop.engine(&origin, &token).sync().await.unwrap();
    assert!(again.done.is_empty(), "{:?}", again.done);
    assert!(
        server
            .storage
            .find(top, vec!["Old".into()])
            .await
            .unwrap()
            .is_none(),
        "Old went to the trash"
    );
    assert_eq!(
        local_tree(&laptop.my_drive()),
        server_tree(&server.storage, top).await
    );
}

/// Found by seed 3 at 1,000 steps: a folder moved here, while Accordion
/// deleted it and made a new one with its old name, lost its record to the
/// new folder. What was inside ended up recorded inside the new one, and
/// came back out of the trash. Since nothing in it changed here, it goes,
/// as it would have without the new folder.
#[tokio::test]
async fn a_folder_moved_here_keeps_its_contents_apart_from_a_new_one_in_its_place() {
    let server = start().await;
    let origin = serve(&server).await;
    let token = server.alex.bearer["Bearer ".len()..].to_owned();
    let top = server.storage.top_folder(server.alex.id).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut desktop = device(dir.path(), "Desktop");

    let old = server.folder(top, "Old").await;
    let work = server.folder(old, "Work").await;
    let inner = server.folder(work, "Work").await;
    server.file(inner, "a.txt", "notes").await;
    desktop.engine(&origin, &token).sync().await.unwrap();

    let reply = server
        .api(&server.alex, "DELETE", &format!("/items/{work}"), None)
        .await;
    assert!(reply.status.is_success(), "{}", reply.text());
    let again = server.folder(old, "Work").await;
    server.file(again, "list.md", "a list").await;
    fs::rename(
        desktop.my_drive().join("Old/Work"),
        desktop.my_drive().join("Trips"),
    )
    .unwrap();

    for _ in 0..3 {
        desktop.engine(&origin, &token).sync().await.unwrap();
    }
    assert!(!desktop.my_drive().join("Trips").exists());
    assert!(
        server
            .storage
            .find(top, vec!["Trips".into()])
            .await
            .unwrap()
            .is_none(),
        "nothing came back out of the trash"
    );
    assert_eq!(
        local_tree(&desktop.my_drive()),
        server_tree(&server.storage, top).await
    );
}
