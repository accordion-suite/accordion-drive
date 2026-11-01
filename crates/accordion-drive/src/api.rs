//! `/api/drive`: the JSON API that Accordion's web, desktop, and mobile apps
//! use.
//!
//! Items are files and folders, named by ID. Each request checks that the
//! signed-in user owns what it names, and answers "not found" otherwise.
//! Lists that reach across folders, such as search results, say where each
//! item is, as the names of the folders it's in.

use std::io;
use std::ops::Range;

use accordion_core::storage::{
    self, Blob, Found, Item, ItemKind, Only, Search, Upload, Usage, Version,
};
use accordion_core::thumbnails::{ThumbnailKind, Thumbnails};
use accordion_module::{ApiError, Authenticated};
use accordion_types::{ItemId, UploadId, UserId, VersionId};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_RANGE, RANGE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio_util::io::{ReaderStream, StreamReader};

use crate::settings::DriveSettings;
use crate::{DriveState, archive, time};

/// Says where a piece of a resumable upload starts. The name follows the
/// tus protocol, which uses the same header for the same purpose.
const UPLOAD_OFFSET: &str = "upload-offset";

pub(crate) fn routes(state: DriveState) -> Router {
    Router::new()
        .route("/top", get(top))
        .route("/items/{id}", get(item).patch(move_item).delete(trash_item))
        .route("/items/{id}/children", get(children).post(create_folder))
        .route("/items/{id}/children/{name}", put(write_file))
        .route("/items/{id}/content", get(download))
        .route("/items/{id}/preview", get(preview_file))
        .route("/items/{id}/thumbnail", get(thumbnail))
        .route("/items/{id}/restore", post(restore))
        .route("/items/{id}/star", put(star).delete(unstar))
        .route("/items/{id}/copy", post(copy))
        .route("/items/{id}/versions", get(versions))
        .route(
            "/items/{id}/versions/{version}/content",
            get(download_version),
        )
        .route(
            "/items/{id}/versions/{version}/restore",
            post(restore_version),
        )
        .route("/trash", get(trash).delete(empty_trash))
        .route("/trash/{id}", delete(delete_forever))
        .route("/usage", get(usage))
        .route("/recent", get(recent))
        .route("/starred", get(starred))
        .route("/search", get(search))
        .route("/zip", get(zip_download))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/uploads", post(start_upload))
        .route(
            "/uploads/{id}",
            get(upload).patch(append).delete(cancel_upload),
        )
        .route("/uploads/{id}/finish", post(finish_upload))
        .with_state(state)
}

#[derive(Serialize)]
struct ItemView {
    id: ItemId,
    /// `None` for the user's top folder.
    parent: Option<ItemId>,
    name: String,
    kind: &'static str,
    /// For files: the current version, its size, and its SHA-256 hash.
    version: Option<VersionId>,
    size: Option<u64>,
    sha256: Option<String>,
    /// For files the browser can show itself: "image" or "pdf". The web
    /// app opens these from `/items/{id}/preview`.
    preview: Option<&'static str>,
    /// What `/items/{id}/thumbnail` has for the file yet: "picture", a small
    /// WebP, or "text", its first lines. Core makes them in the background,
    /// so a new photo gets one soon.
    thumbnail: Option<&'static str>,
    created_at: String,
    modified_at: String,
    /// When it was deleted, if it's in the trash itself.
    trashed_at: Option<String>,
    /// Whether the signed-in user starred it.
    starred: bool,
}

