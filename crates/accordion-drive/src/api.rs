//! `/api/drive`: the JSON API that Accordion's web, desktop, and mobile apps
//! use.
//!
//! Items are files and folders, named by ID. Each request asks core what
//! the signed-in user may do with what it names, and answers "not found"
//! if it isn't theirs and nobody shared it with them. Lists that reach
//! across folders, such as search results, say where each item is, as the
//! names of the folders it's in, starting from the shared folder for items
//! someone else owns.

use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::Range;

use accordion_core::events::{Action, NewEvent};
use accordion_core::storage::{
    self, Access, Blob, Found, Item, ItemKind, Only, Owner, Place, Search, Upload, Usage, Version,
};
use accordion_core::texts::Words;
use accordion_core::thumbnails::{ThumbnailKind, Thumbnails};
use accordion_module::auth::actor;
use accordion_module::{ApiError, Authenticated};
use accordion_types::{ItemId, UploadId, UserId, VersionId};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_MATCH, IF_NONE_MATCH, IF_RANGE, RANGE,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio_util::io::{ReaderStream, StreamReader};

use crate::settings::{DriveSettings, NewSettings};
use crate::{DriveState, activity, archive, colors, descriptions, links, sharing, sync, time};

/// Says where a piece of a resumable upload starts. The name follows the
/// tus protocol, which uses the same header for the same purpose.
const UPLOAD_OFFSET: &str = "upload-offset";

pub(crate) fn routes(state: DriveState) -> Router {
    Router::new()
        .route("/top", get(top))
        .route("/items/{id}", get(item).patch(move_item).delete(trash_item))
        .route("/items/{id}/children", get(children).post(create_folder))
        .route("/items/{id}/children/{name}", put(write_file))
        .route("/items/{id}/content", get(download).put(write_version))
        .route("/items/{id}/preview", get(preview_file))
        .route("/items/{id}/preview/{name}", get(preview_named))
        .route("/items/{id}/thumbnail", get(thumbnail))
        .route("/items/{id}/restore", post(restore))
        .route("/items/{id}/star", put(star).delete(unstar))
        .route("/items/{id}/color", put(set_color).delete(clear_color))
        .route(
            "/items/{id}/description",
            get(description).put(set_description),
        )
        .route("/largest", get(largest))
        .route("/items/{id}/copy", post(copy))
        .route("/items/{id}/versions", get(versions))
        .route(
            "/items/{id}/shares",
            get(sharing::shares).delete(sharing::unshare_all),
        )
        .route(
            "/items/{id}/shares/{user}",
            put(sharing::share).delete(sharing::unshare),
        )
        .route("/items/{id}/activity", get(activity::activity))
        .route(
            "/notifications",
            get(activity::notifications).delete(activity::clear),
        )
        .route("/notifications/seen", post(activity::mark_seen))
        .route("/notifications/read", post(activity::mark_read))
        .route("/notifications/remove", post(activity::remove))
        .route(
            "/items/{id}/links",
            get(links::item_links).post(links::make_link),
        )
        .route("/links", get(links::all_links))
        .route("/links/{id}", delete(links::remove_link))
        .route("/shared", get(sharing::shared_with_me))
        .route("/shared-by-me", get(sharing::shared_by_me))
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
        .route("/retention", get(retention))
        .route("/recent", get(recent))
        .route("/starred", get(starred))
        .route("/search", get(search))
        .route("/zip", get(zip_download))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/types", get(get_types))
        .route("/uploads", post(start_upload))
        .route(
            "/uploads/{id}",
            get(upload).patch(append).delete(cancel_upload),
        )
        .route("/uploads/{id}/finish", post(finish_upload))
        .route("/sync/start", get(sync::start))
        .route("/sync/folders/{id}", get(sync::folder))
        .route("/sync/changes", get(sync::changes))
        .with_state(state)
}

#[derive(Serialize)]
pub(crate) struct ItemView {
    id: ItemId,
    /// `None` for the user's top folder.
    parent: Option<ItemId>,
    name: String,
    kind: &'static str,
    /// For files: the current version, its size, and its SHA-256 hash.
    version: Option<VersionId>,
    size: Option<u64>,
    sha256: Option<String>,
    /// For files the browser can show itself: "image", "pdf", "video", or
    /// "audio", or "document" for an Office document that core has made a
    /// PDF of. The web app opens these from `/items/{id}/preview`.
    preview: Option<&'static str>,
    /// For a document: whether its preview stops at its first pages, so the
    /// document may go on past them.
    preview_partial: bool,
    /// What `/items/{id}/thumbnail` has for the file yet: "picture", a small
    /// WebP, or "text", its first lines. Core makes them in the background,
    /// so a new photo gets one soon.
    thumbnail: Option<&'static str>,
    /// Changes each time core makes the thumbnail again, such as when a
    /// newer worker makes a better one. Browsers keep a thumbnail for a
    /// year, so the web app puts this in its address with the hash.
    thumbnail_version: Option<String>,
    /// For files: which of [`TYPES`] it is, such as "photos", or null for
    /// none. The web app shows its icon by this.
    #[serde(rename = "type")]
    file_type: Option<&'static str>,
    created_at: String,
    modified_at: String,
    /// When it was deleted, if it's in the trash itself.
    trashed_at: Option<String>,
    /// Whether the signed-in user starred it.
    starred: bool,
    /// What the signed-in user may do with it: "owner", "editor", or
    /// "viewer".
    access: &'static str,
    /// The owner's name, when it's someone else's.
    owner: Option<String>,
    /// For the owner: whether they shared it with anyone, or it has a link
    /// that works.
    shared: bool,
    /// For a folder: the color the signed-in user picked for it, if any.
    color: Option<String>,
}

/// What views need to know about the signed-in user.
pub(crate) struct Reader {
    user: UserId,
    stars: HashSet<ItemId>,
    /// Items that are shared with someone or have a link that works. Only
    /// the user's own are marked.
    shared: HashSet<ItemId>,
    /// The names of the people who own the other items being shown.
    names: HashMap<UserId, String>,
    /// The folder colors they picked.
    colors: HashMap<ItemId, String>,
}

pub(crate) fn access_name(access: Access) -> &'static str {
    match access {
        Access::Owner => "owner",
        Access::Editor => "editor",
        Access::Viewer => "viewer",
    }
}

