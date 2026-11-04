//! Links to files and folders, for people without an account (decision 22
//! in the `accordion` repo's `docs/decisions`), and the part of `/api/drive`
//! that makes, lists, and removes them.
//!
//! A link's address carries a random 128-bit token. Only its SHA-256 is
//! stored, as for app passwords, so a link is shown once, when it's made.
//! A link gives view-only access to its item and everything inside it, and
//! can have a password, an expiry date, and a download limit. The pages it
//! opens are in [`crate::link_pages`], on a listener of their own.

use accordion_core::db::rusqlite::{self, OptionalExtension, Row};
use accordion_core::db::{Database, Migration};
use accordion_core::events::{Action, NewEvent};
use accordion_core::identity::password;
use accordion_core::sql_now;
use accordion_core::storage::{Access, Error};
use accordion_module::auth::actor;
use accordion_module::{ApiError, Authenticated};
use accordion_types::{ItemId, LinkId, Role, UserId};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use base64ct::{Base64UrlUnpadded, Encoding};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::DriveState;

pub(crate) const MIGRATION: Migration = Migration {
    version: 6,
    name: "links",
    sql: "
    CREATE TABLE drive_links (
        id BLOB PRIMARY KEY CHECK (length(id) = 16),
        item_id BLOB NOT NULL CHECK (length(item_id) = 16),
        -- The owner or an editor of the item.
        made_by BLOB NOT NULL CHECK (length(made_by) = 16),
        -- SHA-256 of the token in the link's address. The token itself is
        -- never stored.
        token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
        -- Argon2, when the link has a password.
        password_hash TEXT,
        -- Only its maker sees it, such as \"For the accountant\".
        note TEXT NOT NULL,
        created_at TEXT NOT NULL,
        expires_at TEXT,
        download_limit INTEGER CHECK (download_limit > 0),
        downloads INTEGER NOT NULL DEFAULT 0,
        opens INTEGER NOT NULL DEFAULT 0,
        last_opened_at TEXT,
        -- When its maker last heard that it was opened, at most once a day.
        noticed_at TEXT
    ) STRICT, WITHOUT ROWID;

    CREATE INDEX drive_links_item ON drive_links (item_id);
    ",
};

/// 16 random bytes, which is 22 characters in the address.
const TOKEN_BYTES: usize = 16;

const MAX_NOTE_LEN: usize = 200;

macro_rules! link_columns {
    () => {
        "id, item_id, made_by, password_hash IS NOT NULL, note, created_at, expires_at, \
         download_limit, downloads, opens, last_opened_at"
    };
}

/// A link, as its maker and admins see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Link {
    pub id: LinkId,
    pub item: ItemId,
    pub made_by: UserId,
    pub password: bool,
    pub note: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub download_limit: Option<u32>,
    pub downloads: u32,
    pub opens: u32,
    pub last_opened_at: Option<String>,
}

/// Drive's links. Cloning it is cheap.
#[derive(Clone)]
pub(crate) struct Links {
    db: Database,
}

/// How long a new link works.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub(crate) enum Expires {
    #[serde(rename = "1d")]
    Day,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
    #[serde(rename = "1y")]
    Year,
    #[serde(rename = "never")]
    Never,
}

impl Expires {
    /// SQLite's modifier for the expiry, or `None` for never.
    fn modifier(self) -> Option<&'static str> {
        match self {
            Self::Day => Some("+1 days"),
            Self::Week => Some("+7 days"),
            Self::Month => Some("+30 days"),
            Self::Year => Some("+365 days"),
            Self::Never => None,
        }
    }
}

/// What the link pages need to know about a link that still works.
pub(crate) struct Opened {
    pub link: Link,
    pub password_hash: Option<String>,
}

