//! Google Drive's files from Google Takeout (decision 34 in the `accordion`
//! repo's `docs/decisions`), as the sandbox reads them.
//!
//! Takeout writes an `-info.json` beside each file and folder, with its
//! full title, dates, star, description, and sharing. This matches each
//! file to its info, which takes some care:
//!
//! - Two files with one name come out as `Budget.xlsx` and
//!   `Budget(1).xlsx`, with infos `Budget-info.json` and
//!   `Budget-info(1).json`.
//! - Google's own files come out as Office files, so a Doc titled `Resume`
//!   is `Resume.docx`.
//! - A `.tgz` cuts names to about 100 bytes, so `Long name….txt` and its
//!   info become `Long na.txt` and `Long na.json`. The info's title still
//!   has the whole name.
//! - Kept versions come out as copies, `<name>-at-<time>-pinned.<ext>`,
//!   beside their file or in a top-level folder of their own.
//! - Trash is a top-level folder. It and the kept versions' folder are the
//!   only folders without an info, which tells them apart in any language.
//!
//! Everything here runs in the sandbox on files from the archive, so it
//! reads them as untrusted. The server checks the result again.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use accordion_core::takeout::{FolderEntry, Planned, PlannedFile, Product, ProductFolder};
use serde::{Deserialize, Serialize};

/// Google Drive, as plans and the report name it.
pub const PRODUCT: &str = "drive";

/// The most of each list an item carries.
pub const MAX_LIST: usize = 1000;
/// The longest description Drive keeps, in characters.
pub const MAX_DESCRIPTION: usize = crate::descriptions::MAX_LEN;

/// Google Drive, for the reader's list of products.
pub struct GoogleDrive;

/// What an item is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// **From Google Drive** itself, which carries what the whole export
    /// says, such as files missing from it.
    Root,
    Folder,
    #[default]
    File,
}

/// One item, as the server's side stores it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DriveItem {
    pub kind: Kind,
    /// The folders it's in, under **From Google Drive**.
    pub path: Vec<String>,
    /// Its name: the info's title, with the extension the export gave it.
    pub name: String,
    /// When it was made and last changed, in milliseconds since 1970.
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub starred: bool,
    pub description: String,
    /// Who else it was shared with, in words, such as
    /// `alex@example.com (editor)`.
    pub shared_with: Vec<String>,
    /// What Google file it was, such as "Google Doc", when it came out as
    /// an Office file.
    pub google: Option<String>,
    /// Whether it had an info. Without one, it keeps its name from the
    /// archive and has no dates.
    pub info: bool,
    /// Whether it goes to the trash once stored.
    pub trash: bool,
    /// When each kept version was made, in milliseconds since 1970, in the
    /// order of the item's `version` files.
    pub versions: Vec<i64>,
    /// Whether Google's page of its comments was left out, since the
    /// Word file has them.
    pub comments: bool,
    /// Why it isn't converted, for a file that goes to **Unconverted**.
    pub why: Option<String>,
    /// Files the infos name that aren't in the archives, by path.
    pub missing: Vec<String>,
    pub shortcuts: Vec<Shortcut>,
}

/// A shortcut, which isn't stored: the report lists it with its link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    pub path: String,
    pub url: String,
}

/// An `-info.json`, as far as Accordion reads it.
#[derive(Clone, Debug, Deserialize)]
struct Info {
    title: String,
    drive_id: String,
    mime_type: String,
    #[serde(default)]
    starred: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    content_last_modified: Option<String>,
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    permissions: Vec<Permission>,
}

#[derive(Clone, Debug, Deserialize)]
struct Permission {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    email_address: Option<String>,
    #[serde(default)]
    domain: Option<String>,
}

const FOLDER_TYPE: &str = "application/vnd.google-apps.folder";
const SHORTCUT_TYPE: &str = "application/vnd.google-apps.shortcut";