impl ItemView {
    fn new(item: Item, starred: bool, thumbnails: &Thumbnails) -> Self {
        let (kind, version, blob) = match item.kind {
            ItemKind::Folder => ("folder", None, None),
            ItemKind::File { version, blob } => ("file", Some(version), Some(blob)),
        };
        let preview = blob.and(preview(&item.name)).map(|preview| match preview {
            Preview::Image(_) => "image",
            Preview::Pdf => "pdf",
            Preview::Video(_) => "video",
            Preview::Audio(_) => "audio",
        });
        let thumbnail = blob
            .and_then(|blob| thumbnails.find(blob.hash))
            .and_then(|thumbnail| match thumbnail.kind {
                ThumbnailKind::Picture => Some("picture"),
                // A song or a video that happens to be text, such as a
                // placeholder, would look broken as a page of text.
                ThumbnailKind::Text => text_kind(&item.name).then_some("text"),
            });
        Self {
            id: item.id,
            parent: item.parent,
            name: item.name,
            kind,
            version,
            size: blob.map(|blob| blob.size),
            sha256: blob.map(|blob| blob.hash.to_hex()),
            preview,
            thumbnail,
            created_at: item.created_at,
            modified_at: item.modified_at,
            trashed_at: item.trashed_at,
            starred,
        }
    }
}

/// An item in a list that reaches across folders.
#[derive(Serialize)]
struct FoundView {
    #[serde(flatten)]
    item: ItemView,
    /// The names of the folders it's in, outermost first, not counting the
    /// top folder.
    folders: Vec<String>,
}

impl DriveState {
    async fn view(&self, user: UserId, item: Item) -> Result<ItemView, ApiError> {
        let starred = self.stars.set(user).await?.contains(&item.id);
        Ok(ItemView::new(item, starred, &self.thumbnails))
    }

    async fn views(&self, user: UserId, items: Vec<Item>) -> Result<Vec<ItemView>, ApiError> {
        let stars = self.stars.set(user).await?;
        Ok(items
            .into_iter()
            .map(|item| {
                let starred = stars.contains(&item.id);
                ItemView::new(item, starred, &self.thumbnails)
            })
            .collect())
    }

    async fn found_views(
        &self,
        user: UserId,
        found: Vec<Found>,
    ) -> Result<Json<Vec<FoundView>>, ApiError> {
        let stars = self.stars.set(user).await?;
        Ok(Json(
            found
                .into_iter()
                .map(|found| {
                    let starred = stars.contains(&found.item.id);
                    FoundView {
                        item: ItemView::new(found.item, starred, &self.thumbnails),
                        folders: found.folders,
                    }
                })
                .collect(),
        ))
    }
}

#[derive(Serialize)]
struct VersionView {
    id: VersionId,
    number: u32,
    size: u64,
    sha256: String,
    created_at: String,
}

impl From<Version> for VersionView {
    fn from(version: Version) -> Self {
        Self {
            id: version.id,
            number: version.number,
            size: version.blob.size,
            sha256: version.blob.hash.to_hex(),
            created_at: version.created_at,
        }
    }
}

#[derive(Serialize)]
struct UploadView {
    id: UploadId,
    size: u64,
    /// How much has arrived. The next piece starts here.
    received: u64,
    created_at: String,
}

impl From<Upload> for UploadView {
    fn from(upload: Upload) -> Self {
        Self {
            id: upload.id,
            size: upload.size,
            received: upload.received,
            created_at: upload.created_at,
        }
    }
}

#[derive(Serialize)]
struct UsageView {
    used: u64,
    uploading: u64,
    /// `None` when there's no limit.
    quota: Option<u64>,
}

impl From<Usage> for UsageView {
    fn from(usage: Usage) -> Self {
        Self {
            used: usage.used,
            uploading: usage.uploading,
            quota: usage.quota,
        }
    }
}

type ItemResult = Result<Json<ItemView>, ApiError>;

/// `GET /top`: the user's top folder.
async fn top(State(state): State<DriveState>, Authenticated(me): Authenticated) -> ItemResult {
    let id = state.storage.top_folder(me.user.id).await?;
    let item = state.owned(me.user.id, id).await?;
    Ok(Json(state.view(me.user.id, item).await?))
}

