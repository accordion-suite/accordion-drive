//! `/api/drive`: the JSON API that Accordion's web, desktop, and mobile apps
//! use.
//!
//! Items are files and folders, named by ID. Each request checks that the
//! signed-in user owns what it names, and answers "not found" otherwise.

use std::io;

use accordion_core::storage::{self, Blob, Item, ItemKind, Upload, Usage, Version};
use accordion_module::{ApiError, Authenticated};
use accordion_types::{ItemId, UploadId, UserId, VersionId};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    ETAG, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;
use tokio_util::io::{ReaderStream, StreamReader};

use crate::DriveState;

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
        .route("/items/{id}/restore", post(restore))
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
    created_at: String,
    modified_at: String,
    /// When it was deleted, if it's in the trash itself.
    trashed_at: Option<String>,
}

impl From<Item> for ItemView {
    fn from(item: Item) -> Self {
        let (kind, version, blob) = match item.kind {
            ItemKind::Folder => ("folder", None, None),
            ItemKind::File { version, blob } => ("file", Some(version), Some(blob)),
        };
        Self {
            id: item.id,
            parent: item.parent,
            name: item.name,
            kind,
            version,
            size: blob.map(|blob| blob.size),
            sha256: blob.map(|blob| blob.hash.to_hex()),
            created_at: item.created_at,
            modified_at: item.modified_at,
            trashed_at: item.trashed_at,
        }
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
    Ok(Json(item.into()))
}

/// `GET /items/{id}`
async fn item(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> ItemResult {
    Ok(Json(state.owned(me.user.id, id).await?.into()))
}

/// `GET /items/{id}/children`: what's in a folder, sorted by name.
async fn children(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<Vec<ItemView>>, ApiError> {
    state.owned(me.user.id, id).await?;
    let children = state.storage.children(id).await?;
    Ok(Json(children.into_iter().map(ItemView::from).collect()))
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
    Ok((StatusCode::CREATED, Json(folder.into())))
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
    Ok(Json(file.into()))
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
    Ok(Json(
        state.storage.move_item(id, parent, &name).await?.into(),
    ))
}

/// `DELETE /items/{id}`: moves an item to the trash.
async fn trash_item(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> ItemResult {
    state.owned(me.user.id, id).await?;
    Ok(Json(state.storage.delete(id).await?.into()))
}

/// `POST /items/{id}/restore`: takes an item out of the trash.
async fn restore(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> ItemResult {
    state.owned(me.user.id, id).await?;
    Ok(Json(state.storage.restore(id).await?.into()))
}

/// `GET /items/{id}/content`: downloads the current version of a file.
async fn download(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Response, ApiError> {
    let item = state.owned(me.user.id, id).await?;
    let ItemKind::File { blob, .. } = item.kind else {
        return Err(storage::Error::NotAFile.into());
    };
    file_response(&state, &item.name, blob).await
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
) -> Result<Response, ApiError> {
    let item = state.owned(me.user.id, id).await?;
    let versions = state.storage.versions(id).await?;
    let version = versions
        .into_iter()
        .find(|v| v.id == version)
        .ok_or_else(ApiError::not_found)?;
    file_response(&state, &item.name, version.blob).await
}

/// `POST /items/{id}/versions/{version}/restore`: makes an older version
/// current again.
async fn restore_version(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path((id, version)): Path<(ItemId, VersionId)>,
) -> ItemResult {
    state.owned(me.user.id, id).await?;
    Ok(Json(
        state.storage.restore_version(id, version).await?.into(),
    ))
}

/// `GET /trash`: what the user has deleted, newest first.
async fn trash(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<Vec<ItemView>>, ApiError> {
    let items = state.storage.trash(me.user.id).await?;
    Ok(Json(items.into_iter().map(ItemView::from).collect()))
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
    Ok(Json(file.into()))
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

/// Sends a file as a download.
///
/// Files come from users, so a browser must never treat one as part of
/// Accordion's own pages: a crafted HTML file could otherwise run scripts
/// with the user's session. The headers make the browser save the file,
/// stop it guessing the type, and sandbox it if it's shown anyway.
async fn file_response(state: &DriveState, name: &str, blob: Blob) -> Result<Response, ApiError> {
    let file = state.storage.open_contents(blob.hash).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(CONTENT_LENGTH, HeaderValue::from(blob.size));
    headers.insert(CONTENT_DISPOSITION, attachment(name));
    headers.insert(
        ETAG,
        HeaderValue::try_from(format!("\"{}\"", blob.hash)).expect("hex is valid header text"),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    Ok((headers, Body::from_stream(ReaderStream::new(file))).into_response())
}

/// A `Content-Disposition` header that saves the file under its own name.
/// `filename*` carries the exact name in any script (RFC 6266), and
/// `filename` is a plain ASCII stand-in for clients that don't read it.
fn attachment(name: &str) -> HeaderValue {
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
        "attachment; filename=\"{plain}\"; filename*=UTF-8''{encoded}"
    ))
    .expect("the name is ASCII once encoded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachments_keep_any_name() {
        assert_eq!(
            attachment("report.pdf"),
            "attachment; filename=\"report.pdf\"; filename*=UTF-8''report.pdf"
        );
        assert_eq!(
            attachment("Ünïcode \"quoted\".txt"),
            "attachment; filename=\"_n_code _quoted_.txt\"; \
             filename*=UTF-8''%C3%9Cn%C3%AFcode%20%22quoted%22.txt"
        );
    }
}