impl Product for GoogleDrive {
    fn id(&self) -> &'static str {
        PRODUCT
    }

    fn wants(&self, path: &str) -> bool {
        path.ends_with(".json")
    }

    fn plan(&self, folder: &ProductFolder) -> Option<Vec<Planned>> {
        let infos: HashMap<usize, Info> = folder
            .entries
            .iter()
            .filter(|entry| !entry.folder && self.wants(&entry.path))
            .filter_map(|entry| {
                let info: Info = serde_json::from_slice(folder.read(entry)?).ok()?;
                (!info.drive_id.is_empty()).then_some((entry.id, info))
            })
            .collect();
        if infos.is_empty() {
            return None;
        }
        Some(Planner::new(folder.entries, infos).plan())
    }
}

/// A folder of the export, with what's directly in it.
#[derive(Default)]
struct Dir<'a> {
    files: Vec<&'a FolderEntry>,
    dirs: BTreeSet<String>,
}

/// An item being planned.
struct Draft {
    key: String,
    item: DriveItem,
    /// The file, if it's one, and its kept versions with their times.
    file: Option<usize>,
    versions: Vec<(i64, usize)>,
}

struct Planner<'a> {
    dirs: BTreeMap<String, Dir<'a>>,
    infos: HashMap<usize, Info>,
    /// The folder that holds the trash, and the one that holds kept
    /// versions, by their names in the archive.
    trash: Option<String>,
    unorganized: Option<String>,
}

impl<'a> Planner<'a> {
    fn new(entries: &'a [FolderEntry], infos: HashMap<usize, Info>) -> Self {
        let mut dirs: BTreeMap<String, Dir> = BTreeMap::new();
        dirs.entry(String::new()).or_default();
        for entry in entries {
            let path = entry.path.trim_end_matches('/');
            if entry.folder {
                add_dir(&mut dirs, path);
            } else {
                let (parent, _) = split_path(path);
                add_dir(&mut dirs, parent);
                dirs.get_mut(parent).expect("just added").files.push(entry);
            }
        }
        let mut planner = Self {
            dirs,
            infos,
            trash: None,
            unorganized: None,
        };
        // Takeout's own folders are the top-level ones without an info. A
        // folder whose info has a name a `.tgz` cut short is the person's,
        // so it's never taken for the trash.
        let root_infos: Vec<(String, Option<u32>, String)> = planner.dirs[""]
            .files
            .iter()
            .filter_map(|entry| {
                let info = planner.infos.get(&entry.id)?;
                let (x, n, _) = info_base(name_of(&entry.path))?;
                Some((x.to_owned(), n, info.title.clone()))
            })
            .collect();
        for top in planner.dirs[""].dirs.clone() {
            if root_infos.iter().any(|(x, n, title)| {
                folder_matches(&top, x, *n) || x.starts_with(&top) || title.starts_with(&top)
            }) {
                continue;
            }
            let dir = &planner.dirs[&top];
            let only_kept = dir.dirs.is_empty()
                && !dir.files.is_empty()
                && dir
                    .files
                    .iter()
                    .all(|entry| pinned(name_of(&entry.path)).is_some());
            if only_kept && planner.unorganized.is_none() {
                planner.unorganized = Some(top);
            } else if planner.trash.is_none() {
                planner.trash = Some(top);
            }
        }
        planner
    }

    fn plan(mut self) -> Vec<Planned> {
        let mut root = Draft {
            key: "From Google Drive".to_owned(),
            item: DriveItem {
                kind: Kind::Root,
                info: true,
                ..DriveItem::default()
            },
            file: None,
            versions: Vec::new(),
        };
        let mut drafts = Vec::new();
        self.walk("", Vec::new(), false, &mut root, &mut drafts);
        if let Some(trash) = self.trash.clone() {
            self.walk(
                &trash,
                vec![name_of(&trash).to_owned()],
                true,
                &mut root,
                &mut drafts,
            );
        }
        self.unorganized_versions(&mut drafts);

        let mut planned = vec![finish(root)];
        planned.extend(drafts.into_iter().map(finish));
        planned
    }

