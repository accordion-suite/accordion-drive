//! The pages a link opens, for people without an account (decision 22 in
//! the `accordion` repo's `docs/decisions`). They're on the link sharing
//! listener, which answers on the internet, and its router has these routes
//! and nothing else.
//!
//! The pages are plain HTML made here, with no script and no sign-in. A
//! link reaches its item and what's inside it, through core's folder walk,
//! and nothing else. Previews are only for kinds of file that can't run
//! code, and everything else only downloads. Expired, used up, removed, and
//! made-up links all get the same page, so nobody learns that a link
//! existed.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use accordion_core::events::{Action, Actor, NewEvent};
use accordion_core::identity::password;
use accordion_core::storage::{Item, ItemKind};
use accordion_core::thumbnails::ThumbnailKind;
use accordion_types::{ItemId, LinkId};
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Form, Path, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, LOCATION,
    RANGE, REFERRER_POLICY, SET_COOKIE, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64ct::{Base64UrlUnpadded, Encoding};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::DriveState;
use crate::api::{self, Preview};
use crate::archive;
use crate::links::Opened;

/// Requests from one address in a minute: enough for a folder of a few
/// hundred photos, whose thumbnails each take one.
const REQUESTS_PER_MINUTE: usize = 600;

/// Wrong passwords from one address, and for one link from anywhere, in
/// `PASSWORD_WINDOW`.
const PASSWORD_TRIES_PER_ADDRESS: usize = 10;
const PASSWORD_TRIES_PER_LINK: usize = 20;
const PASSWORD_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Downloads at once from one address, so one person downloading a big
/// folder doesn't take the household's whole upload for everyone else's
/// links.
const DOWNLOADS_AT_ONCE: usize = 4;

/// How long a right password keeps a link open in that browser.
const UNLOCKED_FOR: Duration = Duration::from_secs(12 * 60 * 60);

const COOKIE_NAME: &str = "accordion_link";

/// Pages: their own style sheet, pictures and media from here, forms that
/// post back here, and nothing else, not even a frame around them.
const PAGE_POLICY: &str = "default-src 'none'; style-src 'self'; img-src 'self'; \
                           media-src 'self'; form-action 'self'; frame-ancestors 'none'; \
                           base-uri 'none'";

const STYLE: &str = include_str!("link_pages/links.css");

/// What the link pages share.
#[derive(Clone)]
pub(crate) struct LinkPages {
    drive: DriveState,
    guard: Arc<Guard>,
}

/// The limits, and which browsers gave a link's password. All of it stays in
/// memory: the addresses of people who open links aren't stored.
#[derive(Default)]
struct Guard {
    requests: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
    wrong_passwords: Mutex<HashMap<Tried, VecDeque<Instant>>>,
    downloads: Mutex<HashMap<IpAddr, usize>>,
    unlocked: Mutex<HashMap<String, (LinkId, Instant)>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Tried {
    From(IpAddr),
    Link(LinkId),
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Keeps the times in `times` that are within `window`, and says whether
/// there's room for one more under `limit`.
fn room(times: &mut VecDeque<Instant>, window: Duration, limit: usize) -> bool {
    let now = Instant::now();
    while times
        .front()
        .is_some_and(|time| now.duration_since(*time) > window)
    {
        times.pop_front();
    }
    times.len() < limit
}

impl Guard {
    fn allow_request(&self, address: IpAddr) -> bool {
        let mut requests = lock(&self.requests);
        // Forget quiet addresses now and then, so the map stays small.
        if requests.len() > 10_000 {
            requests.retain(|_, times| {
                times
                    .back()
                    .is_some_and(|time| time.elapsed() < Duration::from_secs(60))
            });
        }
        let times = requests.entry(address).or_default();
        let allowed = room(times, Duration::from_secs(60), REQUESTS_PER_MINUTE);
        if allowed {
            times.push_back(Instant::now());
        }
        allowed
    }

    fn may_try_password(&self, address: IpAddr, link: LinkId) -> bool {
        let mut tries = lock(&self.wrong_passwords);
        [Tried::From(address), Tried::Link(link)]
            .into_iter()
            .zip([PASSWORD_TRIES_PER_ADDRESS, PASSWORD_TRIES_PER_LINK])
            .all(|(key, limit)| room(tries.entry(key).or_default(), PASSWORD_WINDOW, limit))
    }

    fn wrong_password(&self, address: IpAddr, link: LinkId) {
        let mut tries = lock(&self.wrong_passwords);
        for key in [Tried::From(address), Tried::Link(link)] {
            tries.entry(key).or_default().push_back(Instant::now());
        }
    }

    /// A secret for the browser that gave `link`'s password.
    fn unlock(&self, link: LinkId) -> String {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).expect("the operating system's random number generator works");
        let secret = Base64UrlUnpadded::encode_string(&bytes);
        let mut unlocked = lock(&self.unlocked);
        unlocked.retain(|_, (_, at)| at.elapsed() < UNLOCKED_FOR);
        unlocked.insert(secret.clone(), (link, Instant::now()));
        secret
    }

    fn is_unlocked(&self, headers: &HeaderMap, link: LinkId) -> bool {
        let unlocked = lock(&self.unlocked);
        cookies(headers).any(|secret| {
            unlocked
                .get(secret)
                .is_some_and(|(id, at)| *id == link && at.elapsed() < UNLOCKED_FOR)
        })
    }

    /// Takes one of `address`'s downloads, if it has one free.
    fn start_download(self: &Arc<Self>, address: IpAddr) -> Option<DownloadSlot> {
        let mut downloads = lock(&self.downloads);
        let count = downloads.entry(address).or_default();
        if *count >= DOWNLOADS_AT_ONCE {
            return None;
        }
        *count += 1;
        Some(DownloadSlot {
            guard: Arc::clone(self),
            address,
        })
    }
}

/// One download in progress. It frees its place when the download ends,
/// however it ends.
struct DownloadSlot {
    guard: Arc<Guard>,
    address: IpAddr,
}

impl Drop for DownloadSlot {
    fn drop(&mut self) {
        let mut downloads = lock(&self.guard.downloads);
        if let Some(count) = downloads.get_mut(&self.address) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                downloads.remove(&self.address);
            }
        }
    }
}

