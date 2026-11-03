//! Activity and notifications in `/api/drive`: what happened to an item and
//! what's inside it, and what the signed-in person should hear about. Core
//! keeps the event log and decides who sees which events (decision 18).
//!
//! Events name items and folders by ID. This fills in their names as they
//! are now, and only for folders the reader can open, so a rename shows the
//! new name and nobody learns a folder's name through its activity.

use std::collections::HashMap;

use accordion_core::events::{Action, Event, Notice};
use accordion_core::storage::{Access, ItemKind};
use accordion_module::{ApiError, Authenticated};
use accordion_types::{ItemId, UserId};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::DriveState;

/// Events on one page of an item's activity.
const PAGE: usize = 50;

/// Notifications the bell shows, before grouping.
const NOTIFICATIONS: usize = 100;

#[derive(Deserialize)]
pub(crate) struct PageQuery {
    /// The lowest event ID on the previous page.
    before: Option<i64>,
}

#[derive(Serialize)]
pub(crate) struct ActivityPage {
    events: Vec<EventView>,
    /// Whether there are older events to ask for.
    more: bool,
}

/// One event, with names filled in.
#[derive(Serialize)]
struct EventView {
    id: i64,
    at: String,
    action: &'static str,
    /// `None` for Accordion itself.
    actor: Option<PersonView>,
    via: &'static str,
    /// The item, if it still exists and the reader can open it.
    item: Option<ItemRef>,
    other: Option<PersonView>,
    /// Depends on the action. Folders in a move are `null` when the reader
    /// can't open them.
    details: Value,
}

#[derive(Clone, Serialize)]
struct PersonView {
    id: UserId,
    /// `None` for an account that was deleted.
    name: Option<String>,
}

#[derive(Clone, Serialize)]
struct ItemRef {
    id: ItemId,
    /// Empty for someone's top folder, which the web app calls My Drive.
    name: String,
    folder: bool,
}

/// Names looked up once per request.
struct Names<'a> {
    state: &'a DriveState,
    reader: UserId,
    people: HashMap<UserId, PersonView>,
    items: HashMap<ItemId, Option<ItemRef>>,
}

impl<'a> Names<'a> {
    fn new(state: &'a DriveState, reader: UserId) -> Self {
        Self {
            state,
            reader,
            people: HashMap::new(),
            items: HashMap::new(),
        }
    }

    async fn person(&mut self, id: Option<UserId>) -> Result<Option<PersonView>, ApiError> {
        let Some(id) = id else {
            return Ok(None);
        };
        if let Some(person) = self.people.get(&id) {
            return Ok(Some(person.clone()));
        }
        let name = self
            .state
            .identity
            .user(id)
            .await?
            .map(|user| user.display_name);
        let person = PersonView { id, name };
        self.people.insert(id, person.clone());
        Ok(Some(person))
    }

    /// An item, if the reader can open it.
    async fn item(&mut self, id: Option<ItemId>) -> Result<Option<ItemRef>, ApiError> {
        let Some(id) = id else {
            return Ok(None);
        };
        if let Some(item) = self.items.get(&id) {
            return Ok(item.clone());
        }
        let item = match self.state.storage.access(self.reader, id).await? {
            Some(_) => self.state.storage.item(id).await?.map(|item| ItemRef {
                id: item.id,
                folder: item.kind == ItemKind::Folder,
                name: if item.parent.is_none() {
                    String::new()
                } else {
                    item.name
                },
            }),
            None => None,
        };
        self.items.insert(id, item.clone());
        Ok(item)
    }

    async fn event(&mut self, event: Event) -> Result<EventView, ApiError> {
        let mut details = event.details;
        if event.action == Action::Moved {
            for end in ["from", "to"] {
                let folder = details
                    .get(end)
                    .and_then(|id| serde_json::from_value::<ItemId>(id.clone()).ok());
                details[end] = json!(self.item(folder).await?);
            }
        }
        Ok(EventView {
            id: event.id,
            at: event.at,
            action: event.action.as_str(),
            actor: self.person(event.actor).await?,
            via: event.via.as_str(),
            item: self.item(event.item).await?,
            other: self.person(event.other).await?,
            details,
        })
    }
}

/// `GET /items/{id}/activity`: what happened to an item, and for a folder
/// to everything inside it, newest first, a page at a time.
pub(crate) async fn activity(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Query(query): Query<PageQuery>,
) -> Result<Json<ActivityPage>, ApiError> {
    state.reach(me.user.id, id, Access::Viewer).await?;
    let events = state
        .events
        .activity(me.user.id, id, query.before, PAGE)
        .await?;
    let more = events.len() == PAGE;
    let mut names = Names::new(&state, me.user.id);
    let mut views = Vec::with_capacity(events.len());
    for event in events {
        views.push(names.event(event).await?);
    }
    Ok(Json(ActivityPage {
        events: views,
        more,
    }))
}

