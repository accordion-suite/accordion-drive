//! Storing Google Drive's files from Google Takeout (decision 34), on the
//! server. The sandbox matched each file to its info and planned a
//! [`DriveItem`]; this checks it again, since it came from the archive,
//! and stores it under **From Google Drive** in the person's My Drive with
//! its name, dates, star, description, and kept versions.

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use accordion_core::storage::{Dates, Error, ItemKind};
use accordion_core::takeout::imports::BoxFuture;
use accordion_core::takeout::{Importer, Importing, ItemError, Unpacked, UnpackedFile, escape};
use accordion_types::ItemId;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::descriptions::Descriptions;
use crate::stars::Stars;
use crate::takeout::{self, DriveItem, Kind};

/// The folder in My Drive that Google Drive's files go in, so they don't
/// mix with what's already there.
pub(crate) const ROOT: &str = "From Google Drive";

/// The report's headings for what only Drive has.
const GOOGLE: &str = "Google Docs, Sheets, and Slides";
const COMMENTS: &str = "Comments";
const SHARED: &str = "Files that were shared";
const NO_INFO: &str = "Files without details from Google";
const MISSING: &str = "Files missing from the archives";
const SHORTCUTS: &str = "Shortcuts";
const SHARED_WITH_YOU: &str = "Files shared with you";

/// The deepest folder path taken from the archive.
const MAX_DEPTH: usize = 64;
/// The longest name taken from the archive, in bytes, before it's
/// shortened to fit.
const MAX_NAME: usize = 4096;

pub(crate) struct DriveImporter {
    pub(crate) stars: Stars,
    pub(crate) descriptions: Descriptions,
}

impl Importer for DriveImporter {
    fn product(&self) -> &'static str {
        takeout::PRODUCT
    }

    fn app(&self) -> &'static str {
        "Drive"
    }

    fn brought(&self, count: u64) -> String {
        let what = if count == 1 {
            "file or folder"
        } else {
            "files and folders"
        };
        format!("{count} {what} from Google Drive, in **{ROOT}** in Drive")
    }

    fn label(&self, item: &Unpacked) -> String {
        match serde_json::from_value::<DriveItem>(item.fields.clone()) {
            Ok(drive) if drive.kind == Kind::Root => format!("**{ROOT}**"),
            Ok(drive) => escape(&shown(&drive)),
            Err(_) => escape(&item.key),
        }
    }

    fn store<'a>(
        &'a self,
        item: &'a Unpacked,
        importing: &'a Importing,
    ) -> BoxFuture<'a, Result<Option<ItemId>, ItemError>> {
        Box::pin(self.store_item(item, importing))
    }
}

/// The item's path from **From Google Drive**, as the report shows it.
fn shown(drive: &DriveItem) -> String {
    let mut parts = drive.path.clone();
    parts.push(drive.name.clone());
    parts.join("/")
}