/// The values of this page's cookie in a request.
fn cookies(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == COOKIE_NAME).then_some(value)
        })
}

/// The link pages' routes, for the link sharing listener.
pub(crate) fn routes(drive: DriveState) -> Router {
    let state = LinkPages {
        drive,
        guard: Arc::default(),
    };
    Router::new()
        .route("/robots.txt", get(robots))
        .route("/links.css", get(style))
        .route("/s/{token}", get(root_page).post(give_password))
        .route("/s/{token}/i/{item}", get(item_page))
        .route("/s/{token}/download/{item}", get(download))
        .route("/s/{token}/thumbnail/{item}", get(thumbnail))
        .route("/s/{token}/preview/{item}", get(preview))
        .fallback(|| async { not_working() })
        .layer(middleware::from_fn_with_state(
            state.clone(),
            guard_requests,
        ))
        .with_state(state)
}

/// The client's address. The link listener adds it to every request.
fn address(request: &Request) -> IpAddr {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |ConnectInfo(address)| {
            address.ip().to_canonical()
        })
}

/// Limits requests from each address, and adds the headers every response
/// on the internet gets.
async fn guard_requests(State(state): State<LinkPages>, request: Request, next: Next) -> Response {
    let mut response = if state.guard.allow_request(address(&request)) {
        next.run(request).await
    } else {
        too_many()
    };
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex, nofollow"),
    );
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers
        .entry(CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("private, no-store"));
    response
}

async fn robots() -> &'static str {
    "User-agent: *\nDisallow: /\n"
}

async fn style() -> Response {
    (
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, "private, max-age=3600"),
        ],
        STYLE,
    )
        .into_response()
}

/// A link, and the items from its own down to the one asked for.
struct Reached {
    opened: Opened,
    path: Vec<Item>,
}

impl Reached {
    fn item(&self) -> &Item {
        self.path
            .last()
            .expect("a path has at least the link's item")
    }

    fn root(&self) -> &Item {
        &self.path[0]
    }
}

/// Why a link doesn't open.
enum Refused {
    /// It needs its password, which this browser hasn't given.
    Locked,
    NotWorking,
}

impl LinkPages {
    async fn reach(
        &self,
        token: &str,
        item: Option<ItemId>,
        headers: &HeaderMap,
    ) -> Result<Reached, Refused> {
        let Ok(Some(opened)) = self.drive.links.open(token).await else {
            return Err(Refused::NotWorking);
        };
        if opened.password_hash.is_some() && !self.guard.is_unlocked(headers, opened.link.id) {
            return Err(Refused::Locked);
        }
        let wanted = item.unwrap_or(opened.link.item);
        match self
            .drive
            .storage
            .path_within(opened.link.item, wanted)
            .await
        {
            Ok(Some(path)) => Ok(Reached { opened, path }),
            _ => Err(Refused::NotWorking),
        }
    }