    /// Plans the folder `dir`, shown in Accordion at `path`, and everything
    /// in it. Items in the trash come after what's inside them, so a
    /// folder goes to the trash once its files are in it.
    /// Returns where the folder's own item is in `drafts`.
    fn walk(
        &mut self,
        dir: &str,
        path: Vec<String>,
        trash: bool,
        root: &mut Draft,
        drafts: &mut Vec<Draft>,
    ) -> Option<usize> {
        let found = self.dirs.get(dir)?;
        // What's directly in the trash goes there; what's inside a folder
        // in the trash goes with its folder.
        let trashed = trash && Some(dir) == self.trash.as_deref();
        let own_trashed = trash && Some(split_path(dir).0) == self.trash.as_deref();
        let files = found.files.clone();
        let subdirs: Vec<String> = found.dirs.iter().cloned().collect();

        let mut infos: Vec<(&FolderEntry, &str, Option<u32>, bool)> = Vec::new();
        let mut kept = Vec::new();
        let mut comments = Vec::new();
        let mut plain = Vec::new();
        for entry in &files {
            let name = name_of(&entry.path);
            if self.infos.contains_key(&entry.id)
                && let Some((x, n, cut)) = info_base(name)
            {
                infos.push((entry, x, n, cut));
                continue;
            }
            if pinned(name).is_some() {
                kept.push(*entry);
            } else if name.ends_with("-comments.html") {
                comments.push(*entry);
            } else {
                plain.push(*entry);
            }
        }
        infos.sort_by(|a, b| a.0.path.cmp(&b.0.path));

        // Each info to its file or folder: exact names first, then with an
        // extension added, then names a `.tgz` cut short.
        let mut file_of: HashMap<usize, usize> = HashMap::new();
        let mut taken_files: HashSet<usize> = HashSet::new();
        let mut folder_of: HashMap<String, usize> = HashMap::new();
        for pass in 0..3 {
            for (info_entry, x, n, cut) in &infos {
                if file_of.contains_key(&info_entry.id)
                    || folder_of.values().any(|id| *id == info_entry.id)
                {
                    continue;
                }
                let info = &self.infos[&info_entry.id];
                if info.mime_type == FOLDER_TYPE {
                    if pass == 0
                        && let Some(sub) = subdirs.iter().find(|sub| {
                            !folder_of.contains_key(*sub) && folder_matches(name_of(sub), x, *n)
                        })
                    {
                        folder_of.insert(sub.clone(), info_entry.id);
                    }
                    continue;
                }
                if *cut != (pass == 2) {
                    continue;
                }
                let found = plain.iter().find(|entry| {
                    !taken_files.contains(&entry.id)
                        && file_matches(name_of(&entry.path), x, *n, pass > 0)
                });
                if let Some(entry) = found {
                    taken_files.insert(entry.id);
                    file_of.insert(info_entry.id, entry.id);
                }
            }
        }

        // This folder's own item, ahead of what's in it unless it's in the
        // trash. The trash's own folder has none.
        let own = if dir.is_empty() || Some(dir) == self.trash.as_deref() {
            None
        } else {
            Some(drafts.len())
        };
        if own.is_some() {
            let name = path.last().cloned().unwrap_or_default();
            drafts.push(Draft {
                key: format!("folder {dir}"),
                item: DriveItem {
                    kind: Kind::Folder,
                    path: path[..path.len() - 1].to_vec(),
                    name,
                    trash: own_trashed,
                    ..DriveItem::default()
                },
                file: None,
                versions: Vec::new(),
            });
        }

        let mut here: Vec<Draft> = Vec::new();
        let mut names_here: HashMap<String, usize> = HashMap::new();
        for (info_entry, ..) in &infos {
            let info = self.infos[&info_entry.id].clone();
            if info.mime_type == FOLDER_TYPE {
                continue;
            }
            let Some(&file) = file_of.get(&info_entry.id) else {
                if info.mime_type == SHORTCUT_TYPE {
                    root.item.shortcuts.push(Shortcut {
                        path: shown(&path, &info.title),
                        url: info.download_url.clone().unwrap_or_default(),
                    });
                } else {
                    root.item.missing.push(shown(&path, &info.title));
                }
                continue;
            };
            let entry = files
                .iter()
                .find(|entry| entry.id == file)
                .expect("matched");
            let archived = name_of(&entry.path);
            if info.mime_type == SHORTCUT_TYPE {
                root.item.shortcuts.push(Shortcut {
                    path: shown(&path, &info.title),
                    url: info.download_url.clone().unwrap_or_default(),
                });
                continue;
            }
            let mut item = described(&info, &path, trashed);
            item.name = with_extension(&info.title, archived);
            names_here.insert(archived.to_owned(), here.len());
            here.push(Draft {
                key: info.drive_id.clone(),
                item,
                file: Some(file),
                versions: Vec::new(),
            });
        }
        for entry in &plain {
            if taken_files.contains(&entry.id) {
                continue;
            }
            let archived = name_of(&entry.path);
            names_here.insert(archived.to_owned(), here.len());
            here.push(Draft {
                key: format!("{} {}", entry.path, entry.size),
                item: DriveItem {
                    path: path.clone(),
                    name: archived.to_owned(),
                    trash: trashed,
                    ..DriveItem::default()
                },
                file: Some(entry.id),
                versions: Vec::new(),
            });
        }
        // Kept versions beside their file.
        for entry in kept {
            let name = name_of(&entry.path);
            let (original, time) = pinned(name).expect("kept");
            let at = names_here.get(&original).copied().or_else(|| {
                // Takeout cuts the start of a long name for a kept version.
                let mut ends = names_here
                    .iter()
                    .filter(|(name, _)| cut_from_start(name, &original));
                match (ends.next(), ends.next()) {
                    (Some((_, &at)), None) => Some(at),
                    _ => None,
                }
            });
            match at {
                Some(at) => here[at].versions.push((time, entry.id)),
                None => here.push(orphan(entry, &path)),
            }
        }
        // Google's page of a Doc's comments repeats what the Word file has.
        for entry in comments {
            let name = name_of(&entry.path);
            let base = name.trim_end_matches("-comments.html");
            let doc = here.iter_mut().find(|draft| {
                draft.file.is_some_and(|id| {
                    files
                        .iter()
                        .any(|file| file.id == id && name_of(&file.path) == format!("{base}.docx"))
                })
            });
            match doc {
                Some(doc) => doc.item.comments = true,
                None => here.push(Draft {
                    key: format!("{} {}", entry.path, entry.size),
                    item: DriveItem {
                        path: path.clone(),
                        name: name.to_owned(),
                        why: Some(
                            "it's Google's page of comments on a file that isn't a Word file, \
                             which may not have them"
                                .to_owned(),
                        ),
                        ..DriveItem::default()
                    },
                    file: Some(entry.id),
                    versions: Vec::new(),
                }),
            }
        }
        here.sort_by(|a, b| a.item.name.cmp(&b.item.name).then(a.key.cmp(&b.key)));
        drafts.extend(here);

        for sub in subdirs {
            if Some(sub.as_str()) == self.trash.as_deref()
                || Some(sub.as_str()) == self.unorganized.as_deref()
            {
                continue;
            }
            let name = match folder_of.get(&sub) {
                Some(info) => self.infos[info].title.clone(),
                None => name_of(&sub).to_owned(),
            };
            let mut inner = path.clone();
            inner.push(name);
            let own = self.walk(&sub, inner, trash, root, drafts);
            // Its info, now its item is made.
            if let (Some(info), Some(draft)) =
                (folder_of.get(&sub), own.and_then(|own| drafts.get_mut(own)))
            {
                let info = &self.infos[info];
                let described = described(info, &draft.item.path, draft.item.trash);
                draft.key = info.drive_id.clone();
                draft.item = DriveItem {
                    kind: Kind::Folder,
                    name: draft.item.name.clone(),
                    ..described
                };
            }
        }
        // Folders with an info but nothing in them, which archives leave
        // out.
        for (info_entry, ..) in &infos {
            let info = &self.infos[&info_entry.id];
            if info.mime_type != FOLDER_TYPE || folder_of.values().any(|id| *id == info_entry.id) {
                continue;
            }
            let mut item = described(info, &path, trashed);
            item.kind = Kind::Folder;
            item.name = info.title.clone();
            drafts.push(Draft {
                key: info.drive_id.clone(),
                item,
                file: None,
                versions: Vec::new(),
            });
        }

        // In the trash, a folder's item comes after what's in it.
        match own {
            Some(own) if trash => {
                let draft = drafts.remove(own);
                drafts.push(draft);
                Some(drafts.len() - 1)
            }
            own => own,
        }
    }

