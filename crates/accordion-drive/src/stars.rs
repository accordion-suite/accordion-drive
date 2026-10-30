//! Stars: items people mark so they can find them again from **Starred**,
//! as in Google Drive. Each person has their own, so once sharing arrives,
//! two people can star the same item separately.
//!
//! Drive owns this table (decision 1), so it names items only by ID and
//! never joins core's tables. A starred item that goes to the trash keeps
//! its star, so it's still starred if it's restored.

use std::collections::HashSet;

use accordion_core::db::{Database, Migration};
use accordion_core::sql_now;
use accordion_core::storage::Error;
use accordion_types::{ItemId, UserId};

pub(crate) const OWNER: &str = "drive";

pub(crate) static MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "Create stars",
    sql: "
    CREATE TABLE drive_stars (
        user_id BLOB NOT NULL CHECK (length(user_id) = 16),
        item_id BLOB NOT NULL CHECK (length(item_id) = 16),
        starred_at TEXT NOT NULL,
        PRIMARY KEY (user_id, item_id)
    ) STRICT, WITHOUT ROWID;
    ",
}];

#[derive(Clone)]
pub(crate) struct Stars {
    db: Database,
}

impl Stars {
    pub(crate) fn new(db: Database) -> Self {
        Self { db }
    }

    /// Stars an item for `user`. Starring it again changes nothing.
    pub(crate) async fn star(&self, user: UserId, item: ItemId) -> Result<(), Error> {
        self.db
            .write(move |tx| {
                tx.execute(
                    concat!(
                        "INSERT INTO drive_stars (user_id, item_id, starred_at) VALUES (?1, ?2, ",
                        sql_now!(),
                        ") ON CONFLICT DO NOTHING"
                    ),
                    (user.as_uuid(), item.as_uuid()),
                )?;
                Ok(())
            })
            .await
    }

    pub(crate) async fn unstar(&self, user: UserId, items: Vec<ItemId>) -> Result<(), Error> {
        self.db
            .write(move |tx| {
                for item in items {
                    tx.execute(
                        "DELETE FROM drive_stars WHERE user_id = ?1 AND item_id = ?2",
                        (user.as_uuid(), item.as_uuid()),
                    )?;
                }
                Ok(())
            })
            .await
    }

    /// What `user` has starred, most recently starred first.
    pub(crate) async fn starred(&self, user: UserId) -> Result<Vec<ItemId>, Error> {
        self.db
            .read(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT item_id FROM drive_stars WHERE user_id = ?1
                     ORDER BY starred_at DESC, item_id",
                )?;
                let ids = statement
                    .query_map([user.as_uuid()], |row| Ok(ItemId::from_uuid(row.get(0)?)))?
                    .collect::<Result<_, _>>()?;
                Ok(ids)
            })
            .await
    }

    /// The same, as a set, for marking items in a list.
    pub(crate) async fn set(&self, user: UserId) -> Result<HashSet<ItemId>, Error> {
        Ok(self.starred(user).await?.into_iter().collect())
    }
}