    /// Who shared the link's item, by the name people see.
    async fn owner_name(&self, item: &Item) -> String {
        self.drive
            .identity
            .user(item.owner)
            .await
            .ok()
            .flatten()
            .map_or_else(|| "Someone".to_owned(), |user| user.display_name)
    }
}

/// `GET /s/{token}`: the link's own item.
async fn root_page(
    State(state): State<LinkPages>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    match state.reach(&token, None, &headers).await {
        Ok(reached) => {
            count_opening(&state, &reached).await;
            item_response(&state, &token, &reached).await
        }
        Err(Refused::Locked) => password_page(&token, false),
        Err(Refused::NotWorking) => not_working(),
    }
}

/// `GET /s/{token}/i/{item}`: a file or folder inside a linked folder.
async fn item_page(
    State(state): State<LinkPages>,
    Path((token, item)): Path<(String, ItemId)>,
    headers: HeaderMap,
) -> Response {
    match state.reach(&token, Some(item), &headers).await {
        Ok(reached) => item_response(&state, &token, &reached).await,
        Err(Refused::Locked) => password_page(&token, false),
        Err(Refused::NotWorking) => not_working(),
    }
}

/// Counts an opening of the link, and tells its maker, at most once a day.
async fn count_opening(state: &LinkPages, reached: &Reached) {
    let link = &reached.opened.link;
    match state.drive.links.opened(link.id).await {
        Ok(true) => {
            let root = reached.root();
            let event = NewEvent::about(Action::LinkOpened, root.id, root.owner)
                .other(link.made_by)
                .details(serde_json::json!({ "link": link.id }));
            if let Err(err) = state.drive.events.record(Actor::SERVER, event).await {
                tracing::warn!(%err, "couldn't record that a link was opened");
            }
        }
        Ok(false) => {}
        Err(err) => tracing::warn!(%err, "couldn't count a link's opening"),
    }
}

#[derive(Deserialize)]
struct PasswordForm {
    password: String,
}

/// `POST /s/{token}`: the link's password. A right one keeps the link open
/// in this browser for a while.
async fn give_password(
    State(state): State<LinkPages>,
    Path(token): Path<String>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    Form(form): Form<PasswordForm>,
) -> Response {
    let Ok(Some(opened)) = state.drive.links.open(&token).await else {
        return not_working();
    };
    let Some(hash) = opened.password_hash.clone() else {
        return see_other(&format!("/s/{token}"));
    };
    let address = from.ip().to_canonical();
    if !state.guard.may_try_password(address, opened.link.id) {
        return too_many();
    }
    if !password::verify(form.password, Some(hash)).await {
        state.guard.wrong_password(address, opened.link.id);
        return password_page(&token, true);
    }
    let secret = state.guard.unlock(opened.link.id);
    let mut response = see_other(&format!("/s/{token}"));
    let cookie = format!(
        "{COOKIE_NAME}={secret}; Path=/s/{token}; Max-Age={}; HttpOnly; Secure; SameSite=Lax",
        UNLOCKED_FOR.as_secs()
    );
    if let Ok(cookie) = HeaderValue::try_from(cookie) {
        response.headers_mut().append(SET_COOKIE, cookie);
    }
    response
}

/// `GET /s/{token}/download/{item}`: a file, or a folder as one ZIP file.
/// Each one counts against the link's download limit.
async fn download(
    State(state): State<LinkPages>,
    Path((token, item)): Path<(String, ItemId)>,
    ConnectInfo(from): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let Ok(reached) = state.reach(&token, Some(item), &headers).await else {
        return not_working();
    };
    let Some(slot) = state.guard.start_download(from.ip().to_canonical()) else {
        return too_many();
    };
    // A download that picks up where it stopped is the same download.
    let resuming = headers
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|range| !range.trim().starts_with("bytes=0-"));
    if !resuming
        && !matches!(
            state.drive.links.downloaded(reached.opened.link.id).await,
            Ok(true)
        )
    {
        return not_working();
    }
    let item = reached.item().clone();
    let response = match item.kind {
        ItemKind::File { blob, .. } => {
            api::file_response(&state.drive, &headers, &item.name, blob).await
        }
        ItemKind::Folder => zip(&state, item).await,
    };
    match response {
        Ok(response) => holding(response, slot),
        Err(_) => not_working(),
    }
}

