//! Each person's Drive settings: the page Drive opens on, and whether grid
//! view shows thumbnails. They're kept on the server, so they follow the
//! person to every device. Someone who never changes them has no row, and
//! gets the defaults.

use accordion_core::db::rusqlite::OptionalExtension;
use accordion_core::db::{Database, Migration};
use accordion_core::storage::Error;
use accordion_types::UserId;
use serde::{Deserialize, Serialize};

pub(crate) const MIGRATION: Migration = Migration {
    version: 2,
    name: "Create settings",
    sql: "
    CREATE TABLE drive_settings (
        user_id BLOB PRIMARY KEY NOT NULL CHECK (length(user_id) = 16),
        start_page TEXT NOT NULL CHECK (start_page IN ('home', 'my-drive')),
        thumbnails INTEGER NOT NULL CHECK (thumbnails IN (0, 1))
    ) STRICT, WITHOUT ROWID;
    ",
};

/// The page Drive opens on from the server's address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StartPage {
    /// Recent files, as in Google Drive.
    #[default]
    Home,
    MyDrive,
}

impl StartPage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::MyDrive => "my-drive",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DriveSettings {
    pub start_page: StartPage,
    pub thumbnails: bool,
}

impl Default for DriveSettings {
    fn default() -> Self {
        Self {
            start_page: StartPage::Home,
            thumbnails: true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Settings {
    db: Database,
}

impl Settings {
    pub(crate) fn new(db: Database) -> Self {
        Self { db }
    }

    pub(crate) async fn get(&self, user: UserId) -> Result<DriveSettings, Error> {
        self.db
            .read(move |conn| {
                let found = conn
                    .query_row(
                        "SELECT start_page, thumbnails FROM drive_settings WHERE user_id = ?1",
                        [user.as_uuid()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
                    )
                    .optional()?;
                Ok(match found {
                    Some((start_page, thumbnails)) => DriveSettings {
                        start_page: if start_page == "my-drive" {
                            StartPage::MyDrive
                        } else {
                            StartPage::Home
                        },
                        thumbnails,
                    },
                    None => DriveSettings::default(),
                })
            })
            .await
    }

    pub(crate) async fn set(&self, user: UserId, settings: DriveSettings) -> Result<(), Error> {
        self.db
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO drive_settings (user_id, start_page, thumbnails)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT (user_id) DO UPDATE SET
                         start_page = excluded.start_page,
                         thumbnails = excluded.thumbnails",
                    (
                        user.as_uuid(),
                        settings.start_page.as_str(),
                        settings.thumbnails,
                    ),
                )?;
                Ok(())
            })
            .await
    }
}
