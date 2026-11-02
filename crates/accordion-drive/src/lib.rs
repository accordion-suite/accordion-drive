//! Accordion Drive: people's files and folders, through a JSON API for
//! Accordion's apps and WebDAV for standard clients, such as File Explorer
//! on Windows and Finder on a Mac.
//!
//! Drive keeps no files of its own. Core's storage service holds them
//! (decision 6 in the `accordion` repo's `docs/decisions`), and Drive is how
//! people reach them. People see their own files and what others shared
//! with them, and core decides what each person may do (decision 16).

mod api;
mod archive;
mod dav;
mod settings;
mod sharing;
mod stars;
mod time;

use std::sync::Arc;

use accordion_core::db::Migration;
use accordion_core::identity::Identity;
use accordion_core::storage::{Access, Item, Storage};
use accordion_core::thumbnails::Thumbnails;
use accordion_module::{
    ApiError, BoxFuture, Module, Routes, Services, StartError, Started, Stopping,
};
use accordion_types::{ItemId, UserId};
use axum::extract::FromRef;
use axum::http::StatusCode;

/// The name Drive's migrations are recorded under.
const OWNER: &str = "drive";

/// Drive's tables, in the order they were added. Each table's module has
/// its own.
static MIGRATIONS: &[Migration] = &[stars::MIGRATION, settings::MIGRATION];

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
            services.db.migrate(OWNER, MIGRATIONS).await?;
            let state = DriveState {
                stars: stars::Stars::new(services.db.clone()),
                settings: settings::Settings::new(services.db),
                identity: services.identity,
                dav: Arc::new(dav::handler(services.storage.clone())),
                storage: services.storage,
                thumbnails: services.thumbnails,
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
    stars: stars::Stars,
    settings: settings::Settings,
    storage: Storage,
    thumbnails: Thumbnails,
    dav: Arc<dav_server::DavHandler<dav::Credentials>>,
}

impl FromRef<DriveState> for Identity {
    fn from_ref(state: &DriveState) -> Self {
        state.identity.clone()
    }
}

impl DriveState {
    /// An item that `user` may use at `needed` or above, with what they may
    /// do. What nobody shared with them is "not found", so nobody learns
    /// what other people have.
    async fn reach(
        &self,
        user: UserId,
        id: ItemId,
        needed: Access,
    ) -> Result<(Item, Access), ApiError> {
        let access = self
            .storage
            .access(user, id)
            .await?
            .ok_or_else(ApiError::not_found)?;
        if access < needed {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                match needed {
                    Access::Owner => "Only the owner can do that.",
                    _ => "You can view this, but not change it. Ask its owner for edit access.",
                },
            ));
        }
        let item = self
            .storage
            .item(id)
            .await?
            .ok_or_else(ApiError::not_found)?;
        Ok((item, access))
    }
}