    /// Matches the kept versions in Takeout's own folder to their files by
    /// name, anywhere in the export. One whose file isn't there goes to
    /// **Unconverted**.
    fn unorganized_versions(&self, drafts: &mut Vec<Draft>) {
        let Some(unorganized) = &self.unorganized else {
            return;
        };
        let Some(dir) = self.dirs.get(unorganized) else {
            return;
        };
        let names: HashMap<usize, &str> = self
            .dirs
            .values()
            .flat_map(|dir| dir.files.iter())
            .map(|entry| (entry.id, name_of(&entry.path)))
            .collect();
        for entry in &dir.files {
            let (original, time) = pinned(name_of(&entry.path)).expect("only kept versions");
            let named = |exact: bool, draft: &Draft| {
                draft.item.why.is_none()
                    && draft.file.is_some_and(|id| {
                        names.get(&id).is_some_and(|name| {
                            if exact {
                                *name == original
                            } else {
                                cut_from_start(name, &original)
                            }
                        })
                    })
            };
            let at = drafts
                .iter()
                .position(|draft| named(true, draft))
                .or_else(|| {
                    let mut ends = drafts
                        .iter()
                        .enumerate()
                        .filter(|(_, draft)| named(false, draft));
                    match (ends.next(), ends.next()) {
                        (Some((at, _)), None) => Some(at),
                        _ => None,
                    }
                });
            let found = at.map(|at| &mut drafts[at]);
            match found {
                Some(draft) => draft.versions.push((time, entry.id)),
                None => drafts.push(orphan(entry, &[name_of(unorganized).to_owned()])),
            }
        }
    }
}

