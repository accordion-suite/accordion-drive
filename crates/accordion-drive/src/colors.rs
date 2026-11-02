//! Folder colors, as in Google Drive. Each person picks their own, as with
//! stars, so a color one person picks for a shared folder doesn't change it
//! for anyone else.
//!
//! Drive owns this table (decision 1), so it names items only by ID and
//! never joins core's tables.

use std::collections::HashMap;

use accordion_core::db::{Database, Migration};
use accordion_core::storage::Error;
use accordion_types::{ItemId, UserId};

pub(crate) const MIGRATION: Migration = Migration {
    version: 3,
    name: "Create folder colors",
    sql: "
    CREATE TABLE drive_colors (
        user_id BLOB NOT NULL CHECK (length(user_id) = 16),
        item_id BLOB NOT NULL CHECK (length(item_id) = 16),
        color TEXT NOT NULL,
        PRIMARY KEY (user_id, item_id)
    ) STRICT, WITHOUT ROWID;
    ",
};

/// The colors a folder can have. The web app has a shade of each for light
/// and dark themes.
pub(crate) const COLORS: [&str; 9] = [
    "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink", "brown",
];

#[derive(Clone)]
pub(crate) struct Colors {
    db: Database,
}

impl Colors {
    pub(crate) fn new(db: Database) -> Self {
        Self { db }
    }

    /// Gives a folder a color for `user`, or takes it away with `None`.
    pub(crate) async fn set(
        &self,
        user: UserId,
        item: ItemId,
        color: Option<&'static str>,
    ) -> Result<(), Error> {
        self.db
            .write(move |tx| {
                match color {
                    Some(color) => tx.execute(
                        "INSERT INTO drive_colors (user_id, item_id, color) VALUES (?1, ?2, ?3)
                         ON CONFLICT (user_id, item_id) DO UPDATE SET color = excluded.color",
                        (user.as_uuid(), item.as_uuid(), color),
                    )?,
                    None => tx.execute(
                        "DELETE FROM drive_colors WHERE user_id = ?1 AND item_id = ?2",
                        (user.as_uuid(), item.as_uuid()),
                    )?,
                };
                Ok(())
            })
            .await
    }

    /// Every color `user` picked, for marking folders in a list.
    pub(crate) async fn all(&self, user: UserId) -> Result<HashMap<ItemId, String>, Error> {
        self.db
            .read(move |conn| {
                let mut statement =
                    conn.prepare("SELECT item_id, color FROM drive_colors WHERE user_id = ?1")?;
                let colors = statement
                    .query_map([user.as_uuid()], |row| {
                        Ok((ItemId::from_uuid(row.get(0)?), row.get(1)?))
                    })?
                    .collect::<Result<_, _>>()?;
                Ok(colors)
            })
            .await
    }
}
