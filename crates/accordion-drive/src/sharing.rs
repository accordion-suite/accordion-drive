//! Sharing in `/api/drive`: who an item is shared with, sharing it with
//! someone or changing their access, **Shared with me**, and **Shared by
//! me**. Core keeps the
//! shares and decides what each person may do (decision 16). Only the owner
//! shares, at first.

use accordion_core::storage::{Access, Share, ShareLevel};
use accordion_module::{ApiError, Authenticated};
use accordion_types::{ItemId, UserId};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::DriveState;
use crate::api::{FoundView, ItemView};

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Level {
    Viewer,
    Editor,
}

impl From<Level> for ShareLevel {
    fn from(level: Level) -> Self {
        match level {
            Level::Viewer => Self::Viewer,
            Level::Editor => Self::Editor,
        }
    }
}

impl From<ShareLevel> for Level {
    fn from(level: ShareLevel) -> Self {
        match level {
            ShareLevel::Viewer => Self::Viewer,
            ShareLevel::Editor => Self::Editor,
        }
    }
}

/// Someone an item is shared with.
#[derive(Serialize)]
pub(crate) struct ShareView {
    user: UserId,
    username: String,
    display_name: String,
    level: Level,
    created_at: String,
}

impl DriveState {
    async fn share_view(&self, share: Share) -> Result<Option<ShareView>, ApiError> {
        // Accounts are deleted with their shares, so a missing one is a
        // deletion in progress.
        let Some(user) = self.identity.user(share.user).await? else {
            return Ok(None);
        };
        Ok(Some(ShareView {
            user: user.id,
            username: user.username,
            display_name: user.display_name,
            level: share.level.into(),
            created_at: share.created_at,
        }))
    }
}

/// `GET /items/{id}/shares`: who the owner shared an item with, in the
/// order they were added. Shares on the folders above it aren't included.
pub(crate) async fn shares(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<Vec<ShareView>>, ApiError> {
    state.reach(me.user.id, id, Access::Owner).await?;
    let mut views = Vec::new();
    for share in state.storage.shares(id).await? {
        views.extend(state.share_view(share).await?);
    }
    Ok(Json(views))
}

#[derive(Deserialize)]
pub(crate) struct ShareRequest {
    level: Level,
}

/// `PUT /items/{id}/shares/{user}`: shares an item with someone, or changes
/// what they may do with it.
pub(crate) async fn share(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path((id, user)): Path<(ItemId, UserId)>,
    Json(request): Json<ShareRequest>,
) -> Result<Json<ShareView>, ApiError> {
    state.reach(me.user.id, id, Access::Owner).await?;
    let share = state
        .storage
        .share(id, user, request.level.into(), me.user.id)
        .await?;
    tracing::info!(user = %me.user.id, item = %id, with = %user, level = share.level.as_str(), "shared an item");
    let view = state
        .share_view(share)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(view))
}

/// `DELETE /items/{id}/shares/{user}`: stops sharing an item with someone.
/// It works on their next request, in the web app and over WebDAV.
pub(crate) async fn unshare(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path((id, user)): Path<(ItemId, UserId)>,
) -> Result<StatusCode, ApiError> {
    state.reach(me.user.id, id, Access::Owner).await?;
    state.storage.unshare(id, user, me.user.id).await?;
    tracing::info!(user = %me.user.id, item = %id, with = %user, "stopped sharing an item");
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /items/{id}/shares`: stops sharing an item with everyone it's
/// shared with directly. Shares on folders above it stay.
pub(crate) async fn unshare_all(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<StatusCode, ApiError> {
    state.reach(me.user.id, id, Access::Owner).await?;
    let removed = state.storage.unshare_all(id, me.user.id).await?;
    tracing::info!(user = %me.user.id, item = %id, people = removed.len(), "stopped sharing an item with everyone");
    Ok(StatusCode::NO_CONTENT)
}

/// Something the signed-in user shared, and who with.
#[derive(Serialize)]
pub(crate) struct SharedOutView {
    #[serde(flatten)]
    found: FoundView,
    shared_with: Vec<ShareView>,
    /// When it was last shared with someone new.
    shared_at: String,
}

/// `GET /shared-by-me`: what the signed-in user shared directly, outside
/// the trash, most recently shared first, as Proton Drive's **Shared**
/// lists it.
pub(crate) async fn shared_by_me(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<Vec<SharedOutView>>, ApiError> {
    let shared = state.storage.shared_by(me.user.id).await?;
    let mut people = Vec::with_capacity(shared.len());
    let mut found = Vec::with_capacity(shared.len());
    for out in shared {
        let shared_at = out
            .shares
            .iter()
            .map(|share| share.created_at.clone())
            .max()
            .unwrap_or_default();
        let mut with = Vec::with_capacity(out.shares.len());
        for share in out.shares {
            with.extend(state.share_view(share).await?);
        }
        people.push((with, shared_at));
        found.push(out.found);
    }
    let Json(views) = state.found_views(me.user.id, found).await?;
    // The user owns every item, so none are dropped and the order holds.
    let views = views
        .into_iter()
        .zip(people)
        .map(|(found, (shared_with, shared_at))| SharedOutView {
            found,
            shared_with,
            shared_at,
        })
        .collect();
    Ok(Json(views))
}

/// Something someone else shared with the signed-in user.
#[derive(Serialize)]
pub(crate) struct SharedView {
    #[serde(flatten)]
    item: ItemView,
    /// Always empty: a shared item is where its own path starts. It's here
    /// so the list has the same shape as search results.
    folders: Vec<String>,
    shared_at: String,
}

/// `GET /shared`: what others shared with the signed-in user, outside the
/// trash, most recently shared first.
pub(crate) async fn shared_with_me(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<Vec<SharedView>>, ApiError> {
    let shared = state.storage.shared_with(me.user.id).await?;
    let owners: Vec<UserId> = shared.iter().map(|shared| shared.item.owner).collect();
    let reader = state.reader(me.user.id, owners).await?;
    let mut views = Vec::with_capacity(shared.len());
    for shared in shared {
        // A share on a folder above it can give more than this one.
        let access = state
            .storage
            .access(me.user.id, shared.item.id)
            .await?
            .unwrap_or(shared.share.level.into());
        views.push(SharedView {
            item: ItemView::new(shared.item, access, &reader, &state.thumbnails),
            folders: Vec::new(),
            shared_at: shared.share.created_at,
        });
    }
    state
        .size_folders(views.iter_mut().map(|view| &mut view.item))
        .await?;
    Ok(Json(views))
}