/// A kept version whose file isn't in the export, for **Unconverted**.
fn orphan(entry: &FolderEntry, path: &[String]) -> Draft {
    Draft {
        key: format!("{} {}", entry.path, entry.size),
        item: DriveItem {
            path: path.to_vec(),
            name: name_of(&entry.path).to_owned(),
            why: Some("it's a kept version of a file that isn't in the archives".to_owned()),
            ..DriveItem::default()
        },
        file: Some(entry.id),
        versions: Vec::new(),
    }
}

fn finish(mut draft: Draft) -> Planned {
    draft.versions.sort();
    draft.versions.dedup_by_key(|(_, entry)| *entry);
    let mut files: Vec<PlannedFile> = draft
        .versions
        .iter()
        .map(|(_, entry)| PlannedFile {
            role: "version".to_owned(),
            entry: *entry,
        })
        .collect();
    draft.item.versions = draft.versions.iter().map(|(time, _)| *time).collect();
    if let Some(file) = draft.file {
        files.push(PlannedFile {
            role: "file".to_owned(),
            entry: file,
        });
    }
    draft.item.missing.truncate(MAX_LIST);
    draft.item.shortcuts.truncate(MAX_LIST);
    Planned {
        key: draft.key,
        fields: serde_json::to_value(draft.item).expect("plain fields"),
        files,
    }
}

/// The item's details from its info.
fn described(info: &Info, path: &[String], trash: bool) -> DriveItem {
    let description: String = info
        .description
        .as_deref()
        .unwrap_or_default()
        .chars()
        .take(MAX_DESCRIPTION)
        .collect();
    let shared_with = info
        .permissions
        .iter()
        .filter(|permission| permission.role != "owner")
        .filter_map(|permission| {
            let who = match permission.kind.as_str() {
                "anyone" => "anyone with the link".to_owned(),
                "domain" => format!("anyone at {}", permission.domain.as_deref()?),
                _ => permission.email_address.clone()?,
            };
            let role = match permission.role.as_str() {
                "writer" | "fileOrganizer" | "organizer" => "editor",
                "commenter" => "commenter",
                _ => "viewer",
            };
            Some(format!("{who} ({role})"))
        })
        .take(MAX_LIST)
        .collect();
    DriveItem {
        kind: Kind::File,
        path: path.to_vec(),
        name: info.title.clone(),
        created: info.created.as_deref().and_then(parse_time),
        modified: info.content_last_modified.as_deref().and_then(parse_time),
        starred: info.starred,
        description,
        shared_with,
        google: info.download_url.as_deref().and_then(google_kind),
        info: true,
        trash,
        ..DriveItem::default()
    }
}

