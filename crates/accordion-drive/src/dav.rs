//! `/dav/drive`: WebDAV, so people can open Drive in File Explorer on
//! Windows, Finder on a Mac, file managers on Linux, and phone apps that
//! speak WebDAV.
//!
//! Clients sign in with their username and an app password (decision 3),
//! and see their own top folder at `/dav/drive/`. The `dav-server` crate
//! handles the protocol, and [`DriveFs`] connects it to core's storage.
//!
//! Two things differ from a plain file system:
//!
//! - **Deleting moves to the trash.** A folder goes to the trash whole, so
//!   restoring it brings everything inside back. `dav-server` would delete
//!   each file inside separately, so [`delete`] handles `DELETE` itself.
//!   File Explorer deletes what's inside a folder first, so [`gather`] puts
//!   that back into the folder before the folder goes.
//! - **Locks always succeed.** Windows and macOS only write to servers that
//!   support locks, but nothing here needs them: every save is kept as a
//!   version, so two people saving at once can't lose either copy.

use std::io::{self, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use accordion_core::identity::UsingAppPassword;
use accordion_core::storage::{self, Item, ItemKind, Storage};
use accordion_module::{ApiError, DavUser};
use accordion_types::{ItemId, UserId};
use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequestParts, OriginalUri, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::{Buf, Bytes};
use dav_server::DavHandler;
use dav_server::davpath::DavPath;
use dav_server::fakels::FakeLs;
use dav_server::fs::{
    DavDirEntry, DavFile, DavMetaData, FsError, FsFuture, FsResult, FsStream, GuardedFileSystem,
    OpenOptions, ReadDirMeta,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::io::StreamReader;

use crate::{DriveState, time};

/// Where the server mounts Drive's WebDAV routes (decision 7).
const PREFIX: &str = "/dav/drive";

/// How much of a file each read sends. dav-server's default is 16 KB, and
/// each read is a trip to tokio's blocking threads, so downloads spent more
/// time on the trips than on the data: 200 MB/s on a desktop, and far less on
/// a Raspberry Pi. 256 KB reaches 1.3 GB/s, and larger reads were no faster.
const READ_SIZE: usize = 256 * 1024;

/// Who a request is for, found once per request.
#[derive(Clone)]
pub(crate) struct Credentials {
    owner: UserId,
    top: ItemId,
}

pub(crate) fn handler(storage: Storage) -> DavHandler<Credentials> {
    DavHandler::builder()
        .filesystem(Box::new(DriveFs { storage }))
        .locksystem(FakeLs::new())
        .strip_prefix(PREFIX)
        .read_buf_size(READ_SIZE)
        .build_handler()
}

pub(crate) fn routes(state: DriveState) -> Router {
    Router::new().fallback(serve).with_state(state)
}

async fn serve(
    State(state): State<DriveState>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Response {
    if request.method() == Method::OPTIONS && !request.headers().contains_key(AUTHORIZATION) {
        return discovery();
    }
    let (mut parts, body) = request.into_parts();
    let using = match DavUser::from_request_parts(&mut parts, &state).await {
        Ok(DavUser(using)) => using,
        Err(rejection) => return rejection,
    };
    signed_in(state, using, uri, Request::from_parts(parts, body))
        .await
        .into_response()
}

/// Answers a client checking whether this is a WebDAV server. File Explorer
/// on Windows asks before it sends a password, and gives up if the answer is
/// "sign in first", so this answer doesn't need one. It says only what any
/// Accordion server supports.
fn discovery() -> Response {
    (
        [
            ("DAV", "1,2,3"),
            ("MS-Author-Via", "DAV"),
            (
                "Allow",
                "OPTIONS,GET,HEAD,PUT,DELETE,PROPFIND,PROPPATCH,MKCOL,COPY,MOVE,LOCK,UNLOCK",
            ),
        ],
        StatusCode::OK,
    )
        .into_response()
}

async fn signed_in(
    state: DriveState,
    using: UsingAppPassword,
    uri: Uri,
    request: Request,
) -> Result<Response, ApiError> {
    let owner = using.user.id;
    let top = state.storage.top_folder(owner).await?;
    // The server strips the prefix before routing, but the links that
    // dav-server sends back must include it.
    let (mut parts, body) = request.into_parts();
    parts.uri = uri;
    if parts.method == Method::DELETE {
        return delete(&state.storage, top, &parts.uri).await;
    }
    let response = state
        .dav
        .handle_guarded(
            Request::from_parts(parts, body),
            using.user.username,
            Credentials { owner, top },
        )
        .await;
    Ok(response.map(Body::new))
}

/// Moves what `uri` names to the trash, in one piece.
async fn delete(storage: &Storage, top: ItemId, uri: &Uri) -> Result<Response, ApiError> {
    let names = DavPath::new(uri.path())
        .ok()
        .and_then(|mut path| {
            path.set_prefix(PREFIX).ok()?;
            names(&path).ok()
        })
        .ok_or_else(ApiError::not_found)?;
    if names.is_empty() {
        return Err(storage::Error::TopFolder.into());
    }
    let item = storage
        .find(top, names)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if item.kind == ItemKind::Folder {
        gather(storage, &item).await?;
    }
    storage.delete(item.id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// How long before a folder is deleted that what's deleted from it counts
/// as deleted along with it.
const DELETED_TOGETHER: Duration = Duration::from_secs(10 * 60);

/// File Explorer deletes a folder one item at a time, then deletes the empty
/// folder. This puts back what was deleted from `folder` in the last few
/// minutes, so the folder goes to the trash whole, and restoring it brings
/// everything back. Items deleted earlier stay in the trash on their own,
/// because someone chose to delete them separately.
async fn gather(storage: &Storage, folder: &Item) -> Result<(), storage::Error> {
    let now = SystemTime::now();
    for item in storage.trash(folder.owner).await? {
        let recent = item
            .trashed_at
            .as_deref()
            .and_then(time::parse)
            .is_some_and(|at| {
                now.duration_since(at)
                    .is_ok_and(|age| age <= DELETED_TOGETHER)
                    || at >= now
            });
        if item.parent != Some(folder.id) || !recent {
            continue;
        }
        match storage.restore(item.id).await {
            // Something new has the name now, so the deleted item stays in
            // the trash on its own.
            Ok(_) | Err(storage::Error::NameTaken) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// The names along a path, such as `["Documents", "report.pdf"]` for
/// `/Documents/report.pdf`.
fn names(path: &DavPath) -> FsResult<Vec<String>> {
    path.as_bytes()
        .split(|&b| b == b'/')
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8(name.to_vec()).map_err(|_| FsError::NotFound))
        .collect()
}

fn fs_error(err: storage::Error) -> FsError {
    use storage::Error::*;
    match err {
        NotFound | UserNotFound | UploadNotFound => FsError::NotFound,
        NameTaken => FsError::Exists,
        InvalidName
        | NotAFolder
        | NotAFile
        | TopFolder
        | MoveIntoItself
        | DifferentOwner
        | InTrash
        | NotInTrash
        | UploadOffset { .. }
        | UploadIncomplete { .. } => FsError::Forbidden,
        QuotaFull => FsError::InsufficientStorage,
        UploadTooLong { .. } => FsError::TooLarge,
        Identity(_) | NoHardLinks(_) | Io(_) | Database(_) => {
            tracing::error!("Drive couldn't answer a WebDAV request: {err}");
            FsError::GeneralFailure
        }
    }
}

/// Core's storage, seen as a file system.
#[derive(Clone)]
struct DriveFs {
    storage: Storage,
}

impl DriveFs {
    async fn find(&self, top: ItemId, path: &DavPath) -> FsResult<Item> {
        self.storage
            .find(top, names(path)?)
            .await
            .map_err(fs_error)?
            .ok_or(FsError::NotFound)
    }

    /// The folder that a new item at `path` goes into, and the item's name.
    async fn parent_and_name(&self, top: ItemId, path: &DavPath) -> FsResult<(Item, String)> {
        let mut names = names(path)?;
        // The top folder has no parent, and can't be replaced.
        let name = names.pop().ok_or(FsError::Forbidden)?;
        let parent = self
            .storage
            .find(top, names)
            .await
            .map_err(fs_error)?
            .ok_or(FsError::NotFound)?;
        if parent.kind != ItemKind::Folder {
            return Err(FsError::NotFound);
        }
        Ok((parent, name))
    }
}

impl GuardedFileSystem<Credentials> for DriveFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
        credentials: &'a Credentials,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            let writing = options.write || options.append || options.create;
            if !writing {
                let item = self.find(credentials.top, path).await?;
                let ItemKind::File { blob, .. } = item.kind else {
                    return Err(FsError::Forbidden);
                };
                let file = self
                    .storage
                    .open_contents(blob.hash)
                    .await
                    .map_err(fs_error)?;
                return Ok(Box::new(Reading {
                    file,
                    meta: Meta::of(&item),
                }) as Box<dyn DavFile>);
            }
            let (parent, name) = self.parent_and_name(credentials.top, path).await?;
            let exists = match self.find(credentials.top, path).await {
                Ok(item) if item.kind == ItemKind::Folder => return Err(FsError::Forbidden),
                Ok(_) if options.create_new => return Err(FsError::Exists),
                Err(FsError::NotFound) if !options.create => return Err(FsError::NotFound),
                Ok(_) => true,
                Err(FsError::NotFound) => false,
                Err(err) => return Err(err),
            };
            // Every save replaces the whole file as a new version, so
            // changing part of a file isn't possible.
            if options.append || (exists && !options.truncate) {
                return Err(FsError::NotImplemented);
            }
            // Locking a name that doesn't exist yet creates an empty file
            // there, which is how Windows and macOS start a new file. The
            // lock never writes to it, so create it now.
            if !options.truncate {
                let item = self
                    .storage
                    .write_file(parent.id, &name, tokio::io::empty())
                    .await
                    .map_err(fs_error)?;
                let ItemKind::File { blob, .. } = item.kind else {
                    return Err(FsError::GeneralFailure);
                };
                let file = self
                    .storage
                    .open_contents(blob.hash)
                    .await
                    .map_err(fs_error)?;
                return Ok(Box::new(Reading {
                    file,
                    meta: Meta::of(&item),
                }) as Box<dyn DavFile>);
            }
            Ok(Box::new(Writing::start(
                self.storage.clone(),
                parent.id,
                name,
                options.size,
            )) as Box<dyn DavFile>)
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
        credentials: &'a Credentials,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let folder = self.find(credentials.top, path).await?;
            if folder.kind != ItemKind::Folder {
                return Err(FsError::Forbidden);
            }
            let children = self.storage.children(folder.id).await.map_err(fs_error)?;
            let entries = children.into_iter().map(|child| {
                Ok(Box::new(Entry {
                    name: child.name.clone().into_bytes(),
                    meta: Meta::of(&child),
                }) as Box<dyn DavDirEntry>)
            });
            Ok(Box::pin(futures_util::stream::iter(entries)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn metadata<'a>(
        &'a self,
        path: &'a DavPath,
        credentials: &'a Credentials,
    ) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let item = self.find(credentials.top, path).await?;
            Ok(Box::new(Meta::of(&item)) as Box<dyn DavMetaData>)
        })
    }

    fn create_dir<'a>(
        &'a self,
        path: &'a DavPath,
        credentials: &'a Credentials,
    ) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let (parent, name) = self.parent_and_name(credentials.top, path).await?;
            self.storage
                .create_folder(parent.id, &name)
                .await
                .map_err(fs_error)?;
            Ok(())
        })
    }

    fn remove_dir<'a>(
        &'a self,
        path: &'a DavPath,
        credentials: &'a Credentials,
    ) -> FsFuture<'a, ()> {
        self.remove(path, credentials)
    }

    fn remove_file<'a>(
        &'a self,
        path: &'a DavPath,
        credentials: &'a Credentials,
    ) -> FsFuture<'a, ()> {
        self.remove(path, credentials)
    }

    fn rename<'a>(
        &'a self,
        from: &'a DavPath,
        to: &'a DavPath,
        credentials: &'a Credentials,
    ) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let item = self.find(credentials.top, from).await?;
            let (parent, name) = self.parent_and_name(credentials.top, to).await?;
            self.storage
                .move_item(item.id, parent.id, &name)
                .await
                .map_err(fs_error)?;
            Ok(())
        })
    }

    fn copy<'a>(
        &'a self,
        from: &'a DavPath,
        to: &'a DavPath,
        credentials: &'a Credentials,
    ) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let item = self.find(credentials.top, from).await?;
            let ItemKind::File { blob, .. } = item.kind else {
                return Err(FsError::Forbidden);
            };
            let (parent, name) = self.parent_and_name(credentials.top, to).await?;
            // The copy has the same contents, so storage keeps them once.
            let contents = self
                .storage
                .open_contents(blob.hash)
                .await
                .map_err(fs_error)?;
            self.storage
                .write_file(parent.id, &name, contents)
                .await
                .map_err(fs_error)?;
            Ok(())
        })
    }

    fn get_quota<'a>(&'a self, credentials: &'a Credentials) -> FsFuture<'a, (u64, Option<u64>)> {
        Box::pin(async move {
            let usage = self
                .storage
                .usage(credentials.owner)
                .await
                .map_err(fs_error)?;
            Ok((usage.used + usage.uploading, usage.quota))
        })
    }
}