impl Links {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Makes a link, and returns its token, which is shown once.
    pub async fn create(
        &self,
        item: ItemId,
        made_by: UserId,
        password_hash: Option<String>,
        note: String,
        expires: Expires,
        download_limit: Option<u32>,
    ) -> Result<(String, Link), Error> {
        let mut bytes = [0; TOKEN_BYTES];
        getrandom::fill(&mut bytes).expect("the operating system's random number generator works");
        let token = Base64UrlUnpadded::encode_string(&bytes);
        let hash = hash_token(&token);
        let id = LinkId::generate();
        let link = self
            .db
            .write(move |tx| {
                tx.execute(
                    concat!(
                        "INSERT INTO drive_links (id, item_id, made_by, token_hash, password_hash, \
                         note, created_at, expires_at, download_limit)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ",
                        sql_now!(),
                        ", CASE WHEN ?7 IS NULL THEN NULL
                              ELSE strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ?7) END, ?8)"
                    ),
                    (
                        id.as_uuid(),
                        item.as_uuid(),
                        made_by.as_uuid(),
                        hash,
                        password_hash,
                        note,
                        expires.modifier(),
                        download_limit,
                    ),
                )?;
                Ok::<_, Error>(tx.query_row(
                    concat!(
                        "SELECT ",
                        link_columns!(),
                        " FROM drive_links WHERE id = ?1"
                    ),
                    [id.as_uuid()],
                    link_from_row,
                )?)
            })
            .await?;
        Ok((token, link))
    }

    /// The links to `item`, newest first.
    pub async fn for_item(&self, item: ItemId) -> Result<Vec<Link>, Error> {
        self.list("WHERE item_id = ?1", Some(item)).await
    }

    /// Every link, newest first, for admins.
    pub async fn all(&self) -> Result<Vec<Link>, Error> {
        self.list("", None).await
    }

    async fn list(
        &self,
        condition: &'static str,
        item: Option<ItemId>,
    ) -> Result<Vec<Link>, Error> {
        self.db
            .read(move |conn| {
                let sql = format!(
                    concat!(
                        "SELECT ",
                        link_columns!(),
                        " FROM drive_links {} ORDER BY created_at DESC, id DESC"
                    ),
                    condition
                );
                let mut statement = conn.prepare(&sql)?;
                let links = match item {
                    Some(item) => statement
                        .query_map([item.as_uuid()], link_from_row)?
                        .collect::<rusqlite::Result<_>>(),
                    None => statement
                        .query_map([], link_from_row)?
                        .collect::<rusqlite::Result<_>>(),
                };
                Ok::<_, Error>(links?)
            })
            .await
    }

    pub async fn get(&self, id: LinkId) -> Result<Option<Link>, Error> {
        self.db
            .read(move |conn| {
                Ok::<_, Error>(
                    conn.query_row(
                        concat!(
                            "SELECT ",
                            link_columns!(),
                            " FROM drive_links WHERE id = ?1"
                        ),
                        [id.as_uuid()],
                        link_from_row,
                    )
                    .optional()?,
                )
            })
            .await
    }

    /// The link with `token`, if it still works: it hasn't expired, and it
    /// has downloads left.
    pub async fn open(&self, token: &str) -> Result<Option<Opened>, Error> {
        let Some(hash) = parse_token(token) else {
            return Ok(None);
        };
        self.db
            .read(move |conn| {
                conn.query_row(
                    concat!(
                        "SELECT ",
                        link_columns!(),
                        ", password_hash FROM drive_links
                         WHERE token_hash = ?1
                           AND (expires_at IS NULL OR expires_at > ",
                        sql_now!(),
                        ")
                           AND (download_limit IS NULL OR downloads < download_limit)"
                    ),
                    [hash],
                    |row| {
                        Ok(Opened {
                            link: link_from_row(row)?,
                            password_hash: row.get(11)?,
                        })
                    },
                )
                .optional()
                .map_err(Error::from)
            })
            .await
    }

    pub async fn remove(&self, id: LinkId) -> Result<bool, Error> {
        self.db
            .write(move |tx| {
                Ok::<_, Error>(
                    tx.execute("DELETE FROM drive_links WHERE id = ?1", [id.as_uuid()])? > 0,
                )
            })
            .await
    }

    /// Counts an opening. Returns whether its maker should hear about it,
    /// which they do at most once a day for each link.
    pub async fn opened(&self, id: LinkId) -> Result<bool, Error> {
        self.db
            .write(move |tx| {
                tx.execute(
                    concat!(
                        "UPDATE drive_links SET opens = opens + 1, last_opened_at = ",
                        sql_now!(),
                        " WHERE id = ?1"
                    ),
                    [id.as_uuid()],
                )?;
                let notice = tx.execute(
                    concat!(
                        "UPDATE drive_links SET noticed_at = ",
                        sql_now!(),
                        " WHERE id = ?1 AND (noticed_at IS NULL
                             OR noticed_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 days'))"
                    ),
                    [id.as_uuid()],
                )?;
                Ok::<_, Error>(notice > 0)
            })
            .await
    }

    /// Counts a download, if the link has one left. Returns whether it did.
    pub async fn downloaded(&self, id: LinkId) -> Result<bool, Error> {
        self.db
            .write(move |tx| {
                Ok::<_, Error>(
                    tx.execute(
                        "UPDATE drive_links SET downloads = downloads + 1
                     WHERE id = ?1 AND (download_limit IS NULL OR downloads < download_limit)",
                        [id.as_uuid()],
                    )? > 0,
                )
            })
            .await
    }
}

fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// The hash to look up, or `None` if `token` can't be a link's.
fn parse_token(token: &str) -> Option<Vec<u8>> {
    let mut bytes = [0; TOKEN_BYTES];
    let decoded = Base64UrlUnpadded::decode(token, &mut bytes).ok()?;
    (decoded.len() == TOKEN_BYTES).then(|| hash_token(token))
}

fn link_from_row(row: &Row<'_>) -> rusqlite::Result<Link> {
    Ok(Link {
        id: LinkId::from_uuid(row.get(0)?),
        item: ItemId::from_uuid(row.get(1)?),
        made_by: UserId::from_uuid(row.get(2)?),
        password: row.get(3)?,
        note: row.get(4)?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        download_limit: row.get(7)?,
        downloads: row.get(8)?,
        opens: row.get(9)?,
        last_opened_at: row.get(10)?,
    })
}

#[derive(Serialize)]
pub(crate) struct LinkView {
    #[serde(flatten)]
    link: Link,
    /// The name of who made it.
    made_by_name: String,
}

#[derive(Serialize)]
pub(crate) struct AdminLinkView {
    #[serde(flatten)]
    link: LinkView,
    item_name: String,
    owner_name: String,
}

impl DriveState {
    async fn link_view(&self, link: Link) -> Result<LinkView, ApiError> {
        let made_by_name = self
            .identity
            .user(link.made_by)
            .await?
            .map_or_else(String::new, |user| user.display_name);
        Ok(LinkView { link, made_by_name })
    }

    /// The link origin, or why links can't be made now.
    fn link_origin(&self) -> Result<String, ApiError> {
        self.link_origin.borrow().clone().ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "Link sharing is off, so links can't reach anyone yet. Ask your admin to turn it \
                 on.",
            )
        })
    }
}

/// `GET /items/{id}/links`: an item's links, for its owner and editors.
pub(crate) async fn item_links(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
) -> Result<Json<Vec<LinkView>>, ApiError> {
    state.reach(me.user.id, id, Access::Editor).await?;
    let mut views = Vec::new();
    for link in state.links.for_item(id).await? {
        views.push(state.link_view(link).await?);
    }
    Ok(Json(views))
}

#[derive(Deserialize)]
pub(crate) struct NewLink {
    expires: Expires,
    #[serde(default)]
    password: String,
    download_limit: Option<u32>,
    #[serde(default)]
    note: String,
}

#[derive(Serialize)]
pub(crate) struct MadeLink {
    link: LinkView,
    /// The whole address, which is shown only now.
    url: String,
}