/// What Google file a `download_url` says it was, for files Takeout
/// turned into Office files.
fn google_kind(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://docs.google.com/")?;
    let kind = match rest.split('/').next()? {
        "document" => "Google Doc",
        "spreadsheets" => "Google Sheet",
        "presentation" => "Google Slides",
        "drawings" => "Google Drawing",
        _ => return None,
    };
    Some(kind.to_owned())
}

/// A path shown in the report, from **From Google Drive**.
fn shown(path: &[String], name: &str) -> String {
    let mut parts = path.to_vec();
    parts.push(name.to_owned());
    parts.join("/")
}

fn add_dir(dirs: &mut BTreeMap<String, Dir>, path: &str) {
    let mut path = path.to_owned();
    loop {
        let (parent, _) = split_path(&path);
        let parent = parent.to_owned();
        let known = dirs.contains_key(&path);
        dirs.entry(path.clone()).or_default();
        if path.is_empty() {
            return;
        }
        dirs.entry(parent.clone())
            .or_default()
            .dirs
            .insert(path.clone());
        if known {
            return;
        }
        path = parent;
    }
}

/// A path's folder and name.
fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn name_of(path: &str) -> &str {
    split_path(path).1
}

/// An info's name as what it's for, its number for a second file with one
/// name, and whether a `.tgz` cut it short: `Budget-info(1).json` is
/// `("Budget", Some(1), false)`, and `Long na.json` is `("Long na", None,
/// true)`.
fn info_base(name: &str) -> Option<(&str, Option<u32>, bool)> {
    let stem = name.strip_suffix(".json")?;
    if let Some(x) = stem.strip_suffix("-info") {
        return Some((x, None, false));
    }
    if let Some((x, number)) = stem.rsplit_once("-info(")
        && let Some(n) = number.strip_suffix(')').and_then(|n| n.parse().ok())
    {
        return Some((x, Some(n), false));
    }
    Some((stem, None, true))
}

/// Whether a file `name` is the one an info for `x` and number `n` is
/// about. With `extended`, the export may have added an extension, as it
/// does to Google's own files, or kept the rest of a name it cut.
fn file_matches(name: &str, x: &str, n: Option<u32>, extended: bool) -> bool {
    let added = |rest: Option<&str>| {
        extended
            && rest
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(is_extension)
    };
    match n {
        None => name == x || added(name.strip_prefix(x)),
        Some(n) => {
            let tag = format!("({n})");
            if let Some((stem, extension)) = split_extension(x)
                && name == format!("{stem}{tag}.{extension}")
            {
                return true;
            }
            let numbered = format!("{x}{tag}");
            name == numbered || added(name.strip_prefix(&numbered))
        }
    }
}

/// Whether a folder `name` is the one an info for `x` and number `n` is
/// about.
fn folder_matches(name: &str, x: &str, n: Option<u32>) -> bool {
    match n {
        None => name == x,
        Some(n) => name == format!("{x}({n})"),
    }
}

/// `name` split at its extension, if it has one.
fn split_extension(name: &str) -> Option<(&str, &str)> {
    let (stem, extension) = name.rsplit_once('.')?;
    (!stem.is_empty() && is_extension(extension)).then_some((stem, extension))
}

fn is_extension(text: &str) -> bool {
    !text.is_empty() && text.len() <= 16 && !text.contains(['.', ' ', '/'])
}

/// The info's title with the extension the archive gave the file, when
/// the title doesn't have it: Google's own files are titled `Resume` and
/// come out as `Resume.docx`.
fn with_extension(title: &str, archived: &str) -> String {
    match split_extension(archived) {
        Some((_, extension))
            if !title
                .to_lowercase()
                .ends_with(&format!(".{}", extension.to_lowercase())) =>
        {
            format!("{title}.{extension}")
        }
        _ => title.to_owned(),
    }
}