impl DriveImporter {
    async fn store_item(
        &self,
        item: &Unpacked,
        importing: &Importing,
    ) -> Result<Option<ItemId>, ItemError> {
        let drive = serde_json::from_value::<DriveItem>(item.fields.clone())
            .map(checked)
            .map_err(|_| ItemError::NotConverted("Accordion couldn't read its details.".into()))?;
        if let Some(why) = &drive.why {
            return Err(ItemError::NotConverted(escape(why)));
        }
        let person = importing.person();
        let top = importing.storage().top_folder(person).await?;
        let root = importing.folder(top, ROOT).await?;
        let label = escape(&shown(&drive));

        let stored = match drive.kind {
            Kind::Root => {
                for missing in &drive.missing {
                    importing.report(MISSING, escape(missing));
                }
                for shortcut in &drive.shortcuts {
                    let link = if shortcut.url.starts_with("https://") {
                        format!(": <{}>", shortcut.url.replace(['<', '>', ' '], ""))
                    } else {
                        String::new()
                    };
                    importing.report(SHORTCUTS, format!("{}{link}", escape(&shortcut.path)));
                }
                importing.report(
                    SHARED_WITH_YOU,
                    "Takeout leaves out files other people shared with you, and the files \
                     shortcuts point to. To keep them, open **Shared with me** in Google Drive, \
                     select them, then select **Download**.",
                );
                return Ok(Some(root));
            }
            Kind::Folder => {
                let parent = resolve(importing, root, &drive.path).await?;
                folder_dated(importing, parent, &drive.name, dates(&drive)).await?
            }
            Kind::File => {
                let parent = resolve(importing, root, &drive.path).await?;
                self.file(importing, parent, &drive, item).await?
            }
        };

        if drive.starred {
            self.stars.star(person, stored).await?;
        }
        if !drive.description.is_empty() {
            self.descriptions
                .set(person, stored, drive.description.clone())
                .await?;
        }
        if !drive.shared_with.is_empty() {
            let with: Vec<String> = drive.shared_with.iter().map(|who| escape(who)).collect();
            importing.report(SHARED, format!("{label}: {}", with.join(", ")));
        }
        if let Some(google) = &drive.google {
            importing.report(GOOGLE, format!("{label}, a {}", escape(google)));
        }
        if drive.comments {
            importing.report(
                COMMENTS,
                format!("{label}: its comments are in the Word file, where Word and LibreOffice show them"),
            );
        }
        if drive.kind == Kind::File && !drive.info {
            importing.report(
                NO_INFO,
                format!("{label}: it keeps its name from the archive and today's date"),
            );
        }
        if drive.trash {
            match importing.storage().delete(stored).await {
                Ok(_) | Err(Error::InTrash) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(Some(stored))
    }

    /// Stores the file with its kept versions, oldest first. A kept
    /// version that's the same as the file, or as another, is dropped.
    async fn file(
        &self,
        importing: &Importing,
        parent: ItemId,
        drive: &DriveItem,
        item: &Unpacked,
    ) -> Result<ItemId, ItemError> {
        let Some(current) = item.files.iter().find(|file| file.role == "file") else {
            return Err(ItemError::NotConverted(
                "its file isn't in the archives".into(),
            ));
        };
        let versions: Vec<&UnpackedFile> = item
            .files
            .iter()
            .filter(|file| file.role == "version")
            .collect();
        let mut kept = Vec::new();
        if versions.len() == drive.versions.len() && !versions.is_empty() {
            let mut seen = HashSet::from([hash(current).await?]);
            for (file, &time) in versions.into_iter().zip(&drive.versions) {
                if seen.insert(hash(file).await?) {
                    kept.push((file, time));
                }
            }
        }
        let dates = dates(drive);
        let Some(((first, first_time), older)) = kept.split_first() else {
            let saved = importing
                .create_file_from(parent, &drive.name, current, dates)
                .await?;
            return Ok(saved.id);
        };
        let at = |time: i64| millis(time);
        let created = dates.map_or(at(*first_time), |dates| dates.created);
        let saved = importing
            .create_file_from(
                parent,
                &drive.name,
                first,
                Some(Dates {
                    created,
                    modified: at(*first_time),
                }),
            )
            .await?;
        let storage = importing.storage();
        for (file, time) in older {
            storage
                .write_file_dated(
                    parent,
                    &saved.name,
                    open(file).await?,
                    Dates {
                        created,
                        modified: at(*time),
                    },
                )
                .await?;
        }
        let latest = open(current).await?;
        let saved = match dates {
            Some(dates) => {
                storage
                    .write_file_dated(parent, &saved.name, latest, dates)
                    .await?
            }
            None => storage.write_file(parent, &saved.name, latest).await?,
        };
        Ok(saved.id)
    }
}

/// The folder under **From Google Drive** at `path`, made where it isn't
/// there.
async fn resolve(importing: &Importing, root: ItemId, path: &[String]) -> Result<ItemId, Error> {
    let mut folder = root;
    for name in path {
        folder = importing.folder(folder, name).await?;
    }
    Ok(folder)
}

/// The folder `name` in `parent`, made with `dates` if it isn't there. A
/// folder already there, from this import or an earlier one, is used as
/// it is.
async fn folder_dated(
    importing: &Importing,
    parent: ItemId,
    name: &str,
    dates: Option<Dates>,
) -> Result<ItemId, Error> {
    let usable = usable(name);
    let Some(dates) = dates.filter(|_| usable.len() <= 255) else {
        return importing.folder(parent, name).await;
    };
    match importing
        .storage()
        .find(parent, vec![usable.clone()])
        .await?
    {
        Some(found) if found.kind == ItemKind::Folder => Ok(found.id),
        Some(_) => importing.folder(parent, name).await,
        None => Ok(importing
            .create_folder(parent, &usable, Some(dates))
            .await?
            .id),
    }
}

/// `name` with what storage refuses replaced, as [`Importing::folder`]
/// does, so both find the same folder.
fn usable(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if c == '/' || c == '\0' { '_' } else { c })
        .collect();
    match name.as_str() {
        "" | "." | ".." => "_".to_owned(),
        _ => name,
    }
}

/// The item's dates, when its info gave them.
fn dates(drive: &DriveItem) -> Option<Dates> {
    let modified = drive.modified.or(drive.created)?;
    let created = drive.created.unwrap_or(modified);
    Some(Dates {
        created: millis(created),
        modified: millis(modified),
    })
}

fn millis(time: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(time.max(0).unsigned_abs())
}

/// The item as Drive takes it, whatever the sandbox said: names on one
/// line, lists and lengths capped, and dates within reason.
fn checked(mut drive: DriveItem) -> DriveItem {
    let clean = |text: &str, max: usize| -> String {
        let mut text: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let mut end = text.len().min(max);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text
    };
    drive.path.truncate(MAX_DEPTH);
    drive.path = drive
        .path
        .iter()
        .map(|part| clean(part, MAX_NAME))
        .collect();
    drive.name = clean(&drive.name, MAX_NAME);
    if drive.name.trim().is_empty() && drive.kind != Kind::Root {
        drive.why = Some("it has no name".to_owned());
    }
    drive.description = drive
        .description
        .chars()
        .take(takeout::MAX_DESCRIPTION)
        .collect();
    // Before year 10,000, in milliseconds.
    let sensible = |time: &i64| (0..253_402_300_800_000).contains(time);
    drive.created = drive.created.filter(sensible);
    drive.modified = drive.modified.filter(sensible);
    drive.versions = drive
        .versions
        .into_iter()
        .map(|time| time.clamp(0, 253_402_300_799_999))
        .collect();
    drive.shared_with.truncate(takeout::MAX_LIST);
    drive.shared_with = drive
        .shared_with
        .iter()
        .map(|who| clean(who, 400))
        .collect();
    drive.google = drive.google.map(|google| clean(&google, 40));
    drive.missing.truncate(takeout::MAX_LIST);
    drive.missing = drive
        .missing
        .iter()
        .map(|path| clean(path, MAX_NAME))
        .collect();
    drive.shortcuts.truncate(takeout::MAX_LIST);
    for shortcut in &mut drive.shortcuts {
        shortcut.path = clean(&shortcut.path, MAX_NAME);
        shortcut.url = clean(&shortcut.url, 2048);
    }
    drive
}

async fn open(file: &UnpackedFile) -> Result<tokio::fs::File, ItemError> {
    tokio::fs::File::open(&file.file)
        .await
        .map_err(|err| ItemError::Stop(err.to_string()))
}

/// A file's SHA-256, to tell kept versions apart.
async fn hash(file: &UnpackedFile) -> Result<[u8; 32], ItemError> {
    let mut opened = open(file).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let read = opened
            .read(&mut buffer)
            .await
            .map_err(|err| ItemError::Stop(err.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}