impl DriveFs {
    /// Moves an item to the trash. `DELETE` requests don't come here (see
    /// [`delete`]), but `MOVE` and `COPY` use it to replace what's at their
    /// destination.
    fn remove<'a>(&'a self, path: &'a DavPath, credentials: &'a Credentials) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let item = self.find(credentials.top, path).await?;
            self.storage.delete(item.id).await.map_err(fs_error)?;
            Ok(())
        })
    }
}

#[derive(Clone, Debug)]
struct Meta {
    folder: bool,
    len: u64,
    created: SystemTime,
    modified: SystemTime,
    etag: String,
}

impl Meta {
    fn of(item: &Item) -> Self {
        let created = time::parse(&item.created_at).unwrap_or(UNIX_EPOCH);
        let modified = time::parse(&item.modified_at).unwrap_or(created);
        match item.kind {
            ItemKind::Folder => Self {
                folder: true,
                len: 0,
                created,
                modified,
                etag: format!("{}-{}", item.id, item.modified_at),
            },
            ItemKind::File { blob, .. } => Self {
                folder: false,
                len: blob.size,
                created,
                modified,
                // The contents' hash changes exactly when the contents do.
                etag: blob.hash.to_hex(),
            },
        }
    }
}

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        self.len
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }

    fn created(&self) -> FsResult<SystemTime> {
        Ok(self.created)
    }

    fn is_dir(&self) -> bool {
        self.folder
    }

    fn etag(&self) -> Option<String> {
        Some(self.etag.clone())
    }
}