/// A folder and everything in it, as one ZIP file built as it's sent.
async fn zip(state: &LinkPages, folder: Item) -> Result<Response, accordion_module::ApiError> {
    let storage = &state.drive.storage;
    let name = format!("{}.zip", folder.name);
    let hold = storage.hold_contents().await;
    let entries = archive::entries(storage, vec![folder]).await?;
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    tokio::task::spawn_blocking(move || archive::write(entries, hold, sender));
    let body = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|chunk| (chunk, receiver))
    });
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    headers.insert(CONTENT_DISPOSITION, api::disposition("attachment", &name));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    Ok((headers, Body::from_stream(body)).into_response())
}

/// Keeps `slot` until `response`'s body is sent, or the download stops.
fn holding(response: Response, slot: DownloadSlot) -> Response {
    let (parts, body) = response.into_parts();
    let body = body.into_data_stream().map(move |chunk| {
        let _ = &slot;
        chunk
    });
    Response::from_parts(parts, Body::from_stream(body))
}

/// `GET /s/{token}/thumbnail/{item}`: a file's small picture, for the
/// folder page.
async fn thumbnail(
    State(state): State<LinkPages>,
    Path((token, item)): Path<(String, ItemId)>,
    headers: HeaderMap,
) -> Response {
    let Ok(reached) = state.reach(&token, Some(item), &headers).await else {
        return not_working();
    };
    let ItemKind::File { blob, .. } = &reached.item().kind else {
        return not_working();
    };
    let Some(found) = state.drive.thumbnails.find(blob.hash) else {
        return not_working();
    };
    if found.kind != ThumbnailKind::Picture {
        return not_working();
    }
    let Ok(picture) = tokio::fs::read(found.path).await else {
        return not_working();
    };
    (
        [
            (CONTENT_TYPE, found.kind.content_type()),
            (CONTENT_SECURITY_POLICY, "sandbox"),
            (CACHE_CONTROL, "private, max-age=3600"),
        ],
        picture,
    )
        .into_response()
}

/// `GET /s/{token}/preview/{item}`: a picture, video, sound, or PDF as the
/// browser shows it, or an Office document's PDF preview. Nothing that can
/// run code is ever shown.
async fn preview(
    State(state): State<LinkPages>,
    Path((token, item)): Path<(String, ItemId)>,
    headers: HeaderMap,
) -> Response {
    let Ok(reached) = state.reach(&token, Some(item), &headers).await else {
        return not_working();
    };
    match api::preview_response(&state.drive, &headers, reached.item()).await {
        Ok(response) => response,
        Err(_) => not_working(),
    }
}

/// The page for a file or folder that a link reached.
async fn item_response(state: &LinkPages, token: &str, reached: &Reached) -> Response {
    let item = reached.item();
    let owner = state.owner_name(reached.root()).await;
    let mut body = String::new();
    let _ = write!(
        body,
        "<p class=\"from\">{} shared this with you.</p>",
        escape(&owner)
    );
    if reached.path.len() > 1 {
        body.push_str("<nav class=\"path\" aria-label=\"Folders\">");
        for (depth, folder) in reached.path[..reached.path.len() - 1].iter().enumerate() {
            let href = if depth == 0 {
                format!("/s/{token}")
            } else {
                format!("/s/{token}/i/{}", folder.id)
            };
            let _ = write!(body, "<a href=\"{href}\">{}</a> / ", escape(&folder.name));
        }
        body.push_str("</nav>");
    }
    let _ = write!(body, "<h1>{}</h1>", escape(&item.name));
    match &item.kind {
        ItemKind::File { blob, .. } => {
            let _ = write!(
                body,
                "<p class=\"facts\">{}. Changed {}.</p>",
                size(blob.size),
                day(&item.modified_at)
            );
            body.push_str(&preview_html(state, token, item, blob.hash));
            let _ = write!(
                body,
                "<p><a class=\"button\" href=\"/s/{token}/download/{}\">Download</a></p>",
                item.id
            );
        }
        ItemKind::Folder => {
            let children = state
                .drive
                .storage
                .children(item.id)
                .await
                .unwrap_or_default();
            let _ = write!(
                body,
                "<p><a class=\"button\" href=\"/s/{token}/download/{}\">Download all as a ZIP \
                 file</a></p>",
                item.id
            );
            if children.is_empty() {
                body.push_str("<p class=\"facts\">This folder is empty.</p>");
            } else {
                body.push_str("<ul class=\"items\">");
                for child in &children {
                    body.push_str(&child_html(state, token, child));
                }
                body.push_str("</ul>");
            }
        }
    }
    page(&item.name, &body, StatusCode::OK)
}