/// A kept version's file's name and when it was kept, for a name like
/// `Report-at-2026-11-24T06_44_20.926Z-pinned.pdf`.
fn pinned(name: &str) -> Option<(String, i64)> {
    let at = name.rfind("-at-")?;
    let rest = &name[at + 4..];
    let end = rest.find("Z-pinned")?;
    let time = parse_time(&format!("{}Z", rest[..end].replace('_', ":")))?;
    let extension = &rest[end + "Z-pinned".len()..];
    if !(extension.is_empty() || extension.starts_with('.')) {
        return None;
    }
    Some((format!("{}{extension}", &name[..at]), time))
}

/// Whether `cut` is `name` with its start cut off, as Takeout cuts long
/// names of kept versions. Too short a part could be any name's end.
fn cut_from_start(name: &str, cut: &str) -> bool {
    cut.len() >= 24 && name.len() > cut.len() && name.ends_with(cut)
}

/// A time like `2026-11-24T07:32:56.266Z`, in milliseconds since 1970.
pub fn parse_time(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let (time, millis) = match time.split_once('.') {
        Some((time, fraction)) => {
            let digits: String = fraction.chars().take(3).collect();
            let scale = 10i64.pow(3 - digits.len() as u32);
            (time, digits.parse::<i64>().ok()? * scale)
        }
        None => (time, 0),
    };
    let mut time = time.splitn(3, ':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
        || !(1970..=9999).contains(&year)
    {
        return None;
    }
    // Days from 1970 by the proleptic Gregorian calendar.
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let day_of_year = (153 * m + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_infos_names() {
        assert_eq!(info_base("Budget-info.json"), Some(("Budget", None, false)));
        assert_eq!(
            info_base("Budget-info(1).json"),
            Some(("Budget", Some(1), false))
        );
        assert_eq!(info_base("Long na.json"), Some(("Long na", None, true)));
        assert_eq!(info_base("Budget.xlsx"), None);
    }

    #[test]
    fn matches_files_to_infos() {
        assert!(file_matches("Report.pdf", "Report.pdf", None, false));
        assert!(!file_matches("Resume.docx", "Resume", None, false));
        assert!(file_matches("Resume.docx", "Resume", None, true));
        assert!(file_matches("Budget(1).xlsx", "Budget", Some(1), true));
        assert!(file_matches("Report(1).pdf", "Report.pdf", Some(1), false));
        assert!(!file_matches("Report(2).pdf", "Report.pdf", Some(1), true));
        assert!(file_matches("BSOD3.jpg.html", "BSOD3.jpg", None, true));
        assert!(!file_matches("Report.final.pdf", "Report", None, true));
        assert!(file_matches("Long na.txt", "Long na", None, true));
    }

    #[test]
    fn names_files_by_their_titles() {
        assert_eq!(with_extension("Resume", "Resume.docx"), "Resume.docx");
        assert_eq!(with_extension("Report.PDF", "Report.pdf"), "Report.PDF");
        assert_eq!(
            with_extension("Plans: 2026/27", "Plans_ 2026_27.txt"),
            "Plans: 2026/27.txt"
        );
        assert_eq!(with_extension("notes", "notes"), "notes");
    }

    #[test]
    fn reads_kept_versions_names() {
        assert_eq!(
            pinned("end-at-2026-11-24T07_34_47.369Z-pinned.txt"),
            Some((
                "end.txt".to_owned(),
                parse_time("2026-11-24T07:34:47.369Z").unwrap()
            ))
        );
        assert_eq!(
            pinned("block_ip-at-2026-11-24T06_22_30.782Z-pinned.htaccess")
                .unwrap()
                .0,
            "block_ip.htaccess"
        );
        assert_eq!(pinned("Meet-at-noon.txt"), None);
    }

    #[test]
    fn reads_times() {
        assert_eq!(parse_time("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_time("2026-11-24T07:32:56.266Z"),
            Some(1_795_505_576_266)
        );
        assert_eq!(parse_time("2000-02-29T12:00:00Z"), Some(951_825_600_000));
        assert_eq!(parse_time("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_time("yesterday"), None);
    }

    #[test]
    fn names_googles_own_files() {
        assert_eq!(
            google_kind("https://docs.google.com/document/d/x/edit").as_deref(),
            Some("Google Doc")
        );
        assert_eq!(google_kind("https://drive.google.com/file/d/x/view"), None);
    }
}
