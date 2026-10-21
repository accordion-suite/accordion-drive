//! Accordion Drive: people's files and folders, through a JSON API for
//! Accordion's apps and WebDAV for standard clients, such as File Explorer
//! on Windows and Finder on a Mac.
//!
//! Drive keeps no files of its own. Core's storage service holds them
//! (decision 6 in the `accordion` repo's `docs/decisions`), and Drive is how
//! people reach them. Each person sees only their own files until sharing
//! arrives.

mod api;
mod dav;
mod time;

use std::sync::Arc;

use accordion_core::identity::Identity;
use accordion_core::storage::{Item, Storage};
use accordion_module::{
    ApiError, BoxFuture, Module, Routes, Services, StartError, Started, Stopping,
};
use accordion_types::{ItemId, UserId};
use axum::extract::FromRef;

/// The Drive module. Add it to the server's list of modules.
pub struct Drive;

impl Module for Drive {
    fn id(&self) -> &'static str {
        "drive"
    }

    fn name(&self) -> &'static str {
        "Drive"
    }

    fn start(
        &self,
        services: Services,
        _stopping: Stopping,
    ) -> BoxFuture<Result<Started, StartError>> {
        Box::pin(async move {
            let state = DriveState {
                identity: services.identity,
                dav: Arc::new(dav::handler(services.storage.clone())),
                storage: services.storage,
            };
            Ok(Started {
                routes: Routes {
                    api: Some(api::routes(state.clone())),
                    dav: Some(dav::routes(state)),
                    pages: None,
                },
                background: None,
            })
        })
    }
}

#[derive(Clone)]
struct DriveState {
    identity: Identity,
    storage: Storage,
    dav: Arc<dav_server::DavHandler<dav::Credentials>>,
}

impl FromRef<DriveState> for Identity {
    fn from_ref(state: &DriveState) -> Self {
        state.identity.clone()
    }
}

impl DriveState {
    /// An item that `user` owns. Anyone else's is "not found", so nobody
    /// learns what other people have.
    async fn owned(&self, user: UserId, id: ItemId) -> Result<Item, ApiError> {
        match self.storage.item(id).await? {
            Some(item) if item.owner == user => Ok(item),
            _ => Err(ApiError::not_found()),
        }
    }
}