/// A file's preview on its page, if it has one that can't run code.
fn preview_html(
    state: &LinkPages,
    token: &str,
    item: &Item,
    hash: accordion_core::storage::ContentHash,
) -> String {
    let source = format!("/s/{token}/preview/{}", item.id);
    let name = escape(&item.name);
    // A song's cover art, if it has some, then its sound.
    let sound = || {
        format!(
            "{}<audio controls preload=\"metadata\" src=\"{source}\"></audio>",
            picture_html(state, token, item, hash, "preview")
        )
    };
    let video = || {
        format!("<video class=\"preview\" controls preload=\"metadata\" src=\"{source}\"></video>")
    };
    // Sound or video core converted, for a file the browser can't play.
    if state.drive.thumbnails.sound(hash).is_some() {
        return sound();
    }
    if state.drive.thumbnails.video(hash).is_some() {
        return video();
    }
    match api::preview(&item.name) {
        Some(Preview::Image(_)) => {
            format!("<img class=\"preview\" src=\"{source}\" alt=\"{name}\">")
        }
        Some(Preview::Video(_)) => video(),
        Some(Preview::Audio(_)) => sound(),
        Some(Preview::Pdf) => format!(
            "{}<p><a href=\"{source}\">Open the PDF</a></p>",
            picture_html(state, token, item, hash, "preview")
        ),
        None if state.drive.thumbnails.preview(hash).is_some() => format!(
            "{}<p><a href=\"{source}\">Open a PDF of it</a></p>",
            picture_html(state, token, item, hash, "preview")
        ),
        None => {
            let picture = picture_html(state, token, item, hash, "preview");
            if picture.is_empty() {
                icon_html(item, "large")
            } else {
                picture
            }
        }
    }
}

/// A file's thumbnail, if it has a picture one.
fn picture_html(
    state: &LinkPages,
    token: &str,
    item: &Item,
    hash: accordion_core::storage::ContentHash,
    class: &str,
) -> String {
    match state.drive.thumbnails.find(hash) {
        // The hash and the thumbnail's version name this picture, so a
        // browser fetches a new one when either changes.
        Some(found) if found.kind == ThumbnailKind::Picture => format!(
            "<img class=\"{class}\" src=\"/s/{token}/thumbnail/{}?v={hash}-{}\" alt=\"\" loading=\"lazy\">",
            item.id,
            found.version()
        ),
        _ => String::new(),
    }
}

/// One file or folder in a folder's list.
fn child_html(state: &LinkPages, token: &str, child: &Item) -> String {
    let (picture, detail) = match &child.kind {
        ItemKind::File { blob, .. } => (
            picture_html(state, token, child, blob.hash, "thumbnail"),
            size(blob.size),
        ),
        ItemKind::Folder => (String::new(), "Folder".to_owned()),
    };
    let picture = if picture.is_empty() {
        icon_html(child, "small")
    } else {
        picture
    };
    format!(
        "<li><a href=\"/s/{token}/i/{id}\">{picture}<span class=\"name\">{name}</span>\
         <span class=\"detail\">{detail}</span></a></li>",
        id = child.id,
        name = escape(&child.name),
    )
}