impl ItemView {
    pub(crate) fn new(
        item: Item,
        access: Access,
        reader: &Reader,
        thumbnails: &Thumbnails,
    ) -> Self {
        let starred = reader.stars.contains(&item.id);
        let color = reader.colors.get(&item.id).cloned();
        let shared = item.owner == reader.user && reader.shared.contains(&item.id);
        let owner = (item.owner != reader.user)
            .then(|| reader.names.get(&item.owner).cloned().unwrap_or_default());
        let (kind, version, blob) = match item.kind {
            ItemKind::Folder => ("folder", None, None),
            ItemKind::File { version, blob } => ("file", Some(version), Some(blob)),
        };
        let document = blob.and_then(|blob| thumbnails.preview(blob.hash));
        let picture = blob.is_some_and(|blob| thumbnails.picture(blob.hash).is_some());
        // A sound the browser can't play, such as a Windows Media song,
        // which core has converted to AAC.
        let sound = blob.is_some_and(|blob| thumbnails.sound(blob.hash).is_some());
        let preview = match (blob.and(preview(&item.name)), &document) {
            _ if sound => Some("audio"),
            (Some(Preview::Image(_)), _) => Some("image"),
            (Some(Preview::Pdf), _) => Some("pdf"),
            (Some(Preview::Video(_)), _) => Some("video"),
            (Some(Preview::Audio(_)), _) => Some("audio"),
            (None, Some(_)) => Some("document"),
            // A picture the browser can't show, such as a HEIC, which core
            // has made a large WebP of.
            (None, None) if picture => Some("image"),
            (None, None) => None,
        };
        let preview_partial = document.is_some_and(|document| document.partial);
        let found = blob.and_then(|blob| thumbnails.find(blob.hash));
        let file_type = match blob
            .and_then(|_| item_type(&item.name, found.as_ref().map(|found| found.kind)))
        {
            known @ Some("audio" | "videos") => known,
            // Core found sound in it, such as Ensoniq PARIS audio in a
            // `.paf`, which is mostly family trees.
            _ if sound => Some("audio"),
            other => other,
        };
        let thumbnail = found.as_ref().and_then(|found| match found.kind {
            ThumbnailKind::Picture => Some("picture"),
            // A song or a video that happens to be text, such as a
            // placeholder, would look broken as a page of text.
            ThumbnailKind::Text => text_kind(file_type).then_some("text"),
        });
        let thumbnail_version = thumbnail.and(found.as_ref()).map(|found| found.version());
        Self {
            id: item.id,
            parent: item.parent,
            name: item.name,
            kind,
            version,
            size: blob.map(|blob| blob.size),
            sha256: blob.map(|blob| blob.hash.to_hex()),
            preview,
            preview_partial,
            thumbnail,
            thumbnail_version,
            file_type,
            created_at: item.created_at,
            modified_at: item.modified_at,
            trashed_at: item.trashed_at,
            starred,
            access: access_name(access),
            owner,
            shared,
            color,
        }
    }
}

/// An item in a list that reaches across folders.
#[derive(Serialize)]
pub(crate) struct FoundView {
    #[serde(flatten)]
    pub(crate) item: ItemView,
    /// The names of the folders it's in, outermost first, not counting the
    /// top folder. For someone else's item, they start at the folder that
    /// was shared, and are empty for the shared item itself.
    pub(crate) folders: Vec<String>,
}

impl DriveState {
    /// What views of items with these `owners` need to know about `user`.
    pub(crate) async fn reader(
        &self,
        user: UserId,
        owners: impl IntoIterator<Item = UserId>,
    ) -> Result<Reader, ApiError> {
        let others: HashSet<UserId> = owners.into_iter().filter(|&owner| owner != user).collect();
        let mut names = HashMap::new();
        for owner in others {
            let name = self
                .identity
                .user(owner)
                .await?
                .map(|owner| owner.display_name)
                .unwrap_or_default();
            names.insert(owner, name);
        }
        Ok(Reader {
            user,
            stars: self.stars.set(user).await?,
            shared: self
                .storage
                .shared_by_owner(user)
                .await?
                .into_iter()
                .chain(self.links.linked_items().await?)
                .collect(),
            names,
            colors: self.colors.all(user).await?,
        })
    }

    async fn view(&self, user: UserId, item: Item, access: Access) -> Result<ItemView, ApiError> {
        let reader = self.reader(user, [item.owner]).await?;
        let mut view = ItemView::new(item, access, &reader, &self.thumbnails);
        self.size_folders(std::slice::from_mut(&mut view)).await?;
        Ok(view)
    }

    /// Fills in each folder's size: what's in it, however deep, as
    /// Nextcloud shows.
    pub(crate) async fn size_folders<'a>(
        &self,
        views: impl IntoIterator<Item = &'a mut ItemView>,
    ) -> Result<(), ApiError> {
        let folders: Vec<&mut ItemView> = views
            .into_iter()
            .filter(|view| view.kind == "folder")
            .collect();
        if folders.is_empty() {
            return Ok(());
        }
        let sizes = self
            .storage
            .folder_sizes(folders.iter().map(|view| view.id).collect())
            .await?;
        for view in folders {
            view.size = sizes.get(&view.id).copied();
        }
        Ok(())
    }

    /// Views of what's in a folder that `user` has `access` to. A share on
    /// an item inside can give them more.
    async fn child_views(
        &self,
        user: UserId,
        items: Vec<Item>,
        access: Access,
    ) -> Result<Vec<ItemView>, ApiError> {
        let direct: HashMap<ItemId, Access> = match access {
            Access::Owner => HashMap::new(),
            _ => self
                .storage
                .shared_with(user)
                .await?
                .into_iter()
                .map(|shared| (shared.item.id, shared.share.level.into()))
                .collect(),
        };
        let reader = self
            .reader(
                user,
                items.iter().map(|item| item.owner).collect::<Vec<_>>(),
            )
            .await?;
        let mut views: Vec<ItemView> = items
            .into_iter()
            .map(|item| {
                let access = direct
                    .get(&item.id)
                    .map_or(access, |&more| access.max(more));
                ItemView::new(item, access, &reader, &self.thumbnails)
            })
            .collect();
        self.size_folders(&mut views).await?;
        Ok(views)
    }

    /// Views of items found across folders, leaving out any `user` can't
    /// reach.
    pub(crate) async fn found_views(
        &self,
        user: UserId,
        found: Vec<Found>,
    ) -> Result<Json<Vec<FoundView>>, ApiError> {
        self.found_list(user, found).await.map(Json)
    }

    /// Views of `found`, leaving out any the user can no longer open.
    pub(crate) async fn found_list(
        &self,
        user: UserId,
        found: Vec<Found>,
    ) -> Result<Vec<FoundView>, ApiError> {
        let mut reached = Vec::with_capacity(found.len());
        for found in found {
            let access = match found.item.owner == user {
                true => Some(Access::Owner),
                false => self.storage.access(user, found.item.id).await?,
            };
            if let Some(access) = access {
                reached.push((found, access));
            }
        }
        let owners: Vec<UserId> = reached.iter().map(|(found, _)| found.item.owner).collect();
        let reader = self.reader(user, owners).await?;
        let mut views: Vec<FoundView> = reached
            .into_iter()
            .map(|(found, access)| FoundView {
                item: ItemView::new(found.item, access, &reader, &self.thumbnails),
                folders: found.folders,
            })
            .collect();
        self.size_folders(views.iter_mut().map(|view| &mut view.item))
            .await?;
        Ok(views)
    }
}

