// SPDX-License-Identifier: Apache-2.0
//! Write a ferrobill provisioning plugin.
//!
//! Implement [`Provisioner`], then `ferrobill_plugin_sdk::serve(router(my_plugin, secret))`.
//! The SDK checks ferrobill's signature on every call, parses the JSON of the protocol
//! (spec/protocol.md) and turns your [`Error`]s into the status codes ferrobill expects:
//! [`Error::Refused`] is 422 (the job fails, no retry), [`Error::Retry`] is 503 (ferrobill tries
//! again later).
//!
//! [`client::Client`] calls a plugin the way ferrobill does, for tests and the
//! `ferrobill-plugin-cli` tool.

pub mod client;
pub mod registrar;
pub mod signature;
pub mod types;
pub mod util;

use std::future::Future;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Map, Value, json};

pub use types::*;

/// Why a job or action didn't work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// It can never work as asked (bad settings, a name taken by someone else): answered 422,
    /// ferrobill fails the job without retrying and shows the reason.
    Refused(String),
    /// It may work later (the server is down, a timeout): answered 503, retried.
    Retry(String),
}

impl Error {
    pub fn refused(detail: impl Into<String>) -> Self {
        Error::Refused(detail.into())
    }
    pub fn retry(detail: impl Into<String>) -> Self {
        Error::Retry(detail.into())
    }
    pub fn detail(&self) -> &str {
        match self {
            Error::Refused(d) | Error::Retry(d) => d,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Refused(d) => write!(f, "refused: {d}"),
            Error::Retry(d) => write!(f, "retry: {d}"),
        }
    }
}

impl std::error::Error for Error {}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Error::Refused(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Error::Retry(_) => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, Json(json!({"detail": self.detail()}))).into_response()
    }
}

/// A provisioning plugin.
///
/// Each job may arrive more than once (ferrobill timed out after the work was done), so make
/// every action idempotent: `create` for an account that already exists for this service is
/// done, `terminate` for one already gone is done.
pub trait Provisioner: Send + Sync + 'static {
    fn manifest(&self) -> Manifest;

    /// Run a job. `Ok(data)`: done; `data` (an object, may be empty) is merged into the service's
    /// provisioning data, which later jobs get back as `service.provisioning`.
    fn provision(&self, job: Job)
    -> impl Future<Output = Result<Map<String, Value>, Error>> + Send;

    /// Run an action from the manifest's `actions`.
    fn action(
        &self,
        request: ActionRequest,
    ) -> impl Future<Output = Result<ActionAnswer, Error>> + Send {
        async move {
            Err(Error::refused(format!(
                "this plugin has no action {:?}",
                request.action
            )))
        }
    }
}

struct Shared<P> {
    plugin: P,
    secret: String,
}

/// The plugin's HTTP routes: `GET /manifest`, `POST /provision`, `POST /action` (all signed),
/// and `GET /healthz` (not signed, for liveness probes).
pub fn router<P: Provisioner>(plugin: P, secret: impl Into<String>) -> Router {
    let shared = Arc::new(Shared {
        plugin,
        secret: secret.into(),
    });
    Router::new()
        .route("/manifest", get(manifest::<P>))
        .route("/provision", post(provision::<P>))
        .route("/action", post(action::<P>))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(shared)
}

pub(crate) fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"detail": "missing or wrong ferrobill-signature"})),
    )
        .into_response()
}

pub(crate) fn signed(secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    headers
        .get(signature::HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| signature::verify(secret, h, body, signature::now()))
}

async fn manifest<P: Provisioner>(State(s): State<Arc<Shared<P>>>, headers: HeaderMap) -> Response {
    if !signed(&s.secret, &headers, b"") {
        return unauthorized();
    }
    Json(s.plugin.manifest()).into_response()
}

async fn provision<P: Provisioner>(
    State(s): State<Arc<Shared<P>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !signed(&s.secret, &headers, &body) {
        return unauthorized();
    }
    let job: Job = match serde_json::from_slice(&body) {
        Ok(j) => j,
        Err(e) => return Error::refused(format!("the job is not valid JSON: {e}")).into_response(),
    };
    if job.action == Action::Unknown {
        return Error::refused("this plugin does not know that action").into_response();
    }
    let (job_id, action, service) = (job.job_id.clone(), job.action, job.service.id.clone());
    match s.plugin.provision(job).await {
        Ok(data) => {
            tracing::info!(%job_id, action = action.as_str(), %service, "done");
            Json(json!({"data": data})).into_response()
        }
        Err(e) => {
            tracing::warn!(%job_id, action = action.as_str(), %service, error = %e, "not done");
            e.into_response()
        }
    }
}

async fn action<P: Provisioner>(
    State(s): State<Arc<Shared<P>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !signed(&s.secret, &headers, &body) {
        return unauthorized();
    }
    let req: ActionRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return Error::refused(format!("the request is not valid JSON: {e}")).into_response();
        }
    };
    let (name, service) = (req.action.clone(), req.service.id.clone());
    match s.plugin.action(req).await {
        Ok(answer) => {
            tracing::info!(action = %name, %service, "action done");
            Json(answer).into_response()
        }
        Err(e) => {
            tracing::warn!(action = %name, %service, error = %e, "action not done");
            e.into_response()
        }
    }
}

/// A required setting from the environment.
pub fn env(name: &str) -> anyhow::Result<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("{name} is not set"))
}

/// Log as JSON lines (level from `RUST_LOG`, default info), then serve `router` on `LISTEN`
/// (default `0.0.0.0:8080`) until SIGTERM or Ctrl-C.
pub async fn serve(router: Router) -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let listen = std::env::var("LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(%listen, "listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = ctrl_c.await;
            }
        })
        .await?;
    Ok(())
}

/// An HTTP client for talking to the panel or API a plugin drives: rustls (ring), a timeout,
/// no redirects followed.
pub fn http_client(timeout: std::time::Duration) -> anyhow::Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("ferrobill-plugin/", env!("CARGO_PKG_VERSION")))
        .build()?)
}