/// `GET /items/{id}`
async fn item(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> ItemResult {
    let item = state.owned(me.user.id, id).await?;
    Ok(Json(state.view(me.user.id, item).await?))
}

/// `GET /items/{id}/children`: what's in a folder, sorted by name.
async fn children(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<Vec<ItemView>>, ApiError> {
    state.owned(me.user.id, id).await?;
    let children = state.storage.children(id).await?;
    Ok(Json(state.views(me.user.id, children).await?))
}

#[derive(Deserialize)]
struct NewFolder {
    name: String,
}

/// `POST /items/{id}/children`: creates a folder.
async fn create_folder(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(parent): Path<ItemId>,
    Json(request): Json<NewFolder>,
) -> Result<(StatusCode, Json<ItemView>), ApiError> {
    state.owned(me.user.id, parent).await?;
    let folder = state.storage.create_folder(parent, &request.name).await?;
    Ok((
        StatusCode::CREATED,
        Json(state.view(me.user.id, folder).await?),
    ))
}

/// `PUT /items/{id}/children/{name}`: saves the request body as a file, or
/// as a new version if a file with that name is already there. For large
/// files, use a resumable upload instead.
async fn write_file(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path((parent, name)): Path<(ItemId, String)>,
    body: Body,
) -> ItemResult {
    state.owned(me.user.id, parent).await?;
    let file = state
        .storage
        .write_file(parent, &name, reader(body))
        .await?;
    Ok(Json(state.view(me.user.id, file).await?))
}

#[derive(Deserialize)]
struct MoveRequest {
    /// The folder to move into. Leave it out to stay in the same folder.
    parent: Option<ItemId>,
    /// The new name. Leave it out to keep the name.
    name: Option<String>,
}

/// `PATCH /items/{id}`: renames an item, moves it, or both.
async fn move_item(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Json(request): Json<MoveRequest>,
) -> ItemResult {
    let item = state.owned(me.user.id, id).await?;
    let parent = match request.parent {
        Some(parent) => state.owned(me.user.id, parent).await?.id,
        None => item.parent.ok_or(storage::Error::TopFolder)?,
    };
    let name = request.name.unwrap_or(item.name);
    let moved = state.storage.move_item(id, parent, &name).await?;
    Ok(Json(state.view(me.user.id, moved).await?))
}

/// `DELETE /items/{id}`: moves an item to the trash.
async fn trash_item(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> ItemResult {
    state.owned(me.user.id, id).await?;
    let deleted = state.storage.delete(id).await?;
    Ok(Json(state.view(me.user.id, deleted).await?))
}

/// `POST /items/{id}/restore`: takes an item out of the trash.
async fn restore(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> ItemResult {
    state.owned(me.user.id, id).await?;
    let restored = state.storage.restore(id).await?;
    Ok(Json(state.view(me.user.id, restored).await?))
}

/// `GET /settings`: the signed-in person's Drive settings.
async fn get_settings(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<DriveSettings>, ApiError> {
    Ok(Json(state.settings.get(me.user.id).await?))
}

/// `PUT /settings`: replaces them, so send every setting.
async fn put_settings(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Json(settings): Json<DriveSettings>,
) -> Result<StatusCode, ApiError> {
    state.settings.set(me.user.id, settings).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /items/{id}/star`: stars an item.
async fn star(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<StatusCode, ApiError> {
    state.owned(me.user.id, id).await?;
    state.stars.star(me.user.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /items/{id}/star`: takes the star off an item.
async fn unstar(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<StatusCode, ApiError> {
    state.owned(me.user.id, id).await?;
    state.stars.unstar(me.user.id, vec![id]).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CopyRequest {
    /// The folder to copy into. Leave it out for the same folder.
    parent: Option<ItemId>,
    /// The copy's name, such as "Copy of Beach.jpg". It can't be taken.
    name: String,
}

/// `POST /items/{id}/copy`: copies a file. Folders can't be copied.
async fn copy(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Json(request): Json<CopyRequest>,
) -> Result<(StatusCode, Json<ItemView>), ApiError> {
    let item = state.owned(me.user.id, id).await?;
    let parent = match request.parent {
        Some(parent) => state.owned(me.user.id, parent).await?.id,
        None => item.parent.ok_or(storage::Error::NotAFile)?,
    };
    let copy = state.storage.copy_file(id, parent, &request.name).await?;
    Ok((
        StatusCode::CREATED,
        Json(state.view(me.user.id, copy).await?),
    ))
}

/// How many files **Recent** shows.
const RECENT: usize = 100;

/// The most results a search returns. Searching for more words narrows it.
const SEARCH_LIMIT: usize = 200;

/// `GET /recent`: the user's files outside the trash, most recently
/// changed first.
async fn recent(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<Vec<FoundView>>, ApiError> {
    let found = state.storage.recent(me.user.id, RECENT).await?;
    state.found_views(me.user.id, found).await
}

/// `GET /starred`: what the user starred and isn't in the trash, most
/// recently starred first.
async fn starred(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<Vec<FoundView>>, ApiError> {
    let ids = state.stars.starred(me.user.id).await?;
    let found = state.storage.live_items(ids.clone()).await?;
    // Stars on items that were deleted for good are no use to anyone. Ones
    // in the trash stay, for if they're restored.
    let mut gone = Vec::new();
    for id in ids {
        if !found.iter().any(|found| found.item.id == id) && state.storage.item(id).await?.is_none()
        {
            gone.push(id);
        }
    }
    if !gone.is_empty() {
        state.stars.unstar(me.user.id, gone).await?;
    }
    let found = found
        .into_iter()
        .filter(|found| found.item.owner == me.user.id)
        .collect();
    state.found_views(me.user.id, found).await
}

#[derive(Deserialize)]
struct SearchQuery {
    /// Part of the name, ignoring case.
    #[serde(default)]
    q: String,
    /// A kind of item: one of the names in [`TYPES`], or "folders".
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Changed at or after this time, such as `2026-10-30T00:00:00.000Z`.
    /// The web app works out times like the start of today in the reader's
    /// own time zone, which the server doesn't know.
    after: Option<String>,
    /// Changed before this time.
    before: Option<String>,
}

/// The kinds of file a search can look for, as Google Drive's type filter
/// offers, by the extensions their names end in.
const TYPES: &[(&str, &[&str])] = &[
    (
        "documents",
        &["doc", "docx", "odt", "rtf", "txt", "md", "pages"],
    ),
    (
        "spreadsheets",
        &["xls", "xlsx", "ods", "csv", "tsv", "numbers"],
    ),
    ("presentations", &["ppt", "pptx", "odp", "key"]),
    ("pdfs", &["pdf"]),
    (
        "photos",
        &[
            "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "bmp", "tif", "tiff",
            "svg", "dng", "raw",
        ],
    ),
    (
        "videos",
        &[
            "mp4", "mov", "m4v", "mkv", "webm", "avi", "wmv", "3gp", "mts",
        ],
    ),
    (
        "audio",
        &[
            "mp3", "m4a", "aac", "wav", "flac", "ogg", "opus", "wma", "aiff",
        ],
    ),
    (
        "archives",
        &["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz"],
    ),
];

/// `GET /search?q=&type=&after=&before=`: the user's items outside the
/// trash that match, most recently changed first.
async fn search(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<FoundView>>, ApiError> {
    let only =
        match query.kind.as_deref() {
            None | Some("") => None,
            Some("folders") => Some(Only::Folders),
            Some("files") => Some(Only::Files),
            Some(kind) => {
                let (_, extensions) = TYPES.iter().find(|(name, _)| *name == kind).ok_or_else(
                    || {
                        ApiError::new(
                            StatusCode::BAD_REQUEST,
                            format!(
                                "Accordion doesn't know the type \"{kind}\". Choose another type."
                            ),
                        )
                    },
                )?;
                Some(Only::Extensions(
                    extensions
                        .iter()
                        .map(|&extension| extension.to_owned())
                        .collect(),
                ))
            }
        };
    let search = Search {
        text: query.q.trim().to_owned(),
        only,
        modified_since: checked_time(query.after)?,
        modified_before: checked_time(query.before)?,
        limit: SEARCH_LIMIT,
    };
    let found = state.storage.search(me.user.id, search).await?;
    state.found_views(me.user.id, found).await
}

/// Storage compares times as text, so they have to be in its exact format,
/// with milliseconds, as JavaScript's `toISOString` writes them.
fn checked_time(time: Option<String>) -> Result<Option<String>, ApiError> {
    match time {
        Some(text) if text.len() != 24 || time::parse(&text).is_none() => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Search times need to look like 2026-10-30T00:00:00.000Z, in UTC.",
        )),
        time => Ok(time),
    }
}

/// `GET /items/{id}/content`: downloads the current version of a file.
async fn download(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    request: HeaderMap,
) -> Result<Response, ApiError> {
    let item = state.owned(me.user.id, id).await?;
    let ItemKind::File { blob, .. } = item.kind else {
        return Err(storage::Error::NotAFile.into());
    };
    file_response(&state, &request, &item.name, blob).await
}

/// `GET /items/{id}/preview`: the file as a type the browser shows itself,
/// for the kinds of file that `preview` allows.
async fn preview_file(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    request: HeaderMap,
) -> Result<Response, ApiError> {
    let item = state.owned(me.user.id, id).await?;
    let ItemKind::File { blob, .. } = item.kind else {
        return Err(storage::Error::NotAFile.into());
    };
    let (content_type, policy) = match preview(&item.name) {
        Some(
            Preview::Image(content_type)
            | Preview::Video(content_type)
            | Preview::Audio(content_type),
        ) => (content_type, "sandbox; frame-ancestors 'self'"),
        // Browsers show PDFs in their own viewers, which keep a PDF's
        // scripts away from the page around it. Some of those viewers
        // won't run in a sandbox, so PDFs go without one.
        Some(Preview::Pdf) => ("application/pdf", "frame-ancestors 'self'"),
        None => {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Accordion can't show this kind of file. Download it instead.",
            ));
        }
    };
    let disposition = disposition("inline", &item.name);
    send_file(&state, &request, blob, content_type, disposition, policy).await
}

/// `GET /items/{id}/thumbnail`: a small picture of the file's current
/// version, as WebP, or a text file's first lines, as UTF-8, for the grid.
/// The web app adds the version's hash to the address, such as `?v=3f9a…`,
/// so browsers can keep each one for a year.
async fn thumbnail(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Response, ApiError> {
    let item = state.owned(me.user.id, id).await?;
    let ItemKind::File { blob, .. } = item.kind else {
        return Err(storage::Error::NotAFile.into());
    };
    let none = || ApiError::new(StatusCode::NOT_FOUND, "This file doesn't have a thumbnail.");
    let found = state.thumbnails.find(blob.hash).ok_or_else(none)?;
    // Thumbnails of contents that are gone are removed now and then.
    let thumbnail = tokio::fs::read(found.path).await.map_err(|_| none())?;
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(found.kind.content_type()),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    Ok((headers, thumbnail).into_response())
}

#[derive(Deserialize)]
struct ZipQuery {
    /// Item IDs, separated by commas.
    ids: String,
}

/// `GET /zip?ids=…`: a folder, or several items, as one ZIP file, built as
/// it's sent.
async fn zip_download(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Query(query): Query<ZipQuery>,
) -> Result<Response, ApiError> {
    let ids = query
        .ids
        .split(',')
        .filter(|id| !id.is_empty())
        .map(|id| {
            ItemId::deserialize(
                serde::de::value::StrDeserializer::<serde::de::value::Error>::new(id),
            )
            .map_err(|_| ApiError::not_found())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Choose what to download, then try again.",
        ));
    }
    // Items in the trash, or someone else's, are as good as missing.
    let wanted = ids.len();
    let items: Vec<Item> = state
        .storage
        .live_items(ids)
        .await?
        .into_iter()
        .map(|found| found.item)
        .filter(|item| item.owner == me.user.id)
        .collect();
    if items.len() != wanted {
        return Err(ApiError::not_found());
    }
    let name = match items.as_slice() {
        [folder] if folder.kind == ItemKind::Folder => format!("{}.zip", folder.name),
        _ => "Drive download.zip".to_owned(),
    };
    let hold = state.storage.hold_contents().await;
    let entries = archive::entries(&state.storage, items).await?;
    // A few chunks ahead of the browser at most, so memory stays small.
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    tokio::task::spawn_blocking(move || archive::write(entries, hold, sender));
    let body = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|chunk| (chunk, receiver))
    });
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    headers.insert(CONTENT_DISPOSITION, disposition("attachment", &name));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok((headers, Body::from_stream(body)).into_response())
}

/// `GET /items/{id}/versions`: every saved version of a file, newest first.
async fn versions(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<Vec<VersionView>>, ApiError> {
    state.owned(me.user.id, id).await?;
    let versions = state.storage.versions(id).await?;
    Ok(Json(versions.into_iter().map(VersionView::from).collect()))
}

/// `GET /items/{id}/versions/{version}/content`: downloads an older version.
async fn download_version(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path((id, version)): Path<(ItemId, VersionId)>,
    request: HeaderMap,
) -> Result<Response, ApiError> {
    let item = state.owned(me.user.id, id).await?;
    let versions = state.storage.versions(id).await?;
    let version = versions
        .into_iter()
        .find(|v| v.id == version)
        .ok_or_else(ApiError::not_found)?;
    file_response(&state, &request, &item.name, version.blob).await
}

/// `POST /items/{id}/versions/{version}/restore`: makes an older version
/// current again.
async fn restore_version(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path((id, version)): Path<(ItemId, VersionId)>,
) -> ItemResult {
    state.owned(me.user.id, id).await?;
    let restored = state.storage.restore_version(id, version).await?;
    Ok(Json(state.view(me.user.id, restored).await?))
}

/// `GET /trash`: what the user has deleted, newest first.
async fn trash(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<Vec<ItemView>>, ApiError> {
    let items = state.storage.trash(me.user.id).await?;
    Ok(Json(state.views(me.user.id, items).await?))
}

/// `DELETE /trash`: deletes everything in the trash for good.
async fn empty_trash(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<StatusCode, ApiError> {
    state.storage.empty_trash(me.user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /trash/{id}`: deletes one item in the trash for good.
async fn delete_forever(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<StatusCode, ApiError> {
    state.owned(me.user.id, id).await?;
    state.storage.delete_forever(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /usage`: how much space the user's files take, and their limit.
async fn usage(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<UsageView>, ApiError> {
    Ok(Json(state.storage.usage(me.user.id).await?.into()))
}

#[derive(Deserialize)]
struct NewUpload {
    size: u64,
}

/// `POST /uploads`: starts a resumable upload of `size` bytes. Send the
/// pieces with `PATCH /uploads/{id}`, then save the file with
/// `POST /uploads/{id}/finish`.
async fn start_upload(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Json(request): Json<NewUpload>,
) -> Result<(StatusCode, Json<UploadView>), ApiError> {
    let upload = state.storage.start_upload(me.user.id, request.size).await?;
    Ok((StatusCode::CREATED, Json(upload.into())))
}

/// `GET /uploads/{id}`: how much of an upload has arrived, so a client
/// whose connection dropped knows where to continue.
async fn upload(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<UploadId>,
) -> Result<Json<UploadView>, ApiError> {
    Ok(Json(owned_upload(&state, me.user.id, id).await?.into()))
}

/// `PATCH /uploads/{id}`: adds the request body to an upload. The
/// `Upload-Offset` header says where the piece starts, which must be how
/// much has arrived so far.
async fn append(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<UploadId>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<UploadView>, ApiError> {
    owned_upload(&state, me.user.id, id).await?;
    let offset = headers
        .get(UPLOAD_OFFSET)
        .and_then(|value| value.to_str().ok()?.parse().ok())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "Each piece of an upload needs an Upload-Offset header that says where it \
                 starts. Ask for the upload's progress, then send the rest from there.",
            )
        })?;
    let upload = state.storage.append(id, offset, reader(body)).await?;
    Ok(Json(upload.into()))
}

#[derive(Deserialize)]
struct FinishUpload {
    parent: ItemId,
    name: String,
}

/// `POST /uploads/{id}/finish`: saves a finished upload as a file, or as a
/// new version if a file with that name is already there.
async fn finish_upload(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<UploadId>,
    Json(request): Json<FinishUpload>,
) -> ItemResult {
    owned_upload(&state, me.user.id, id).await?;
    state.owned(me.user.id, request.parent).await?;
    let file = state
        .storage
        .finish_upload(id, request.parent, &request.name)
        .await?;
    Ok(Json(state.view(me.user.id, file).await?))
}

/// `DELETE /uploads/{id}`: stops an upload and deletes what arrived.
async fn cancel_upload(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<UploadId>,
) -> Result<StatusCode, ApiError> {
    owned_upload(&state, me.user.id, id).await?;
    state.storage.cancel_upload(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn owned_upload(state: &DriveState, user: UserId, id: UploadId) -> Result<Upload, ApiError> {
    match state.storage.upload(id).await {
        Ok(upload) if upload.owner == user => Ok(upload),
        Ok(_) | Err(storage::Error::UploadNotFound) => Err(ApiError::not_found()),
        Err(err) => Err(err.into()),
    }
}

/// Reads a request body as it arrives, without holding it all in memory.
fn reader(body: Body) -> impl AsyncRead + Unpin + Send {
    StreamReader::new(body.into_data_stream().map_err(io::Error::other))
}

/// A kind of file the browser can show itself.
enum Preview {
    /// With its type, such as "image/png".
    Image(&'static str),
    Pdf,
    /// With its type, such as "video/mp4".
    Video(&'static str),
    /// With its type, such as "audio/mpeg".
    Audio(&'static str),
}

/// Whether a file's name lets it show as text: a document, a spreadsheet,
/// or a type Accordion doesn't know, such as code.
fn text_kind(name: &str) -> bool {
    let Some((_, extension)) = name.rsplit_once('.') else {
        return true;
    };
    let extension = extension.to_ascii_lowercase();
    TYPES
        .iter()
        .find(|(_, extensions)| extensions.contains(&extension.as_str()))
        .is_none_or(|(kind, _)| matches!(*kind, "documents" | "spreadsheets"))
}

/// Whether the browser can show a file, going by its name's extension.
///
/// The file is sent as exactly this type, and the browser is told not to
/// guess another, so a file that isn't what its name says fails to show
/// rather than running as something else. SVG images are left out because
/// they can hold scripts, and HEIC photos because most browsers can't
/// show them.
fn preview(name: &str) -> Option<Preview> {
    let (_, extension) = name.rsplit_once('.')?;
    let image = match extension.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "pdf" => return Some(Preview::Pdf),
        // QuickTime files are MP4's older form, and browsers that play one
        // play the other when it's sent as MP4. Whether the video inside
        // plays depends on the browser and the device, so the web app
        // offers the download when it doesn't.
        "mp4" | "m4v" | "mov" => return Some(Preview::Video("video/mp4")),
        "webm" => return Some(Preview::Video("video/webm")),
        "mp3" => return Some(Preview::Audio("audio/mpeg")),
        "m4a" => return Some(Preview::Audio("audio/mp4")),
        "aac" => return Some(Preview::Audio("audio/aac")),
        "ogg" | "oga" | "opus" => return Some(Preview::Audio("audio/ogg")),
        "wav" => return Some(Preview::Audio("audio/wav")),
        "flac" => return Some(Preview::Audio("audio/flac")),
        _ => return None,
    };
    Some(Preview::Image(image))
}

/// Sends a file as a download.
///
/// Files come from users, so a browser must never treat one as part of
/// Accordion's own pages: a crafted HTML file could otherwise run scripts
/// with the user's session. The headers make the browser save the file,
/// stop it guessing the type, and sandbox it if it's shown anyway.
async fn file_response(
    state: &DriveState,
    request: &HeaderMap,
    name: &str,
    blob: Blob,
) -> Result<Response, ApiError> {
    let disposition = disposition("attachment", name);
    send_file(
        state,
        request,
        blob,
        "application/octet-stream",
        disposition,
        "sandbox",
    )
    .await
}

/// Sends a file, or the part a `Range` header asks for. Video players ask
/// for parts to start playing and to skip ahead, and Safari plays no video
/// without them. Downloads use them to pick up where they stopped.
async fn send_file(
    state: &DriveState,
    request: &HeaderMap,
    blob: Blob,
    content_type: &'static str,
    disposition: HeaderValue,
    policy: &'static str,
) -> Result<Response, ApiError> {
    let etag = format!("\"{}\"", blob.hash);
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CONTENT_DISPOSITION, disposition);
    headers.insert(
        ETAG,
        HeaderValue::try_from(&etag).expect("hex is valid header text"),
    );
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(policy));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    let range = match requested_range(request, blob.size, &etag) {
        Ok(range) => range,
        Err(Unsatisfiable) => {
            headers.insert(
                CONTENT_RANGE,
                HeaderValue::try_from(format!("bytes */{}", blob.size)).expect("digits are valid"),
            );
            return Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response());
        }
    };
    let mut file = state.storage.open_contents(blob.hash).await?;
    let Some(range) = range else {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(blob.size));
        return Ok((headers, Body::from_stream(ReaderStream::new(file))).into_response());
    };
    file.seek(io::SeekFrom::Start(range.start))
        .await
        .map_err(storage::Error::from)?;
    headers.insert(CONTENT_LENGTH, HeaderValue::from(range.end - range.start));
    headers.insert(
        CONTENT_RANGE,
        HeaderValue::try_from(format!(
            "bytes {}-{}/{}",
            range.start,
            range.end - 1,
            blob.size
        ))
        .expect("digits are valid"),
    );
    let part = file.take(range.end - range.start);
    Ok((
        StatusCode::PARTIAL_CONTENT,
        headers,
        Body::from_stream(ReaderStream::new(part)),
    )
        .into_response())
}

/// A `Range` header that asks only for bytes past the end of the file.
#[derive(Debug, PartialEq, Eq)]
struct Unsatisfiable;

/// The bytes a `Range` header asks for, out of a file of `size` bytes with
/// `etag`. `None` means the whole file: there's no header, or one this
/// doesn't serve, such as several ranges at once, or an `If-Range` for an
/// older version (RFC 9110, section 14).
fn requested_range(
    request: &HeaderMap,
    size: u64,
    etag: &str,
) -> Result<Option<Range<u64>>, Unsatisfiable> {
    let Some(spec) = request
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().strip_prefix("bytes="))
    else {
        return Ok(None);
    };
    if request
        .get(IF_RANGE)
        .is_some_and(|value| value.as_bytes() != etag.as_bytes())
    {
        return Ok(None);
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let number = |text: &str| text.trim().parse::<u64>().ok();
    let range = match (first.trim().is_empty(), last.trim().is_empty()) {
        // "-500": the last 500 bytes.
        (true, false) => match number(last) {
            Some(0) => return Err(Unsatisfiable),
            Some(count) => size.saturating_sub(count)..size,
            None => return Ok(None),
        },
        // "500-": from byte 500 to the end.
        (false, true) => match number(first) {
            Some(start) => start..size,
            None => return Ok(None),
        },
        // "500-999", which includes byte 999.
        (false, false) => match (number(first), number(last)) {
            (Some(start), Some(end)) if end >= start => start..size.min(end + 1),
            _ => return Ok(None),
        },
        (true, true) => return Ok(None),
    };
    if range.start >= size {
        return Err(Unsatisfiable);
    }
    Ok(Some(range))
}

/// A `Content-Disposition` header, "attachment" or "inline", that names the
/// file. `filename*` carries the exact name in any script (RFC 6266), and
/// `filename` is a plain ASCII stand-in for clients that don't read it.
fn disposition(kind: &str, name: &str) -> HeaderValue {
    let plain: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    HeaderValue::try_from(format!(
        "{kind}; filename=\"{plain}\"; filename*=UTF-8''{encoded}"
    ))
    .expect("the name is ASCII once encoded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachments_keep_any_name() {
        assert_eq!(
            disposition("attachment", "report.pdf"),
            "attachment; filename=\"report.pdf\"; filename*=UTF-8''report.pdf"
        );
        assert_eq!(
            disposition("attachment", "Ünïcode \"quoted\".txt"),
            "attachment; filename=\"_n_code _quoted_.txt\"; \
             filename*=UTF-8''%C3%9Cn%C3%AFcode%20%22quoted%22.txt"
        );
    }
}
