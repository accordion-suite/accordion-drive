//! Apps' places beside My Drive, which Drive shows as tabs, and who a move
//! would let open an item (decision 32 in the `accordion` repo's
//! `docs/decisions`).

use std::collections::HashSet;

use accordion_core::storage::{self, Access, Reach};
use accordion_module::{ApiError, Authenticated};
use accordion_types::{ItemId, UserId};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::DriveState;
use crate::api::ItemView;

/// The code a move answers with when it would let someone new open the
/// item.
pub(crate) const WOULD_SHARE: &str = "would_share";

/// One of the person's places, for a tab in Drive.
#[derive(Serialize)]
pub(crate) struct PlaceView {
    /// Such as "notes". The web app shows the tab while the module with
    /// the same name is on, or while the place holds anything.
    place: String,
    /// Such as "Notes".
    name: String,
    /// Its top folder, once it exists.
    id: Option<ItemId>,
    /// Whether anything is in it, outside the trash.
    empty: bool,
}

/// `GET /places`: the person's places apart from My Drive: every app's
/// that keeps files, and any they have files in from an app that's gone.
/// Asking doesn't make any.
pub(crate) async fn places(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<Vec<PlaceView>>, ApiError> {
    let own = state.storage.own_places(me.user.id).await?;
    let mut views = Vec::new();
    for app in state.storage.places() {
        let top = own.iter().find(|top| top.place.as_deref() == Some(app.id));
        let empty = match top {
            Some(top) => state.storage.children(top.id).await?.is_empty(),
            None => true,
        };
        views.push(PlaceView {
            place: app.id.to_owned(),
            name: app.name.to_owned(),
            id: top.map(|top| top.id),
            empty,
        });
    }
    for top in own {
        let Some(place) = top.place else { continue };
        if place == storage::MY_DRIVE || views.iter().any(|view| view.place == place) {
            continue;
        }
        let empty = state.storage.children(top.id).await?.is_empty();
        views.push(PlaceView {
            place,
            name: top.name,
            id: Some(top.id),
            empty,
        });
    }
    Ok(Json(views))
}

/// `GET /places/{place}`: the top folder of one of the person's places,
/// made the first time it's asked for if its app keeps files.
pub(crate) async fn place(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(place): Path<String>,
) -> Result<Json<ItemView>, ApiError> {
    let id = match state
        .storage
        .own_places(me.user.id)
        .await?
        .into_iter()
        .find(|top| top.place.as_deref() == Some(place.as_str()))
    {
        Some(top) => top.id,
        None if place == storage::MY_DRIVE => state.storage.top_folder(me.user.id).await?,
        None => state.storage.place_folder(me.user.id, &place).await?,
    };
    let (item, access) = state.reach(me.user.id, id, Access::Owner).await?;
    Ok(Json(state.view(me.user.id, item, access).await?))
}

/// Someone a move would let in, or leave out.
#[derive(Clone, Serialize)]
pub(crate) struct PersonView {
    user: UserId,
    display_name: String,
}

/// Who a move would let open an item, beyond who can now.
#[derive(Serialize)]
pub(crate) struct WideningView {
    /// People who'd be able to open it.
    people: Vec<PersonView>,
    /// How many links that work would reach it.
    links: usize,
}

#[derive(Deserialize)]
pub(crate) struct MoveCheck {
    items: Vec<ItemId>,
    /// The folder they'd move into.
    into: ItemId,
}

/// `POST /move-check`: who moving `items` into a folder would let open
/// them, so the web app can ask first. Moves then name these people in
/// `allow`.
pub(crate) async fn move_check(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Json(check): Json<MoveCheck>,
) -> Result<Json<WideningView>, ApiError> {
    state.reach(me.user.id, check.into, Access::Editor).await?;
    let mut people = HashSet::new();
    let mut links = HashSet::new();
    for id in check.items {
        state.reach(me.user.id, id, Access::Editor).await?;
        let reach = state.storage.move_reach(id, check.into).await?;
        people.extend(reach.gains);
        links.extend(state.links_gained(id, check.into).await?);
    }
    Ok(Json(WideningView {
        people: state.people(people).await?,
        links: links.len(),
    }))
}

/// What a move that names who it allows checks.
pub(crate) struct Allowed {
    pub(crate) people: Vec<UserId>,
    pub(crate) links: bool,
}

impl DriveState {
    /// Links that work and would reach `id` in `folder`, but don't reach
    /// it now.
    async fn links_gained(&self, id: ItemId, folder: ItemId) -> Result<Vec<ItemId>, ApiError> {
        let mut gained = Vec::new();
        for linked in self.links.linked_items().await? {
            if self.storage.path_within(linked, folder).await?.is_some()
                && self.storage.path_within(linked, id).await?.is_none()
            {
                gained.push(linked);
            }
        }
        Ok(gained)
    }

    async fn people(
        &self,
        users: impl IntoIterator<Item = UserId>,
    ) -> Result<Vec<PersonView>, ApiError> {
        let mut people = Vec::new();
        for user in users {
            // An account being deleted has no one to tell.
            if let Some(user) = self.identity.user(user).await? {
                people.push(PersonView {
                    user: user.id,
                    display_name: user.display_name,
                });
            }
        }
        people.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        Ok(people)
    }

    /// Checks that moving `id` into `folder` lets in only who `allowed`
    /// names, and says who'd lose access. A refusal is a 409 with
    /// [`WOULD_SHARE`] and everyone it would let in, so the app can ask
    /// again.
    pub(crate) async fn check_move(
        &self,
        id: ItemId,
        folder: ItemId,
        allowed: Option<Allowed>,
    ) -> Result<Result<Vec<PersonView>, Response>, ApiError> {
        let Reach { gains, loses } = self.storage.move_reach(id, folder).await?;
        if let Some(allowed) = allowed {
            let links = self.links_gained(id, folder).await?.len();
            let people_ok = gains.iter().all(|user| allowed.people.contains(user));
            if !people_ok || (links > 0 && !allowed.links) {
                let widening = WideningView {
                    people: self.people(gains).await?,
                    links,
                };
                return Ok(Err(would_share(widening)));
            }
        }
        Ok(Ok(self.people(loses).await?))
    }
}

fn would_share(widening: WideningView) -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "Moving this would let more people open it. Check who, then move it again.",
            "code": WOULD_SHARE,
            "people": widening.people,
            "links": widening.links,
        })),
    )
        .into_response()
}