#[derive(Serialize)]
pub(crate) struct NotificationsView {
    /// How many groups have something the person hasn't seen, for the
    /// count on the bell.
    new: usize,
    notifications: Vec<NotificationView>,
}

/// One or more notifications about the same thing, such as "Sam Rivera
/// added 12 files to Holiday".
#[derive(Serialize)]
struct NotificationView {
    /// The newest event's ID.
    id: i64,
    /// Every event in the group, for reading or removing it whole.
    ids: Vec<i64>,
    at: String,
    action: &'static str,
    actor: Option<PersonView>,
    /// Whether the person opened the list since it arrived.
    seen: bool,
    /// Whether they selected it.
    read: bool,
    /// The first item, with how many there are in all.
    item: Option<ItemRef>,
    count: usize,
    /// The folder the items are in, for a group, if the reader can open it.
    folder: Option<ItemRef>,
    details: Value,
}

/// `GET /notifications`: what the signed-in person should hear about,
/// newest first. Changes to files by the same person, in the same folder,
/// on the same day, are grouped.
pub(crate) async fn notifications(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<NotificationsView>, ApiError> {
    let notices = state
        .events
        .notifications(me.user.id, NOTIFICATIONS)
        .await?;
    let mut names = Names::new(&state, me.user.id);
    let mut views: Vec<NotificationView> = Vec::new();
    // Each group's key: who, what, which folder, and which day.
    let mut last_key = None;
    for Notice { event, seen, read } in notices {
        let folder = match event.item {
            Some(item)
                if event.action != Action::Shared && event.action != Action::ShareChanged =>
            {
                state.storage.item(item).await?.and_then(|item| item.parent)
            }
            _ => None,
        };
        let key = (
            event.actor,
            event.action,
            folder,
            event.at.get(..10).map(str::to_owned),
        );
        if folder.is_some()
            && last_key.as_ref() == Some(&key)
            && let Some(group) = views.last_mut()
        {
            group.count += 1;
            group.ids.push(event.id);
            group.seen &= seen;
            group.read &= read;
            // The oldest is shown, so the list reads in the order things
            // happened.
            if let Some(item) = names.item(event.item).await? {
                group.item = Some(item);
            }
            continue;
        }
        last_key = Some(key);
        let folder = names.item(folder).await?;
        let view = names.event(event).await?;
        views.push(NotificationView {
            id: view.id,
            ids: vec![view.id],
            at: view.at,
            action: view.action,
            actor: view.actor,
            seen,
            read,
            item: view.item,
            count: 1,
            folder,
            details: view.details,
        });
    }
    Ok(Json(NotificationsView {
        new: views.iter().filter(|view| !view.seen).count(),
        notifications: views,
    }))
}

#[derive(Deserialize)]
pub(crate) struct UpTo {
    /// The newest notification the person was shown.
    up_to: i64,
}

#[derive(Deserialize)]
pub(crate) struct Ids {
    /// The events of one or more notifications, as `ids` lists them.
    ids: Vec<i64>,
}

/// The most events one request can name, more than the list ever shows.
const MAX_IDS: usize = 1_000;

fn check_ids(ids: &Ids) -> Result<(), ApiError> {
    if ids.ids.len() > MAX_IDS {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "That's too many notifications at once. Try again with fewer.",
        ));
    }
    Ok(())
}

/// `POST /notifications/seen`: marks notifications seen, up to the newest
/// the person was shown, which clears the count on the bell. They stay
/// highlighted until they're read.
pub(crate) async fn mark_seen(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Json(request): Json<UpTo>,
) -> Result<StatusCode, ApiError> {
    state.events.mark_seen(me.user.id, request.up_to).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /notifications/read`: marks notifications read, such as one the
/// person selected.
pub(crate) async fn mark_read(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Json(request): Json<Ids>,
) -> Result<StatusCode, ApiError> {
    check_ids(&request)?;
    state.events.mark_read(me.user.id, request.ids).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /notifications/remove`: removes notifications for the signed-in
/// person only.
pub(crate) async fn remove(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Json(request): Json<Ids>,
) -> Result<StatusCode, ApiError> {
    check_ids(&request)?;
    state
        .events
        .remove_notifications(me.user.id, request.ids)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /notifications?up_to={id}`: removes every notification up to
/// the newest the person was shown, so one that arrives meanwhile stays.
pub(crate) async fn clear(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Query(query): Query<UpTo>,
) -> Result<StatusCode, ApiError> {
    state
        .events
        .clear_notifications(me.user.id, query.up_to)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