/// Storage says "your storage is full", which is wrong when someone saves
/// into a folder that someone else owns and shared with them.
fn space_error(err: storage::Error, owner: UserId, me: UserId) -> ApiError {
    match err {
        storage::Error::QuotaFull if owner != me => ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "The owner of this folder is out of space, so it can't take more files. Ask them \
             to free up some space.",
        ),
        err => err.into(),
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
    let (item, access) = state.reach(me.user.id, id, Access::Owner).await?;
    Ok(Json(state.view(me.user.id, item, access).await?))
}

/// `GET /items/{id}`
async fn item(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> ItemResult {
    let (item, access) = state.reach(me.user.id, id, Access::Viewer).await?;
    Ok(Json(state.view(me.user.id, item, access).await?))
}

/// `GET /items/{id}/children`: what's in a folder, sorted by name.
async fn children(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<Vec<ItemView>>, ApiError> {
    let (_, access) = state.reach(me.user.id, id, Access::Viewer).await?;
    let children = state.storage.children(id).await?;
    Ok(Json(state.child_views(me.user.id, children, access).await?))
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
    let (_, access) = state.reach(me.user.id, parent, Access::Editor).await?;
    let folder = state.by(&me).create_folder(parent, &request.name).await?;
    Ok((
        StatusCode::CREATED,
        Json(state.view(me.user.id, folder, access).await?),
    ))
}

/// `PUT /items/{id}/children/{name}`: saves the request body as a file, or
/// as a new version if a file with that name is already there. With
/// `If-None-Match: *`, the name must be free instead. For large files, use
/// a resumable upload instead.
async fn write_file(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path((parent, name)): Path<(ItemId, String)>,
    request: HeaderMap,
    body: Body,
) -> ItemResult {
    let (folder, access) = state.reach(me.user.id, parent, Access::Editor).await?;
    let storage = state.by(&me);
    let file = if new_only(&request) {
        storage.create_file(parent, &name, reader(body)).await
    } else {
        storage.write_file(parent, &name, reader(body)).await
    }
    .map_err(|err| space_error(err, folder.owner, me.user.id))?;
    Ok(Json(state.view(me.user.id, file, access).await?))
}

/// `PUT /items/{id}/content`: saves the request body as the file's new
/// version, as the web app's text editor does. `If-Match` holds the SHA-256
/// hash of the contents the editor opened, quoted, and the save is refused
/// if the file holds anything else by now, so nobody's changes are lost
/// without them knowing.
async fn write_version(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    request: HeaderMap,
    body: Body,
) -> ItemResult {
    let (item, access) = state.reach(me.user.id, id, Access::Editor).await?;
    let base = replaced(&request)?;
    let file = state
        .by(&me)
        .write_version(id, base, reader(body))
        .await
        .map_err(|err| space_error(err, item.owner, me.user.id))?;
    Ok(Json(state.view(me.user.id, file, access).await?))
}

/// The contents a save replaces: the SHA-256 hash in `If-Match`, quoted.
fn replaced(request: &HeaderMap) -> Result<storage::ContentHash, ApiError> {
    request
        .get(IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix('"')?.strip_suffix('"'))
        .and_then(storage::ContentHash::from_hex)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::PRECONDITION_REQUIRED,
                "Say which contents this replaces, as their SHA-256 hash in If-Match.",
            )
        })
}

/// Whether a save asks for a new file only, with `If-None-Match: *`.
fn new_only(request: &HeaderMap) -> bool {
    request.get(IF_NONE_MATCH).is_some_and(|value| value == "*")
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
    let (item, access) = state.reach(me.user.id, id, Access::Editor).await?;
    // Editors move items only between folders they can edit, so nothing
    // leaves the shared folder unless its owner moves it.
    let parent = match request.parent {
        Some(parent) => state.reach(me.user.id, parent, Access::Editor).await?.0.id,
        None => item.parent.ok_or(storage::Error::TopFolder)?,
    };
    let name = request.name.unwrap_or(item.name);
    let moved = state.by(&me).move_item(id, parent, &name).await?;
    Ok(Json(state.view(me.user.id, moved, access).await?))
}

