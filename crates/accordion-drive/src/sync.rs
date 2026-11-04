//! Sync's addresses (decision 23 in the `accordion` repo's
//! `docs/decisions`): where a device starts, what's inside a folder, and the
//! list of changes. Devices download and save files through the usual
//! addresses, such as `PUT /items/{id}/content` with `If-Match`.
//!
//! A new device asks `/sync/start` for a cursor and the folders to list,
//! lists each with `/sync/folders/{id}`, then follows `/sync/changes` from
//! the cursor. A `drive.items` or `drive.shared` live update is a good time
//! to ask for changes again. A 410 reply means the server can't follow the
//! cursor any more, so the device starts over and compares what it has.

use accordion_core::storage::{self, Access, ItemKind, SyncChange, SyncItem};
use accordion_module::{ApiError, Authenticated};
use accordion_types::ItemId;
use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use crate::DriveState;
use crate::api::access_name;

/// An item as the person sees it, with what a device needs to sync it.
#[derive(Serialize)]
pub(crate) struct SyncItemView {
    id: ItemId,
    /// The folder it's in, if the person can open it. Null for their top
    /// folder, and for an item shared with them whose folder isn't, which
    /// goes in **Shared with me**.
    parent: Option<ItemId>,
    name: String,
    kind: &'static str,
    /// For files.
    size: Option<u64>,
    sha256: Option<String>,
    created_at: String,
    modified_at: String,
    /// "owner", "editor", or "viewer". A device keeps what a viewer has
    /// read-only.
    access: &'static str,
    /// Everything inside changed for the person too, so the device lists
    /// it again with `/sync/folders/{id}`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    whole: bool,
}

impl From<SyncItem> for SyncItemView {
    fn from(entry: SyncItem) -> Self {
        let item = entry.item;
        let (kind, blob) = match item.kind {
            ItemKind::Folder => ("folder", None),
            ItemKind::File { blob, .. } => ("file", Some(blob)),
        };
        Self {
            id: item.id,
            parent: entry.parent,
            name: item.name,
            kind,
            size: blob.map(|blob| blob.size),
            sha256: blob.map(|blob| blob.hash.to_hex()),
            created_at: item.created_at,
            modified_at: item.modified_at,
            access: access_name(entry.access),
            whole: entry.whole,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct StartView {
    /// Follow `/sync/changes` from here once everything is listed.
    cursor: String,
    /// The person's top folder, then what's shared with them.
    roots: Vec<SyncItemView>,
}

/// `GET /sync/start`: where a new device starts, or one told to start
/// over.
pub(crate) async fn start(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<StartView>, ApiError> {
    let start = state.storage.sync_start(me.user.id).await?;
    Ok(Json(StartView {
        cursor: start.cursor,
        roots: start.roots.into_iter().map(Into::into).collect(),
    }))
}

#[derive(Deserialize)]
pub(crate) struct After {
    after: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct FolderView {
    /// Everything inside, at any depth, outside the trash.
    items: Vec<SyncItemView>,
    /// Pass this as `after` for the next page, if there is one.
    next: Option<ItemId>,
}

/// `GET /sync/folders/{id}?after=<id>`: a page of everything inside a
/// folder, by ID.
pub(crate) async fn folder(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Query(query): Query<After>,
) -> Result<Json<FolderView>, ApiError> {
    state.reach(me.user.id, id, Access::Viewer).await?;
    let after = match query.after {
        Some(after) => Some(after.parse().map_err(|_| {
            ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "Pass the `next` value from the last page as `after`.",
            )
        })?),
        None => None,
    };
    let page = match state.storage.sync_list(me.user.id, id, after).await {
        Ok(page) => page,
        // In the trash, or out of reach since `reach` asked.
        Err(storage::Error::NotFound) => return Err(ApiError::not_found()),
        Err(err) => return Err(err.into()),
    };
    Ok(Json(FolderView {
        items: page.items.into_iter().map(Into::into).collect(),
        next: page.next,
    }))
}

#[derive(Serialize)]
#[serde(untagged)]
enum ChangeView {
    Changed(SyncItemView),
    /// Out of the person's reach now, with everything inside it.
    Gone {
        id: ItemId,
        gone: bool,
    },
}

#[derive(Serialize)]
pub(crate) struct ChangesView {
    changes: Vec<ChangeView>,
    cursor: String,
    /// Ask again with `cursor` right away for the rest.
    more: bool,
}

/// `GET /sync/changes?after=<cursor>`: what changed since the cursor, each
/// item once, as it is now. 410 means start over with `/sync/start`.
pub(crate) async fn changes(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Query(query): Query<After>,
) -> Result<Json<ChangesView>, ApiError> {
    let cursor = query.after.unwrap_or_default();
    let page = state.storage.changes(me.user.id, &cursor).await?;
    Ok(Json(ChangesView {
        changes: page
            .changes
            .into_iter()
            .map(|change| match change {
                SyncChange::Changed(entry) => ChangeView::Changed((*entry).into()),
                SyncChange::Gone(id) => ChangeView::Gone { id, gone: true },
            })
            .collect(),
        cursor: page.cursor,
        more: page.more,
    }))
}
