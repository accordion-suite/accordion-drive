//! Descriptions of files and folders, shown in the details panel, as in
//! Google Drive. Unlike colors, there's one per item, which everyone who
//! can open it sees and everyone who can edit it can change.
//!
//! Drive owns this table (decision 1), so it names items only by ID and
//! never joins core's tables. A description stays when its item goes to
//! the trash, for if it's restored.

use accordion_core::db::{Database, Migration};
use accordion_core::sql_now;
use accordion_core::storage::Error;
use accordion_types::{ItemId, UserId};

pub(crate) const MIGRATION: Migration = Migration {
    version: 4,
    name: "Create descriptions",
    sql: "
    CREATE TABLE drive_descriptions (
        item_id BLOB PRIMARY KEY CHECK (length(item_id) = 16),
        description TEXT NOT NULL,
        changed_by BLOB NOT NULL CHECK (length(changed_by) = 16),
        changed_at TEXT NOT NULL
    ) STRICT, WITHOUT ROWID;
    ",
};

/// The longest description, in characters.
pub(crate) const MAX_LEN: usize = 2000;

#[derive(Clone)]
pub(crate) struct Descriptions {
    db: Database,
}

impl Descriptions {
    pub(crate) fn new(db: Database) -> Self {
        Self { db }
    }

    pub(crate) async fn get(&self, item: ItemId) -> Result<Option<String>, Error> {
        self.db
            .read(move |conn| {
                let mut statement =
                    conn.prepare("SELECT description FROM drive_descriptions WHERE item_id = ?1")?;
                let found = statement
                    .query_map([item.as_uuid()], |row| row.get(0))?
                    .next()
                    .transpose()?;
                Ok(found)
            })
            .await
    }

    /// Sets an item's description, or removes it when it's empty.
    pub(crate) async fn set(
        &self,
        by: UserId,
        item: ItemId,
        description: String,
    ) -> Result<(), Error> {
        self.db
            .write(move |tx| {
                if description.is_empty() {
                    tx.execute(
                        "DELETE FROM drive_descriptions WHERE item_id = ?1",
                        [item.as_uuid()],
                    )?;
                } else {
                    tx.execute(
                        concat!(
                            "INSERT INTO drive_descriptions (item_id, description, changed_by, changed_at)
                             VALUES (?1, ?2, ?3, ",
                            sql_now!(),
                            ") ON CONFLICT (item_id) DO UPDATE SET description = excluded.description,
                                 changed_by = excluded.changed_by, changed_at = excluded.changed_at"
                        ),
                        (item.as_uuid(), &description, by.as_uuid()),
                    )?;
                }
                Ok(())
            })
            .await
    }
}