/// `DELETE /items/{id}`: moves an item to the trash.
async fn trash_item(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> ItemResult {
    let (_, access) = state.reach(me.user.id, id, Access::Editor).await?;
    let deleted = state.by(&me).delete(id).await?;
    Ok(Json(state.view(me.user.id, deleted, access).await?))
}

/// `POST /items/{id}/restore`: takes an item out of the trash.
async fn restore(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> ItemResult {
    let (_, access) = state.reach(me.user.id, id, Access::Owner).await?;
    let restored = state.by(&me).restore(id).await?;
    Ok(Json(state.view(me.user.id, restored, access).await?))
}

/// `GET /settings`: the signed-in person's Drive settings.
async fn get_settings(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<DriveSettings>, ApiError> {
    Ok(Json(state.settings.get(me.user.id).await?))
}

/// `PUT /settings`: replaces them, so send every setting. What a click does
/// can be left out, and stays as it was.
async fn put_settings(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Json(settings): Json<NewSettings>,
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
    state.reach(me.user.id, id, Access::Viewer).await?;
    state.stars.star(me.user.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /items/{id}/star`: takes the star off an item.
async fn unstar(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<StatusCode, ApiError> {
    state.reach(me.user.id, id, Access::Viewer).await?;
    state.stars.unstar(me.user.id, vec![id]).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ColorRequest {
    color: String,
}

/// `PUT /items/{id}/color`: gives a folder a color, for the signed-in user
/// only, as with stars.
async fn set_color(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Json(request): Json<ColorRequest>,
) -> Result<StatusCode, ApiError> {
    let (item, _) = state.reach(me.user.id, id, Access::Viewer).await?;
    if !matches!(item.kind, ItemKind::Folder) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Only folders have colors.",
        ));
    }
    let color = colors::COLORS
        .iter()
        .copied()
        .find(|&color| color == request.color)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "Accordion doesn't have the color \"{}\". Choose another.",
                    request.color
                ),
            )
        })?;
    state.colors.set(me.user.id, id, Some(color)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /items/{id}/color`: gives a folder its usual color back.
async fn clear_color(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<StatusCode, ApiError> {
    state.reach(me.user.id, id, Access::Viewer).await?;
    state.colors.set(me.user.id, id, None).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Serialize)]
struct DescriptionBody {
    description: String,
}

/// `GET /items/{id}/description`: what the item's description says, or an
/// empty one.
async fn description(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<DescriptionBody>, ApiError> {
    state.reach(me.user.id, id, Access::Viewer).await?;
    let description = state.descriptions.get(id).await?.unwrap_or_default();
    Ok(Json(DescriptionBody { description }))
}

/// `PUT /items/{id}/description`: changes the item's description, or
/// removes it when it's empty. Anyone who can edit the item can.
async fn set_description(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Json(request): Json<DescriptionBody>,
) -> Result<Json<DescriptionBody>, ApiError> {
    let (item, _) = state.reach(me.user.id, id, Access::Editor).await?;
    let description = request.description.trim().to_owned();
    if description.chars().count() > descriptions::MAX_LEN {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "A description can be up to {} characters. Shorten it, then save it again.",
                descriptions::MAX_LEN
            ),
        ));
    }
    let before = state.descriptions.get(id).await?.unwrap_or_default();
    if before != description {
        state
            .descriptions
            .set(me.user.id, id, description.clone())
            .await?;
        state
            .events
            .record(
                actor(&me),
                NewEvent::about(Action::DescriptionChanged, id, item.owner),
            )
            .await?;
    }
    Ok(Json(DescriptionBody { description }))
}

/// How many files `/largest` lists.
const LARGEST: usize = 100;

/// `GET /largest`: the signed-in user's largest files outside the trash,
/// largest first, for the storage page.
async fn largest(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<Vec<FoundView>>, ApiError> {
    let found = state.storage.largest(me.user.id, LARGEST).await?;
    state.found_views(me.user.id, found).await
}

#[derive(Deserialize)]
struct CopyRequest {
    /// The folder to copy into. Leave it out for the same folder, or for
    /// the top folder if the user can't add to that one.
    parent: Option<ItemId>,
    /// The copy's name, such as "Copy of Beach.jpg". It can't be taken,
    /// unless the copy goes to the top folder in place of a folder the user
    /// can't add to, where it gets a number.
    name: String,
}

/// `POST /items/{id}/copy`: copies a file. Folders can't be copied.
async fn copy(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Json(request): Json<CopyRequest>,
) -> Result<(StatusCode, Json<ItemView>), ApiError> {
    let (item, _) = state.reach(me.user.id, id, Access::Viewer).await?;
    let mut name = request.name;
    let (parent, access) = match request.parent {
        Some(parent) => {
            let (folder, access) = state.reach(me.user.id, parent, Access::Editor).await?;
            (folder, access)
        }
        None => {
            let here = item.parent.ok_or(storage::Error::NotAFile)?;
            match state.storage.access(me.user.id, here).await? {
                Some(access) if access >= Access::Editor => {
                    let folder = state
                        .storage
                        .item(here)
                        .await?
                        .ok_or_else(ApiError::not_found)?;
                    (folder, access)
                }
                // A viewer keeps their copy in their own Drive.
                _ => {
                    let top = state.storage.top_folder(me.user.id).await?;
                    let mut taken: HashSet<String> = state
                        .storage
                        .children(top)
                        .await?
                        .into_iter()
                        .map(|child| child.name)
                        .collect();
                    name = archive::unique(&name, &mut taken);
                    let folder = state
                        .storage
                        .item(top)
                        .await?
                        .ok_or_else(ApiError::not_found)?;
                    (folder, Access::Owner)
                }
            }
        }
    };
    let copy = state
        .by(&me)
        .copy_file(id, parent.id, &name)
        .await
        .map_err(|err| space_error(err, parent.owner, me.user.id))?;
    Ok((
        StatusCode::CREATED,
        Json(state.view(me.user.id, copy, access).await?),
    ))
}

/// How many files **Recent** shows.
const RECENT: usize = 100;

/// The most results a search returns. Searching for more words narrows it.
const SEARCH_LIMIT: usize = 200;

/// `GET /recent`: the user's files outside the trash, and files in what
/// others shared with them, most recently changed first.
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
    // Items nobody shares with them any more drop out, but keep their
    // stars, in case they're shared again.
    state.found_views(me.user.id, found).await
}

#[derive(Deserialize)]
struct SearchQuery {
    /// Words in the name or the text.
    #[serde(default)]
    q: String,
    /// Words in the name only.
    #[serde(default)]
    name: String,
    /// Words in the text only.
    #[serde(default)]
    words: String,
    /// A kind of item: one of the names in [`TYPES`], or "folders".
    #[serde(rename = "type")]
    kind: Option<String>,
    /// `me`, `others`, or someone's username.
    owner: Option<String>,
    /// `my-drive`, `shared`, or a folder's ID, which includes the folders
    /// inside it.
    #[serde(rename = "in")]
    place: Option<String>,
    /// Only what the user starred.
    #[serde(default)]
    starred: bool,
    /// Only what's in the user's trash.
    #[serde(default)]
    trash: bool,
    /// Changed at or after this time, such as `2026-10-30T00:00:00.000Z`.
    /// The web app works out times like the start of today in the reader's
    /// own time zone, which the server doesn't know.
    after: Option<String>,
    /// Changed before this time.
    before: Option<String>,
    /// Only what the user shared with this username themselves.
    shared_with: Option<String>,
}

/// A search result.
#[derive(Serialize)]
struct HitView {
    #[serde(flatten)]
    found: FoundView,
    /// How it matched the words: `name` or `text`. Null without words.
    matched: Option<&'static str>,
    /// A line of text around where the words matched, in pieces, when they
    /// matched in the text.
    snippet: Option<Vec<PieceView>>,
}

#[derive(Serialize)]
struct PieceView {
    text: String,
    /// Whether this piece is a word that matched.
    matched: bool,
}

/// The kinds of file, each with the extensions its files end in. An
/// extension two programs share goes to the more common one (decision 26):
/// ".sln" is a Visual Studio solution, not telephone audio. The web app gets
/// this list from `GET /types`, so there's only the one.
pub(crate) const TYPES: &[(&str, &[&str])] = &[
    (
        "documents",
        &[
            "doc", "docx", "docm", "dot", "dotx", "dotm", "odt", "ott", "fodt", "rtf", "txt", "md",
            "markdown", "pages", "wpd", "wps", "tex", "rst",
        ],
    ),
    (
        "spreadsheets",
        &[
            "xls", "xlsx", "xlsm", "xlsb", "xlt", "xltx", "xltm", "ods", "ots", "fods", "csv",
            "tsv", "numbers",
        ],
    ),
    (
        "presentations",
        &[
            "ppt", "pptx", "pptm", "pps", "ppsx", "pot", "potx", "odp", "otp", "fodp", "key",
        ],
    ),
    ("pdfs", &["pdf"]),
    (
        "photos",
        &[
            "jpg", "jpeg", "jfif", "jpe", "png", "apng", "gif", "webp", "heic", "heif", "avif",
            "jxl", "bmp", "tif", "tiff", "svg", "svgz", "ico", "cur", "jp2", "j2k", "jpf", "jpx",
            "tga", "pcx", "pict", "pct", "pcd", "dds", "exr", "hdr", "sgi", "ras", "xwd", "wbmp",
            "pbm", "pgm", "ppm", "pnm", "pam", "xbm", "xpm", "picon", "mng", "qoi", "fits", "fts",
            // Camera raw
            "dng", "raw", "arw", "srf", "sr2", "cr2", "cr3", "crw", "nef", "nrw", "orf", "pef",
            "raf", "rw2", "rwl", "srw", "erf", "3fr", "iiq", "x3f", "mrw", "kdc", "dcr",
        ],
    ),
    (
        "videos",
        &[
            "mp4", "mov", "qt", "m4v", "mkv", "webm", "avi", "divx", "wmv", "asf", "3gp", "3g2",
            "mts", "m2ts", "m2t", "mpg", "mpeg", "mpe", "vob", "m2v", "flv", "f4v", "ogv", "ogm",
            "mxf", "rm", "rmvb", "wtv", "dvr-ms", "dv", "mjpeg", "mjpg", "hevc", "h264", "h265",
        ],
    ),
    (
        "audio",
        &[
            "mp3", "m4a", "m4b", "m4r", "aac", "wav", "flac", "ogg", "oga", "opus", "spx", "wma",
            "aiff", "aif", "aifc", "mp2", "ac3", "dts", "amr", "ape", "wv", "tta", "au", "snd",
            "caf", "mka", "ra", "w64", "mid", "midi", "kar", "mod", "xm", "s3m", "it", "dsf",
            "dff",
            // Older formats, from Amiga, Atari, classic Mac, Sound Blaster,
            // samplers, telephone, and speech research audio. `.paf`, `.ima`,
            // and `.wve` are mostly other programs' files, so they're audio
            // only when core finds sound in them.
            "8svx", "amb", "avr", "cdda", "cvs", "cvsd", "cvu", "dvms", "fap", "fssd", "gsrt",
            "hcom", "htk", "ircam", "maud", "nist", "pvf", "sd2", "smp", "sndr", "sndt", "sou",
            "sph", "txw", "vms", "voc", "vox",
        ],
    ),
    (
        "archives",
        &[
            "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "zst", "lz",
            "lzma", "lz4", "cab", "iso", "dmg",
        ],
    ),
    (
        "code",
        &[
            "js",
            "mjs",
            "cjs",
            "jsx",
            "ts",
            "cts",
            "tsx",
            "rs",
            "go",
            "c",
            "h",
            "cc",
            "cpp",
            "cxx",
            "hpp",
            "hh",
            "java",
            "kt",
            "kts",
            "scala",
            "groovy",
            "gradle",
            "cs",
            "fs",
            "fsx",
            "swift",
            "php",
            "py",
            "pyw",
            "ipynb",
            "rb",
            "sh",
            "bash",
            "zsh",
            "fish",
            "ps1",
            "psm1",
            "bat",
            "cmd",
            "sql",
            "pl",
            "pm",
            "vb",
            "vbs",
            "bas",
            "lua",
            "r",
            "dart",
            "ex",
            "exs",
            "erl",
            "hs",
            "ml",
            "clj",
            "elm",
            "jl",
            "nim",
            "zig",
            "asm",
            "html",
            "htm",
            "css",
            "scss",
            "sass",
            "less",
            "vue",
            "svelte",
            "json",
            "jsonc",
            "xml",
            "yml",
            "yaml",
            "toml",
            "ini",
            "conf",
            "cfg",
            "properties",
            "sln",
            "csproj",
            "vbproj",
            "fsproj",
            "vcxproj",
            "htaccess",
        ],
    ),
    (
        "fonts",
        &[
            "ttf", "otf", "ttc", "otc", "woff", "woff2", "eot", "fon", "fnt", "pfb", "pfm", "pfa",
            "afm", "bdf", "pcf",
        ],
    ),
    (
        "ebooks",
        &[
            "epub", "mobi", "azw", "azw3", "kf8", "kfx", "prc", "fb2", "lit", "lrf", "ibooks",
            "djvu", "djv", "cbz", "cbr", "cb7",
        ],
    ),
    (
        "designs",
        &[
            "psd",
            "psb",
            "ai",
            "xcf",
            "sketch",
            "fig",
            "xd",
            "indd",
            "idml",
            "afdesign",
            "afphoto",
            "afpub",
            "kra",
            "clip",
            "procreate",
            "cdr",
            "eps",
            "ora",
            // Embroidery patterns: Brother, Janome, Husqvarna Viking, and
            // Tajima.
            "pes",
            "pec",
            "jef",
            "vp3",
            "hus",
            "dst",
        ],
    ),
];

/// Whole names that are code, without an extension or with one that means
/// something else alone, such as `CMakeLists.txt`. Compared in lowercase.
const CODE_NAMES: &[&str] = &[
    "makefile",
    "gnumakefile",
    "dockerfile",
    "containerfile",
    "jenkinsfile",
    "vagrantfile",
    "gemfile",
    "rakefile",
    "procfile",
    "cmakelists.txt",
    ".htaccess",
    ".gitignore",
    ".gitattributes",
    ".gitmodules",
    ".dockerignore",
    ".editorconfig",
    ".env",
    ".npmrc",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".profile",
];

/// Extensions that two common kinds of file share, which a file's contents
/// tell apart: the kind for text, then the kind for anything with a
/// picture, such as a video's frame.
const SHARED_EXTENSIONS: &[(&str, &str, &str)] =
    &[("ts", "code", "videos"), ("mts", "code", "videos")];

/// Which of [`TYPES`] a file is, by its name. A name that only starts with a
/// dot, such as `.zip`, has no extension, though some such names are code,
/// such as `.bashrc`.
pub(crate) fn file_type(name: &str) -> Option<&'static str> {
    let lower = name.to_lowercase();
    if CODE_NAMES.contains(&lower.as_str()) {
        return Some("code");
    }
    let (stem, extension) = lower.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    TYPES
        .iter()
        .find(|(_, extensions)| extensions.contains(&extension))
        .map(|(kind, _)| *kind)
}

/// Which of [`TYPES`] a file is, by its name, unless its thumbnail shows
/// what it is better: a `.ts` file is TypeScript or a TV recording.
pub(crate) fn item_type(name: &str, thumbnail: Option<ThumbnailKind>) -> Option<&'static str> {
    let shared = name.rsplit_once('.').and_then(|(_, extension)| {
        SHARED_EXTENSIONS
            .iter()
            .find(|(known, _, _)| extension.eq_ignore_ascii_case(known))
    });
    match (shared, thumbnail) {
        (Some((_, text, _)), Some(ThumbnailKind::Text)) => Some(text),
        (Some((_, _, picture)), Some(ThumbnailKind::Picture)) => Some(picture),
        _ => file_type(name),
    }
}

/// The types and what they're made of, for the web app: each extension's
/// type, and whole names that are code.
#[derive(Serialize)]
struct TypesView {
    extensions: std::collections::BTreeMap<&'static str, &'static str>,
    names: std::collections::BTreeMap<&'static str, &'static str>,
}

/// `GET /types`: [`TYPES`], so the web app shows the same types as search.
async fn get_types(Authenticated(_): Authenticated) -> Json<TypesView> {
    Json(TypesView {
        extensions: TYPES
            .iter()
            .flat_map(|(kind, extensions)| {
                extensions.iter().map(move |extension| (*extension, *kind))
            })
            .collect(),
        names: CODE_NAMES.iter().map(|name| (*name, "code")).collect(),
    })
}

/// `GET /search`: the user's items outside the trash, and items in what
/// others shared with them, that match every filter that's set (see
/// [`SearchQuery`]). Items whose names have the words come first, newest
/// first, then items whose text has them, best first.
async fn search(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<HitView>>, ApiError> {
    let user = me.user.id;
    let only = match query.kind.as_deref() {
        None | Some("") => None,
        Some("folders") => Some(Only::Folders),
        Some("files") => Some(Only::Files),
        Some(kind) => {
            let (_, extensions) =
                TYPES
                    .iter()
                    .find(|(name, _)| *name == kind)
                    .ok_or_else(|| {
                        bad_search(format!(
                            "Accordion doesn't know the type \"{kind}\". Choose another type."
                        ))
                    })?;
            Some(Only::Extensions(
                extensions
                    .iter()
                    .map(|&extension| extension.to_owned())
                    .collect(),
            ))
        }
    };
    let owner = match query.owner.as_deref() {
        None | Some("") => None,
        Some("me") => Some(Owner::Me),
        Some("others") => Some(Owner::Others),
        Some(username) => Some(Owner::Person(state.person(username).await?)),
    };
    let place = match query.place.as_deref() {
        None | Some("") => None,
        Some("my-drive") => Some(Place::MyDrive),
        Some("shared") => Some(Place::Shared),
        Some(folder) => Some(Place::Folder(folder.parse().map_err(|_| {
            bad_search("Search in My Drive, in what's shared with you, or in a folder.")
        })?)),
    };
    let shared_with = match query.shared_with.as_deref() {
        None | Some("") => None,
        Some(username) => Some(state.person(username).await?),
    };
    let among = match query.starred {
        true => Some(state.stars.starred(user).await?.into_iter().collect()),
        false => None,
    };

    let either = Words::parse(&query.q);
    let name = Words::parse(&query.name);
    let words = Words::parse(&query.words);
    let text = match words.is_empty() {
        true => None,
        false => Some(state.texts.matching(&words).await?),
    };
    let name_or_text = match either.is_empty() {
        true => None,
        false => Some((either.clone(), state.texts.matching(&either).await?)),
    };
    let has_words = !(either.is_empty() && name.is_empty() && words.is_empty());
    let search = Search {
        name,
        text,
        name_or_text,
        only,
        owner,
        place,
        trash: query.trash,
        shared_with,
        among,
        modified_since: checked_time(query.after)?,
        modified_before: checked_time(query.before)?,
        limit: SEARCH_LIMIT,
    };
    let hits = state.storage.search(user, search).await?;

    // Lines of text for what matched in its text. Words only for the text
    // say more about why it's there.
    let shown = match words.is_empty() {
        true => &either,
        false => &words,
    };
    let in_text: Vec<_> = hits
        .iter()
        .filter(|hit| hit.text_rank.is_some())
        .filter_map(|hit| match &hit.found.item.kind {
            ItemKind::File { blob, .. } => Some(blob.hash),
            ItemKind::Folder => None,
        })
        .collect();
    let mut snippets = state.texts.snippets(shown, in_text).await?;
    let mut how: HashMap<ItemId, (Option<&'static str>, Option<Vec<PieceView>>)> = hits
        .iter()
        .map(|hit| {
            let matched = if hit.by_name {
                Some("name")
            } else if hit.text_rank.is_some() {
                Some("text")
            } else if has_words {
                Some("name")
            } else {
                None
            };
            let snippet = match &hit.found.item.kind {
                ItemKind::File { blob, .. } if hit.text_rank.is_some() => {
                    snippets.remove(&blob.hash).map(|pieces| {
                        pieces
                            .into_iter()
                            .map(|piece| PieceView {
                                text: piece.text,
                                matched: piece.matched,
                            })
                            .collect()
                    })
                }
                _ => None,
            };
            (hit.found.item.id, (matched, snippet))
        })
        .collect();
    let found = hits.into_iter().map(|hit| hit.found).collect();
    let views = state.found_list(user, found).await?;
    Ok(Json(
        views
            .into_iter()
            .map(|found| {
                let (matched, snippet) = how.remove(&found.item.id).unwrap_or_default();
                HitView {
                    found,
                    matched,
                    snippet,
                }
            })
            .collect(),
    ))
}

fn bad_search(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message)
}

impl DriveState {
    /// The ID of the person with `username`, for a search filter.
    async fn person(&self, username: &str) -> Result<UserId, ApiError> {
        let user = self.identity.user_by_username(username).await?;
        user.map(|user| user.id).ok_or_else(|| {
            bad_search(format!(
                "Accordion doesn't know anyone with the username \"{username}\"."
            ))
        })
    }
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
    let (item, _) = state.reach(me.user.id, id, Access::Viewer).await?;
    let ItemKind::File { blob, .. } = item.kind else {
        return Err(storage::Error::NotAFile.into());
    };
    file_response(&state, &request, &item.name, blob).await
}

/// `GET /items/{id}/preview`: the file as a type the browser shows itself,
/// for the kinds of file that `preview` allows, or a preview core has made
/// of it: an Office document's PDF of its first pages, a large picture, or
/// sound converted to AAC.
async fn preview_file(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    request: HeaderMap,
) -> Result<Response, ApiError> {
    let (item, _) = state.reach(me.user.id, id, Access::Viewer).await?;
    preview_response(&state, &request, &item).await
}

/// A file's preview, for the web app and for link pages: the file as a
/// type the browser shows itself, or an Office document's PDF preview.
pub(crate) async fn preview_response(
    state: &DriveState,
    request: &HeaderMap,
    item: &Item,
) -> Result<Response, ApiError> {
    let ItemKind::File { blob, .. } = item.kind else {
        return Err(storage::Error::NotAFile.into());
    };
    // Core converts only sound the browser can't play, even in a file
    // whose name says it can, such as Apple Lossless in an M4A.
    if let Some(sound) = state.thumbnails.sound(blob.hash) {
        return converted(request, item, blob.hash, &sound, "m4a", "audio/mp4").await;
    }
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
            if let Some(picture) = state.thumbnails.picture(blob.hash) {
                return converted(request, item, blob.hash, &picture, "webp", "image/webp").await;
            }
            let Some(document) = state.thumbnails.preview(blob.hash) else {
                return Err(ApiError::new(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "Accordion can't show this kind of file. Download it instead.",
                ));
            };
            // Previews of contents that are gone are removed now and then.
            let gone = || ApiError::new(StatusCode::NOT_FOUND, "This file doesn't have a preview.");
            let file = tokio::fs::File::open(&document.path)
                .await
                .map_err(|_| gone())?;
            let size = file.metadata().await.map_err(|_| gone())?.len();
            let stem = item
                .name
                .rsplit_once('.')
                .map_or(&*item.name, |(stem, _)| stem);
            let disposition = disposition("inline", &format!("{stem}.pdf"));
            // Both the contents and the preview's length name this version
            // of it, since a newer LibreOffice may make another.
            let etag = format!("\"{}-{size}\"", blob.hash);
            return send(
                request,
                file,
                size,
                &etag,
                "application/pdf",
                disposition,
                "frame-ancestors 'self'",
            )
            .await;
        }
    };
    let disposition = disposition("inline", &item.name);
    send_file(state, request, blob, content_type, disposition, policy).await
}