/// The web app's file icons (its `src/drive/icons.tsx`), by the kinds of
/// file in [`api::TYPES`], so a shared folder looks the way it does in Drive.
const ICONS: &[(&str, &str)] = &[
    (
        "folder",
        "M2 6a2 2 0 0 1 2-2h5.2l2 2H20a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z",
    ),
    (
        "documents",
        "M6 2h8l6 6v12a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2m2 8.5v1.6h8v-1.6zm0 3.4v1.6h8v-1.6zm0 3.4v1.6h5v-1.6z",
    ),
    (
        "spreadsheets",
        "M6 2h8l6 6v12a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2m1.5 8v8h9v-8zm1.5 1.5h2.2v1.8H9zm3.7 0H15v1.8h-2.3zM9 14.7h2.2v1.8H9zm3.7 0H15v1.8h-2.3z",
    ),
    (
        "presentations",
        "M6 2h8l6 6v12a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2m1.5 8.5v6.5h9v-6.5zm1.5 1.5h6v3.5H9z",
    ),
    (
        "pdfs",
        "M6 2h8l6 6v12a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2m0 9.5v5h1.4v-1.6h.8a1.7 1.7 0 0 0 0-3.4zm1.4 1.2h.8a.5.5 0 0 1 0 1h-.8zM10.3 11.5v5h1.4a2.5 2.5 0 0 0 0-5zm1.4 1.2h.1a1.3 1.3 0 0 1 0 2.6h-.1zM15 11.5v5h1.4v-2h1.4v-1.1h-1.4v-.7H18v-1.2z",
    ),
    (
        "photos",
        "M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2m3.5 4a1.8 1.8 0 1 0 0 3.6 1.8 1.8 0 0 0 0-3.6M6 18h12l-3.8-5.2-3 3.9-2-2.4z",
    ),
    (
        "videos",
        "M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2m5 5.5v7l5.5-3.5z",
    ),
    (
        "audio",
        "M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2m5.5 4v6.3A2.5 2.5 0 1 0 12 15.6V9.5h3V7z",
    ),
    (
        "archives",
        "M6 2h12a2 2 0 0 1 2 2v16a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2m5 1v2h2V3zm0 2v2h-2v2h2v2h-2v2h2v2.5a1.5 1.5 0 1 0 2 0V13h-2v-2h2V9h-2V7h2V5z",
    ),
    (
        "code",
        "M6 2h8l6 6v12a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2m4.3 8.9L7 14.2l3.3 3.3 1-1-2.3-2.3 2.3-2.3zm3.4 0-1 1 2.3 2.3-2.3 2.3 1 1 3.3-3.3z",
    ),
    (
        "fonts",
        "M6 2h8l6 6v12a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2m6 7.5L8.2 18h1.7l.8-1.9h2.6l.8 1.9h1.7zm-.9 5.1.9-2.2.9 2.2z",
    ),
    (
        "ebooks",
        "M6 2h12a2 2 0 0 1 2 2v16a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2m1.5 0v20H9V2zm3.5 5v1.6h6V7zm0 3v1.6h4V10z",
    ),
    (
        "designs",
        "M12 3a9 9 0 0 0 0 18c1 0 1.6-.7 1.6-1.6 0-.5-.2-.8-.5-1.1-.3-.3-.5-.7-.5-1.1 0-.9.7-1.6 1.6-1.6H16a5 5 0 0 0 5-5c0-4-4-7.6-9-7.6M7.5 10.1a1.4 1.4 0 1 0 0 2.8 1.4 1.4 0 0 0 0-2.8m2.5-4a1.4 1.4 0 1 0 0 2.8 1.4 1.4 0 0 0 0-2.8m4.5 0a1.4 1.4 0 1 0 0 2.8 1.4 1.4 0 0 0 0-2.8m2.6 3.6a1.4 1.4 0 1 0 0 2.8 1.4 1.4 0 0 0 0-2.8",
    ),
    (
        "file",
        "M6 2h8l6 6v12a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2m7 1.5V9h5.5z",
    ),
];

/// An item's icon, for when it has no picture. links.css colors it by kind.
fn icon_html(item: &Item, size: &str) -> String {
    let kind = match item.kind {
        ItemKind::Folder => "folder",
        ItemKind::File { .. } => api::file_type(&item.name).unwrap_or("file"),
    };
    let shape = ICONS
        .iter()
        .find(|(name, _)| *name == kind)
        .map_or("", |(_, shape)| shape);
    format!(
        "<svg class=\"icon {size} kind-{kind}\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" \
         focusable=\"false\" fill=\"currentColor\"><path fill-rule=\"evenodd\" d=\"{shape}\"/></svg>"
    )
}

/// The page asking for a link's password.
fn password_page(token: &str, wrong: bool) -> Response {
    let mut body = String::from(
        "<h1>This link needs a password</h1>\
         <p>Whoever sent you the link can tell you what it is.</p>",
    );
    let _ = write!(
        body,
        "<form method=\"post\" action=\"/s/{token}\">\
         <label for=\"password\">Password</label>\
         <input id=\"password\" name=\"password\" type=\"password\" autocomplete=\"off\" \
         autofocus required>"
    );
    if wrong {
        body.push_str(
            "<p class=\"error\" role=\"alert\">That password isn't right. Check it, then try \
             again.</p>",
        );
    }
    body.push_str("<button type=\"submit\">Open</button></form>");
    page("Password needed", &body, StatusCode::OK)
}