/// `POST /items/{id}/links`: makes a link to an item, for its owner and
/// editors, while link sharing is on.
pub(crate) async fn make_link(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<ItemId>,
    Json(request): Json<NewLink>,
) -> Result<(StatusCode, Json<MadeLink>), ApiError> {
    let (item, _) = state.reach(me.user.id, id, Access::Editor).await?;
    if item.parent.is_none() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "My Drive itself can't have a link. Link a folder or file inside it.",
        ));
    }
    let origin = state.link_origin()?;
    let note: String = request.note.trim().chars().take(MAX_NOTE_LEN).collect();
    if request.download_limit == Some(0) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "A download limit has to be at least 1.",
        ));
    }
    let password_hash = if request.password.is_empty() {
        None
    } else {
        Some(password::hash(request.password).await.map_err(|err| {
            tracing::error!(%err, "couldn't hash a link's password");
            ApiError::server_problem()
        })?)
    };
    let (token, link) = state
        .links
        .create(
            id,
            me.user.id,
            password_hash,
            note,
            request.expires,
            request.download_limit,
        )
        .await?;
    let event = NewEvent::about(Action::LinkCreated, id, item.owner).details(serde_json::json!({
        "link": link.id,
        "expires": link.expires_at,
    }));
    if let Err(err) = state.events.record(actor(&me), event).await {
        tracing::warn!(%err, "couldn't record a new link in the event log");
    }
    tracing::info!(user = %me.user.id, item = %id, link = %link.id, "made a link");
    Ok((
        StatusCode::CREATED,
        Json(MadeLink {
            link: state.link_view(link).await?,
            url: format!("{origin}/s/{token}"),
        }),
    ))
}

/// `DELETE /links/{id}`: removes a link, for whoever made it, the item's
/// owner, or an admin. It stops working at once.
pub(crate) async fn remove_link(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
    Path(id): Path<LinkId>,
) -> Result<StatusCode, ApiError> {
    let link = state.links.get(id).await?.ok_or_else(ApiError::not_found)?;
    let admin = matches!(me.user.role, Role::Owner | Role::Admin);
    let owner = state.storage.access(me.user.id, link.item).await? == Some(Access::Owner);
    if link.made_by != me.user.id && !owner && !admin {
        return Err(ApiError::not_found());
    }
    state.links.remove(id).await?;
    if let Some(item) = state.storage.item(link.item).await? {
        let event = NewEvent::about(Action::LinkRemoved, item.id, item.owner)
            .details(serde_json::json!({ "link": id }));
        if let Err(err) = state.events.record(actor(&me), event).await {
            tracing::warn!(%err, "couldn't record a removed link in the event log");
        }
    }
    tracing::info!(user = %me.user.id, link = %id, "removed a link");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /links`: every link on the server, for admins.
pub(crate) async fn all_links(
    State(state): State<DriveState>,
    Authenticated(me): Authenticated,
) -> Result<Json<Vec<AdminLinkView>>, ApiError> {
    if !matches!(me.user.role, Role::Owner | Role::Admin) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Only admins can see everyone's links.",
        ));
    }
    let mut views = Vec::new();
    for link in state.links.all().await? {
        // A link to an item that's gone opens nothing, so it isn't listed.
        let Some(item) = state.storage.item(link.item).await? else {
            continue;
        };
        let owner_name = state
            .identity
            .user(item.owner)
            .await?
            .map_or_else(String::new, |user| user.display_name);
        views.push(AdminLinkView {
            link: state.link_view(link).await?,
            item_name: item.name,
            owner_name,
        });
    }
    Ok(Json(views))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_tokens_of_the_right_length() {
        let token = Base64UrlUnpadded::encode_string(&[7; TOKEN_BYTES]);
        assert_eq!(token.len(), 22);
        assert!(parse_token(&token).is_some());
        assert!(parse_token("short").is_none());
        assert!(parse_token(&format!("{token}AA")).is_none());
        assert!(parse_token("not a token at all!!!!").is_none());
    }
}