/// Sends a preview core made of a file, such as a large WebP of a HEIC,
/// named after the file with the preview's own extension.
async fn converted(
    request: &HeaderMap,
    item: &Item,
    hash: storage::ContentHash,
    path: &std::path::Path,
    extension: &str,
    content_type: &'static str,
) -> Result<Response, ApiError> {
    // Previews of contents that are gone are removed now and then.
    let gone = || ApiError::new(StatusCode::NOT_FOUND, "This file doesn't have a preview.");
    let file = tokio::fs::File::open(path).await.map_err(|_| gone())?;
    let size = file.metadata().await.map_err(|_| gone())?.len();
    let stem = item
        .name
        .rsplit_once('.')
        .map_or(&*item.name, |(stem, _)| stem);
    let disposition = disposition("inline", &format!("{stem}.{extension}"));
    // Both the contents and the preview's length name this version of it,
    // since a newer worker may make another.
    let etag = format!("\"{hash}-{size}\"");
    send(
        request,
        file,
        size,
        &etag,
        content_type,
        disposition,
        "sandbox; frame-ancestors 'self'",
    )
    .await
}

/// `GET /items/{id}/preview/{name}`: the same, at an address that ends in
/// the file's name, which a browser's PDF viewer shows as its title. The
/// name is only for show.
async fn preview_named(
    state: State<DriveState>,
    me: Authenticated,
    Path((id, _name)): Path<(ItemId, String)>,
    request: HeaderMap,
) -> Result<Response, ApiError> {
    preview_file(state, me, Path(id), request).await
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
    let (item, _) = state.reach(me.user.id, id, Access::Viewer).await?;
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
    // Items in the trash, or someone else's that nobody shared with them,
    // are as good as missing.
    let wanted = ids.len();
    let found = state.storage.live_items(ids).await?;
    let mut items: Vec<Item> = Vec::with_capacity(found.len());
    for found in found {
        if state
            .storage
            .access(me.user.id, found.item.id)
            .await?
            .is_some()
        {
            items.push(found.item);
        }
    }
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
/// Only editors and the owner see them, as in Google Drive.
async fn versions(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<Vec<VersionView>>, ApiError> {
    state.reach(me.user.id, id, Access::Editor).await?;
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
    let (item, _) = state.reach(me.user.id, id, Access::Editor).await?;
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
    let (item, access) = state.reach(me.user.id, id, Access::Editor).await?;
    let restored = state
        .by(&me)
        .restore_version(id, version)
        .await
        .map_err(|err| space_error(err, item.owner, me.user.id))?;
    Ok(Json(state.view(me.user.id, restored, access).await?))
}

/// `GET /trash`: what the user has deleted, newest first.
async fn trash(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<Vec<ItemView>>, ApiError> {
    let items = state.storage.trash(me.user.id).await?;
    Ok(Json(
        state.child_views(me.user.id, items, Access::Owner).await?,
    ))
}

/// `DELETE /trash`: deletes everything in the trash for good.
async fn empty_trash(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<StatusCode, ApiError> {
    state.by(&me).empty_trash(me.user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /trash/{id}`: deletes one item in the trash for good.
async fn delete_forever(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<StatusCode, ApiError> {
    state.reach(me.user.id, id, Access::Owner).await?;
    state.by(&me).delete_forever(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /usage`: how much space the user's files take, and their limit.
async fn usage(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<UsageView>, ApiError> {
    Ok(Json(state.storage.usage(me.user.id).await?.into()))
}

/// How long the server keeps trash and older versions, which the admin
/// chooses.
#[derive(Serialize)]
struct RetentionView {
    trash_days: u32,
    version_days: u32,
}

/// `GET /retention`: how long trash and older versions are kept, so the
/// Trash page and version history can say.
async fn retention(
    State(state): State<DriveState>,
    Authenticated(_): Authenticated,
) -> Result<Json<RetentionView>, ApiError> {
    let retention = state.storage.retention().await?;
    Ok(Json(RetentionView {
        trash_days: retention.trash_days,
        version_days: retention.version_days,
    }))
}

#[derive(Deserialize)]
struct NewUpload {
    size: u64,
    /// The folder it will finish in. It counts toward the quota of that
    /// folder's owner, and has to finish in their storage. Leave it out for
    /// the user's own storage.
    parent: Option<ItemId>,
}

/// `POST /uploads`: starts a resumable upload of `size` bytes. Send the
/// pieces with `PATCH /uploads/{id}`, then save the file with
/// `POST /uploads/{id}/finish`.
async fn start_upload(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Json(request): Json<NewUpload>,
) -> Result<(StatusCode, Json<UploadView>), ApiError> {
    let owner = match request.parent {
        Some(parent) => {
            state
                .reach(me.user.id, parent, Access::Editor)
                .await?
                .0
                .owner
        }
        None => me.user.id,
    };
    let upload = state
        .storage
        .start_upload_by(owner, me.user.id, request.size)
        .await
        .map_err(|err| space_error(err, owner, me.user.id))?;
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

/// Where a finished upload goes: `parent` and `name`, or `file` for a new
/// version of that file.
#[derive(Deserialize)]
struct FinishUpload {
    parent: Option<ItemId>,
    name: Option<String>,
    file: Option<ItemId>,
}

/// `POST /uploads/{id}/finish`: saves a finished upload as the file `name`
/// in `parent`, or as a new version if a file with that name is already
/// there. With `If-None-Match: *`, the name must be free instead. To save
/// over one file only while it holds the contents a device last saw, send
/// `file` and that SHA-256 hash in `If-Match`, as `PUT /items/{id}/content`
/// takes.
async fn finish_upload(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<UploadId>,
    headers: HeaderMap,
    Json(request): Json<FinishUpload>,
) -> ItemResult {
    owned_upload(&state, me.user.id, id).await?;
    let storage = state.by(&me);
    let (owner, access, file) = match (request.file, request.parent, request.name) {
        (Some(file), None, None) => {
            let (item, access) = state.reach(me.user.id, file, Access::Editor).await?;
            let base = replaced(&headers)?;
            (
                item.owner,
                access,
                storage.finish_upload_over(id, file, base).await,
            )
        }
        (None, Some(parent), Some(name)) => {
            let (folder, access) = state.reach(me.user.id, parent, Access::Editor).await?;
            let saved = if new_only(&headers) {
                storage.finish_new_upload(id, parent, &name).await
            } else {
                storage.finish_upload(id, parent, &name).await
            };
            (folder.owner, access, saved)
        }
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Say where the upload goes: a `parent` folder and a `name`, or the `file` it's \
                 a new version of.",
            ));
        }
    };
    let file = file.map_err(|err| space_error(err, owner, me.user.id))?;
    Ok(Json(state.view(me.user.id, file, access).await?))
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
        Ok(upload) if upload.started_by == user => Ok(upload),
        Ok(_) | Err(storage::Error::UploadNotFound) => Err(ApiError::not_found()),
        Err(err) => Err(err.into()),
    }
}

/// Reads a request body as it arrives, without holding it all in memory.
fn reader(body: Body) -> impl AsyncRead + Unpin + Send {
    StreamReader::new(body.into_data_stream().map_err(io::Error::other))
}

/// A kind of file the browser can show itself.
pub(crate) enum Preview {
    /// With its type, such as "image/png".
    Image(&'static str),
    Pdf,
    /// With its type, such as "video/mp4".
    Video(&'static str),
    /// With its type, such as "audio/mpeg".
    Audio(&'static str),
}

/// Whether a file of this type can show as text: a document, a
/// spreadsheet, code, or a type Accordion doesn't know.
fn text_kind(file_type: Option<&str>) -> bool {
    matches!(
        file_type,
        None | Some("documents" | "spreadsheets" | "code")
    )
}

/// Whether the browser can show a file, going by its name's extension.
///
/// The file is sent as exactly this type, and the browser is told not to
/// guess another, so a file that isn't what its name says fails to show
/// rather than running as something else. SVG images are left out because
/// they can hold scripts, and HEIC photos because most browsers can't
/// show them.
pub(crate) fn preview(name: &str) -> Option<Preview> {
    let (_, extension) = name.rsplit_once('.')?;
    let image = match extension.to_ascii_lowercase().as_str() {
        // A stereo JPEG shows as its two pictures side by side.
        "jpg" | "jpeg" | "jfif" | "jpe" | "jps" => "image/jpeg",
        // Browsers read cursors with the same decoder as icons.
        "ico" | "cur" => "image/x-icon",
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
        // Audiobooks and iPhone ringtones are M4A files by another name.
        "m4a" | "m4b" | "m4r" => return Some(Preview::Audio("audio/mp4")),
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
pub(crate) async fn file_response(
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
    let file = state.storage.open_contents(blob.hash).await?;
    send(
        request,
        file,
        blob.size,
        &etag,
        content_type,
        disposition,
        policy,
    )
    .await
}

/// Sends an open file of `size` bytes, or the part a `Range` header asks
/// for.
async fn send(
    request: &HeaderMap,
    mut file: tokio::fs::File,
    size: u64,
    etag: &str,
    content_type: &'static str,
    disposition: HeaderValue,
    policy: &'static str,
) -> Result<Response, ApiError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CONTENT_DISPOSITION, disposition);
    headers.insert(
        ETAG,
        HeaderValue::try_from(etag).expect("hex is valid header text"),
    );
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(policy));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    let range = match requested_range(request, size, etag) {
        Ok(range) => range,
        Err(Unsatisfiable) => {
            headers.insert(
                CONTENT_RANGE,
                HeaderValue::try_from(format!("bytes */{size}")).expect("digits are valid"),
            );
            return Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response());
        }
    };
    let Some(range) = range else {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(size));
        return Ok((headers, Body::from_stream(ReaderStream::new(file))).into_response());
    };
    file.seek(io::SeekFrom::Start(range.start))
        .await
        .map_err(storage::Error::from)?;
    headers.insert(CONTENT_LENGTH, HeaderValue::from(range.end - range.start));
    headers.insert(
        CONTENT_RANGE,
        HeaderValue::try_from(format!("bytes {}-{}/{size}", range.start, range.end - 1))
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
pub(crate) fn disposition(kind: &str, name: &str) -> HeaderValue {
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

    #[test]
    fn gives_each_extension_one_type() {
        let mut seen = std::collections::HashMap::new();
        for (kind, extensions) in TYPES {
            for extension in *extensions {
                assert_eq!(
                    extension.to_lowercase(),
                    *extension,
                    "{extension} isn't lowercase"
                );
                if let Some(other) = seen.insert(*extension, *kind) {
                    panic!("{extension} is in both {other} and {kind}");
                }
            }
        }
    }

    #[test]
    fn knows_files_by_their_most_common_use() {
        assert_eq!(file_type("Budget.XLSX"), Some("spreadsheets"));
        assert_eq!(file_type("App.sln"), Some("code"));
        assert_eq!(file_type("Helvetica.pfm"), Some("fonts"));
        assert_eq!(file_type("Novel.prc"), Some("ebooks"));
        assert_eq!(file_type("Poster.psd"), Some("designs"));
        assert_eq!(file_type("Rose.PES"), Some("designs"));
        assert_eq!(file_type("Makefile"), Some("code"));
        assert_eq!(file_type("CMakeLists.txt"), Some("code"));
        assert_eq!(file_type(".bashrc"), Some("code"));
        assert_eq!(file_type("notes.txt"), Some("documents"));
        assert_eq!(file_type(".zip"), None);
        assert_eq!(file_type("tree.paf"), None);
    }

    #[test]
    fn tells_shared_extensions_apart_by_their_contents() {
        assert_eq!(item_type("main.ts", None), Some("code"));
        assert_eq!(
            item_type("main.ts", Some(ThumbnailKind::Text)),
            Some("code")
        );
        assert_eq!(
            item_type("Match.TS", Some(ThumbnailKind::Picture)),
            Some("videos")
        );
        assert_eq!(item_type("clip.mts", None), Some("videos"));
        assert_eq!(
            item_type("module.mts", Some(ThumbnailKind::Text)),
            Some("code")
        );
        // Only shared extensions: a photo's type doesn't follow its thumbnail.
        assert_eq!(
            item_type("scan.png", Some(ThumbnailKind::Text)),
            Some("photos")
        );
        assert!(text_kind(Some("code")));
        assert!(!text_kind(Some("videos")));
    }

    #[test]
    fn previews_jpegs_and_m4a_files_by_any_of_their_names() {
        assert!(matches!(
            preview("scan.JFIF"),
            Some(Preview::Image("image/jpeg"))
        ));
        assert!(matches!(
            preview("scan.jpe"),
            Some(Preview::Image("image/jpeg"))
        ));
        assert!(matches!(
            preview("Ringtone.m4r"),
            Some(Preview::Audio("audio/mp4"))
        ));
        assert!(matches!(
            preview("Book.m4b"),
            Some(Preview::Audio("audio/mp4"))
        ));
        assert!(matches!(
            preview("pair.JPS"),
            Some(Preview::Image("image/jpeg"))
        ));
        assert!(matches!(
            preview("favicon.ico"),
            Some(Preview::Image("image/x-icon"))
        ));
        assert!(preview("drawing.svg").is_none());
    }
}