struct Entry {
    name: Vec<u8>,
    meta: Meta,
}

impl DavDirEntry for Entry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }

    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(std::future::ready(Ok(
            Box::new(self.meta.clone()) as Box<dyn DavMetaData>
        )))
    }
}

/// A file opened for reading.
#[derive(Debug)]
struct Reading {
    file: tokio::fs::File,
    meta: Meta,
}

impl DavFile for Reading {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        Box::pin(std::future::ready(Ok(
            Box::new(meta) as Box<dyn DavMetaData>
        )))
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            let mut buffer = vec![0; count];
            let read = self
                .file
                .read(&mut buffer)
                .await
                .map_err(|_| FsError::GeneralFailure)?;
            buffer.truncate(read);
            Ok(Bytes::from(buffer))
        })
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            self.file
                .seek(pos)
                .await
                .map_err(|_| FsError::GeneralFailure)
        })
    }

    fn write_bytes(&mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn write_buf(&mut self, _buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// A file being saved. What the client sends goes straight to storage,
/// which hashes it on the way in, so large files are never held in memory
/// or read twice.
///
/// The file is saved only if the client sends all of it. If the connection
/// drops partway, dav-server drops this without flushing it, storage sees an
/// error instead of the end of the file, and nothing is saved.
#[derive(Debug)]
struct Writing {
    sender: Option<mpsc::Sender<Bytes>>,
    /// Set just before the sender closes, to tell the end of the file from a
    /// dropped connection.
    finished: Arc<AtomicBool>,
    saving: Option<JoinHandle<Result<Item, storage::Error>>>,
    expected: Option<u64>,
    written: u64,
    saved: Option<Meta>,
}

impl Writing {
    fn start(storage: Storage, parent: ItemId, name: String, expected: Option<u64>) -> Self {
        let (sender, receiver) = mpsc::channel::<Bytes>(8);
        let finished = Arc::new(AtomicBool::new(false));
        let ended = Arc::clone(&finished);
        let pieces = futures_util::stream::unfold(Some(receiver), move |receiver| {
            let ended = Arc::clone(&ended);
            async move {
                let mut receiver = receiver?;
                match receiver.recv().await {
                    Some(piece) => Some((Ok(piece), Some(receiver))),
                    None if ended.load(Ordering::Acquire) => None,
                    None => Some((
                        Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "the client stopped sending the file partway",
                        )),
                        None,
                    )),
                }
            }
        });
        let contents = StreamReader::new(Box::pin(pieces));
        let saving = tokio::spawn(async move { storage.write_file(parent, &name, contents).await });
        Self {
            sender: Some(sender),
            finished,
            saving: Some(saving),
            expected,
            written: 0,
            saved: None,
        }
    }

    async fn send(&mut self, piece: Bytes) -> FsResult<()> {
        self.written += piece.len() as u64;
        let sender = self.sender.as_ref().ok_or(FsError::GeneralFailure)?;
        if sender.send(piece).await.is_err() {
            // Storage stopped early, such as when the quota ran out.
            return Err(self.saved().await.err().unwrap_or(FsError::GeneralFailure));
        }
        Ok(())
    }

    /// Waits for storage to finish saving.
    async fn saved(&mut self) -> FsResult<Item> {
        let saving = self.saving.take().ok_or(FsError::GeneralFailure)?;
        match saving.await {
            Ok(Ok(item)) => Ok(item),
            Ok(Err(err)) => Err(fs_error(err)),
            Err(_) => Err(FsError::GeneralFailure),
        }
    }
}

impl DavFile for Writing {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.saved.clone();
        Box::pin(std::future::ready(
            meta.map(|meta| Box::new(meta) as Box<dyn DavMetaData>)
                .ok_or(FsError::GeneralFailure),
        ))
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(self.send(buf))
    }

    fn write_buf(&mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        let piece = buf.copy_to_bytes(buf.remaining());
        Box::pin(self.send(piece))
    }

    fn read_bytes(&mut self, _count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(std::future::ready(Err(FsError::Forbidden)))
    }

    fn seek(&mut self, _pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(std::future::ready(Err(FsError::NotImplemented)))
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            if self.saved.is_some() {
                return Ok(());
            }
            // Fewer bytes than the client announced means the file is cut
            // short. Leaving `finished` unset makes storage refuse it.
            if self
                .expected
                .is_some_and(|expected| expected != self.written)
            {
                self.sender = None;
                let _ = self.saved().await;
                return Err(FsError::GeneralFailure);
            }
            self.finished.store(true, Ordering::Release);
            self.sender = None;
            let item = self.saved().await?;
            self.saved = Some(Meta::of(&item));
            Ok(())
        })
    }
}
