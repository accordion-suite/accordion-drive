//! A Drive module mounted the way the server mounts it, with two users.

// Each test file uses a different part of this module.
#![allow(dead_code)]

use accordion_core::db::Database;
use accordion_core::identity::{AppPasswordUse, AuthMethod, Client, Identity, NewUser};
use accordion_core::storage::Storage;
use accordion_drive::Drive;
use accordion_module::{Module, Services, Stopping};
use accordion_types::{Role, UserId};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use base64ct::{Base64, Encoding};
use bytes::Bytes;
use http_body_util::BodyExt;
use tokio::sync::watch;
use tower::ServiceExt;

pub struct Server {
    _dir: tempfile::TempDir,
    _stop: watch::Sender<bool>,
    pub router: Router,
    pub storage: Storage,
    pub alex: Person,
    pub sam: Person,
}

pub struct Person {
    pub id: UserId,
    /// For the JSON API.
    pub bearer: String,
    /// For WebDAV.
    pub basic: String,
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|err| {
            panic!(
                "{err} in {} reply: {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("accordion.db")).unwrap();
    let identity = Identity::new(db.clone()).await.unwrap();
    let storage = Storage::open(db.clone(), identity.clone(), &dir.path().join("storage"))
        .await
        .unwrap();
    let (stop, stopping) = watch::channel(false);
    let started = Drive
        .start(
            Services {
                db,
                identity: identity.clone(),
                storage: storage.clone(),
            },
            Stopping::new(stopping),
        )
        .await
        .unwrap();
    let router = Router::new()
        .nest("/api/drive", started.routes.api.unwrap())
        .nest_service("/dav/drive", started.routes.dav.unwrap());
    Server {
        alex: person(&identity, "alex").await,
        sam: person(&identity, "sam").await,
        _dir: dir,
        _stop: stop,
        router,
        storage,
    }
}

async fn person(identity: &Identity, username: &str) -> Person {
    let user = identity
        .create_user(NewUser {
            username: username.into(),
            display_name: username.into(),
            role: Role::User,
            password: "correct horse".into(),
        })
        .await
        .unwrap();
    let (token, _) = identity
        .create_session(user.id, Client::App, "Laptop", AuthMethod::Password)
        .await
        .unwrap();
    let (password, _) = identity
        .create_app_password(user.id, AppPasswordUse::Dav, "Laptop")
        .await
        .unwrap();
    Person {
        id: user.id,
        bearer: format!("Bearer {}", token.as_str()),
        basic: format!(
            "Basic {}",
            Base64::encode_string(format!("{username}:{}", password.as_str()).as_bytes())
        ),
    }
}

impl Server {
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: impl Into<Body>,
    ) -> Reply {
        let mut request = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(path);
        for &(name, value) in headers {
            request = request.header(name, value);
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(body.into()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body,
        }
    }

    /// A JSON API request as `person`.
    pub async fn api(
        &self,
        person: &Person,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Reply {
        let path = format!("/api/drive{path}");
        match body {
            Some(json) => {
                self.send(
                    method,
                    &path,
                    &[
                        ("authorization", &person.bearer),
                        ("content-type", "application/json"),
                    ],
                    json.to_string(),
                )
                .await
            }
            None => {
                self.send(
                    method,
                    &path,
                    &[("authorization", &person.bearer)],
                    Body::empty(),
                )
                .await
            }
        }
    }

    /// A WebDAV request as `person`.
    pub async fn dav(
        &self,
        person: &Person,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: impl Into<Body>,
    ) -> Reply {
        let mut all = vec![("authorization", person.basic.as_str())];
        all.extend_from_slice(headers);
        self.send(method, &format!("/dav/drive{path}"), &all, body)
            .await
    }
}