/// The one page for every link that doesn't open.
fn not_working() -> Response {
    page(
        "This link doesn't work",
        "<h1>This link doesn't work</h1>\
         <p>It may have expired, or been removed, or it may be mistyped. Ask whoever sent it to \
         you for a new one.</p>",
        StatusCode::NOT_FOUND,
    )
}

fn too_many() -> Response {
    let mut response = page(
        "Too many tries",
        "<h1>Too many tries</h1><p>Wait a few minutes, then try again.</p>",
        StatusCode::TOO_MANY_REQUESTS,
    );
    response
        .headers_mut()
        .insert("retry-after", HeaderValue::from_static("300"));
    response
}

fn see_other(location: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(location) = HeaderValue::try_from(location) {
        response.headers_mut().insert(LOCATION, location);
    }
    response
}

fn page(title: &str, body: &str, status: StatusCode) -> Response {
    let html = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex, nofollow\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <title>{}</title><link rel=\"stylesheet\" href=\"/links.css\"></head>\
         <body><main>{body}</main>\
         <footer>Shared from a home server running Accordion.</footer></body></html>\n",
        escape(title)
    );
    (
        status,
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CONTENT_SECURITY_POLICY, PAGE_POLICY),
        ],
        html,
    )
        .into_response()
}

/// Text made safe to put in HTML, in an element or an attribute.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// A size such as "2.4 MB".
fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return if bytes == 1 {
            "1 byte".to_owned()
        } else {
            format!("{bytes} bytes")
        };
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// A time in the database's format, such as `2026-11-04T09:30:00.000Z`, as
/// "November 4, 2026".
fn day(time: &str) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let date = time.get(..10).unwrap_or(time);
    let mut parts = date.split('-').map(str::parse::<usize>);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(year)), Some(Ok(month @ 1..=12)), Some(Ok(day))) => {
            format!("{} {day}, {year}", MONTHS[month - 1])
        }
        _ => date.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_names() {
        assert_eq!(
            escape("<script>alert(\"hi\")</script> & 'you'"),
            "&lt;script&gt;alert(&quot;hi&quot;)&lt;/script&gt; &amp; &#39;you&#39;"
        );
    }

    #[test]
    fn writes_sizes_and_days() {
        assert_eq!(size(1), "1 byte");
        assert_eq!(size(999), "999 bytes");
        assert_eq!(size(2_400_000), "2.4 MB");
        assert_eq!(size(51_000_000_000), "51 GB");
        assert_eq!(day("2026-11-04T09:30:00.000Z"), "November 4, 2026");
    }

    #[test]
    fn has_an_icon_for_every_kind_of_file() {
        for (kind, _) in api::TYPES {
            assert!(
                ICONS.iter().any(|(name, _)| name == kind),
                "no icon for {kind}"
            );
        }
        assert_eq!(api::file_type("Budget.ODS"), Some("spreadsheets"));
        assert_eq!(api::file_type("server.log"), None);
        assert_eq!(api::file_type(".zip"), None);
        assert_eq!(api::file_type("Wedding.VOB"), Some("videos"));
        assert_eq!(api::file_type("Ringtone.m4r"), Some("audio"));
    }

    #[test]
    fn limits_password_tries() {
        let guard = Guard::default();
        let link = LinkId::generate();
        let address: IpAddr = "203.0.113.9".parse().unwrap();
        for _ in 0..PASSWORD_TRIES_PER_ADDRESS {
            assert!(guard.may_try_password(address, link));
            guard.wrong_password(address, link);
        }
        assert!(!guard.may_try_password(address, link));
        // Another address can still try, until the link's own limit.
        let other: IpAddr = "198.51.100.4".parse().unwrap();
        assert!(guard.may_try_password(other, link));
    }

    #[test]
    fn limits_downloads_at_once() {
        let guard = Arc::new(Guard::default());
        let address: IpAddr = "203.0.113.9".parse().unwrap();
        let slots: Vec<_> = (0..DOWNLOADS_AT_ONCE)
            .map(|_| guard.start_download(address).unwrap())
            .collect();
        assert!(guard.start_download(address).is_none());
        drop(slots);
        assert!(guard.start_download(address).is_some());
    }
}
