//! Downloads a folder, or several items, as one ZIP file, built as it's
//! sent, so a large download needs no copy on the disk and little memory.
//! Files go in as they are, without compressing them again: photos and
//! videos are compressed already, and a Raspberry Pi's processor has better
//! things to do.

use std::collections::{HashSet, VecDeque};
use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;

use accordion_core::storage::{self, ContentsHold, Item, ItemKind, Storage};
use bytes::Bytes;
use tokio::sync::mpsc;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

/// How much of the ZIP goes to the browser at a time.
const CHUNK: usize = 256 * 1024;

/// A file or folder in the ZIP, by its path inside it.
pub(crate) enum Entry {
    Folder {
        path: String,
        modified: String,
    },
    File {
        path: String,
        contents: PathBuf,
        size: u64,
        modified: String,
    },
}

/// What goes in the ZIP for `items`: each one, and everything in the folders
/// among them. Items with the same name, such as from different folders in
/// search results, get a number, as uploads do.
pub(crate) async fn entries(
    storage: &Storage,
    items: Vec<Item>,
) -> Result<Vec<Entry>, storage::Error> {
    let mut entries = Vec::new();
    let mut folders = VecDeque::new();
    let mut taken = HashSet::new();
    for item in items {
        let path = unique(&item.name, &mut taken);
        add(storage, item, path, &mut entries, &mut folders);
    }
    while let Some((folder, path)) = folders.pop_front() {
        for child in storage.children(folder).await? {
            let child_path = format!("{path}/{}", child.name);
            add(storage, child, child_path, &mut entries, &mut folders);
        }
    }
    Ok(entries)
}

fn add(
    storage: &Storage,
    item: Item,
    path: String,
    entries: &mut Vec<Entry>,
    folders: &mut VecDeque<(accordion_types::ItemId, String)>,
) {
    match item.kind {
        ItemKind::Folder => {
            folders.push_back((item.id, path.clone()));
            entries.push(Entry::Folder {
                path,
                modified: item.modified_at,
            });
        }
        ItemKind::File { blob, .. } => entries.push(Entry::File {
            path,
            contents: storage.contents_file(blob.hash),
            size: blob.size,
            modified: item.modified_at,
        }),
    }
}

/// `name`, or "name (1).ext", "name (2).ext", and so on, whichever isn't
/// taken yet.
fn unique(name: &str, taken: &mut HashSet<String>) -> String {
    if taken.insert(name.to_owned()) {
        return name.to_owned();
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    (1..)
        .map(|number| format!("{stem} ({number}){extension}"))
        .find(|candidate| taken.insert(candidate.clone()))
        .expect("some number is free")
}

/// Writes the ZIP of `entries` to `sender`, a chunk at a time. Runs on a
/// thread of its own, since the ZIP writer blocks. It stops when the
/// browser stops reading, and keeps `_hold` until then, so upkeep doesn't
/// remove contents it's still to read.
pub(crate) fn write(
    entries: Vec<Entry>,
    _hold: ContentsHold,
    sender: mpsc::Sender<io::Result<Bytes>>,
) {
    let out = Chunks {
        buffer: Vec::with_capacity(CHUNK),
        sender: sender.clone(),
    };
    let written = (move || -> zip::result::ZipResult<()> {
        let mut zip = ZipWriter::new_stream(out);
        for entry in entries {
            match entry {
                Entry::Folder { path, modified } => {
                    zip.add_directory(path, options(&modified, 0))?;
                }
                Entry::File {
                    path,
                    contents,
                    size,
                    modified,
                } => {
                    zip.start_file(path, options(&modified, size))?;
                    io::copy(&mut std::fs::File::open(contents)?, &mut zip)?;
                }
            }
        }
        zip.finish()?.flush()?;
        Ok(())
    })();
    if let Err(err) = written {
        // The browser shows the download as failed, rather than as a ZIP
        // that ends early. If it stopped reading, nobody's there to tell.
        tracing::info!(%err, "a ZIP download stopped");
        let _ = sender.blocking_send(Err(io::Error::other(err)));
    }
}

fn options(modified: &str, size: u64) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        // ZIP64, which every current system reads, for files of 4 GB and up.
        .large_file(size >= u64::from(u32::MAX))
        .last_modified_time(zip_time(modified))
}

/// A time that storage records, such as `2026-10-21T14:05:09.123Z`, as a ZIP
/// file's time, which has no time zone. Anything else is January 1, 1980,
/// the earliest a ZIP file can say.
fn zip_time(text: &str) -> DateTime {
    fn part<T: FromStr>(text: &str, range: std::ops::Range<usize>) -> Option<T> {
        text.get(range)?.parse().ok()
    }
    (|| {
        DateTime::from_date_and_time(
            part(text, 0..4)?,
            part(text, 5..7)?,
            part(text, 8..10)?,
            part(text, 11..13)?,
            part(text, 14..16)?,
            part(text, 17..19)?,
        )
        .ok()
    })()
    .unwrap_or_default()
}

/// Collects the ZIP into chunks for the response.
struct Chunks {
    buffer: Vec<u8>,
    sender: mpsc::Sender<io::Result<Bytes>>,
}

impl Write for Chunks {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(data);
        if self.buffer.len() >= CHUNK {
            self.flush()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.buffer, Vec::with_capacity(CHUNK));
        self.sender
            .blocking_send(Ok(Bytes::from(chunk)))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the browser stopped the download",
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_names_that_are_taken() {
        let mut taken = HashSet::new();
        assert_eq!(unique("notes.txt", &mut taken), "notes.txt");
        assert_eq!(unique("notes.txt", &mut taken), "notes (1).txt");
        assert_eq!(unique("notes.txt", &mut taken), "notes (2).txt");
        assert_eq!(unique("Photos", &mut taken), "Photos");
        assert_eq!(unique("Photos", &mut taken), "Photos (1)");
        assert_eq!(unique(".bashrc", &mut taken), ".bashrc");
        assert_eq!(unique(".bashrc", &mut taken), ".bashrc (1)");
    }

    #[test]
    fn reads_storage_times() {
        assert_eq!(
            zip_time("2026-10-21T14:05:09.123Z"),
            DateTime::from_date_and_time(2026, 10, 21, 14, 5, 9).unwrap()
        );
        assert_eq!(zip_time("yesterday"), DateTime::default());
    }
}
