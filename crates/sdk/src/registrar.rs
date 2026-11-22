// SPDX-License-Identifier: Apache-2.0
//! Registrar plugins (spec/protocol.md, "Registrars"): the domains at a registrar, one domain's
//! expiry date, and renewals, for ferrobill's domains (its docs/domains.md).
//!
//! Implement [`Registrar`] and serve [`router`]: every call is checked against the shared secret
//! (`ferrobill-signature`) before it reaches the plugin, as for provisioning plugins.

use std::future::Future;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;

/// One domain in a registrar's listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedDomain {
    pub name: String,
    /// `YYYY-MM-DD`, if the listing has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<String>,
}

/// A page of the listing (`POST /domains`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainsPage {
    pub domains: Vec<ListedDomain>,
    pub cursor: Option<String>,
    pub more: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DomainsRequest {
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InfoRequest {
    pub name: String,
}

/// One domain's expiry date at the registrar (`POST /info`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainInfo {
    /// `YYYY-MM-DD`.
    pub expires_on: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registrar: Option<String>,
}

/// A renewal (`POST /renew`): renew `name` for `years` from `expires_on`. Must be safe to
/// repeat: if the domain already expires after `expires_on`, it was renewed; answer its date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewRequest {
    pub name: String,
    pub years: u32,
    /// `YYYY-MM-DD`: the expiry date the renewal is from.
    pub expires_on: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenewAnswer {
    /// `YYYY-MM-DD`: the new expiry date.
    pub expires_on: String,
}

/// A registrar plugin. `manifest` must list `"registrar"` in `kinds` and the operations it
/// implements in `registrar_operations` (ferrobill calls only those).
pub trait Registrar: Send + Sync + 'static {
    fn manifest(&self) -> Value;

    fn domains(
        &self,
        _request: DomainsRequest,
    ) -> impl Future<Output = Result<DomainsPage, Error>> + Send {
        async { Err(Error::refused("this plugin can't list domains")) }
    }

    fn info(&self, request: InfoRequest) -> impl Future<Output = Result<DomainInfo, Error>> + Send;

    fn renew(
        &self,
        request: RenewRequest,
    ) -> impl Future<Output = Result<RenewAnswer, Error>> + Send;
}

struct Shared<R> {
    plugin: R,
    secret: String,
}

/// The plugin's HTTP routes: `GET /manifest`, `POST /domains`, `/info`, `/renew`, `GET /healthz`.
pub fn router<R: Registrar>(plugin: R, secret: impl Into<String>) -> Router {
    let shared = Arc::new(Shared {
        plugin,
        secret: secret.into(),
    });
    Router::new()
        .route("/manifest", get(manifest::<R>))
        .route("/domains", post(domains::<R>))
        .route("/info", post(info::<R>))
        .route("/renew", post(renew::<R>))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(shared)
}

async fn manifest<R: Registrar>(State(s): State<Arc<Shared<R>>>, headers: HeaderMap) -> Response {
    if !crate::signed(&s.secret, &headers, b"") {
        return crate::unauthorized();
    }
    Json(s.plugin.manifest()).into_response()
}

/// Check the signature and parse the body, or the answer to send instead.
fn parse<T: serde::de::DeserializeOwned>(
    secret: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<T, Box<Response>> {
    if !crate::signed(secret, headers, body) {
        return Err(Box::new(crate::unauthorized()));
    }
    serde_json::from_slice(body).map_err(|e| {
        Box::new(Error::refused(format!("the request is not valid JSON: {e}")).into_response())
    })
}

fn answer<T: Serialize>(op: &str, what: &str, r: Result<T, Error>) -> Response {
    match r {
        Ok(a) => {
            tracing::info!(op, %what, "done");
            Json(a).into_response()
        }
        Err(e) => {
            tracing::warn!(op, %what, error = %e, "not done");
            e.into_response()
        }
    }
}

async fn domains<R: Registrar>(
    State(s): State<Arc<Shared<R>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match parse::<DomainsRequest>(&s.secret, &headers, &body) {
        Ok(r) => answer("domains", "listing", s.plugin.domains(r).await),
        Err(resp) => *resp,
    }
}

async fn info<R: Registrar>(
    State(s): State<Arc<Shared<R>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match parse::<InfoRequest>(&s.secret, &headers, &body) {
        Ok(r) => {
            let name = r.name.clone();
            answer("info", &name, s.plugin.info(r).await)
        }
        Err(resp) => *resp,
    }
}

async fn renew<R: Registrar>(
    State(s): State<Arc<Shared<R>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match parse::<RenewRequest>(&s.secret, &headers, &body) {
        Ok(r) => {
            let name = r.name.clone();
            answer("renew", &name, s.plugin.renew(r).await)
        }
        Err(resp) => *resp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature;
    use serde_json::json;

    struct Fake;

    impl Registrar for Fake {
        fn manifest(&self) -> Value {
            json!({"name": "fake", "version": "1", "kinds": ["registrar"],
                   "registrar_operations": ["info", "renew"]})
        }
        async fn info(&self, r: InfoRequest) -> Result<DomainInfo, Error> {
            if r.name == "a.uk" {
                Ok(DomainInfo {
                    expires_on: "2027-02-02".into(),
                    registrar: None,
                })
            } else {
                Err(Error::refused("no such domain"))
            }
        }
        async fn renew(&self, r: RenewRequest) -> Result<RenewAnswer, Error> {
            Ok(RenewAnswer {
                expires_on: format!("{}-02-02", 2027 + r.years),
            })
        }
    }

    #[tokio::test]
    async fn calls_are_signed_routed_and_answered() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, router(Fake, "s3cret")).await.unwrap() });
        let http = crate::http_client(std::time::Duration::from_secs(5)).unwrap();
        let post = |path: &str, body: Value, secret: &str| {
            let b = body.to_string();
            let sig = signature::sign(secret, signature::now(), b.as_bytes());
            http.post(format!("{url}{path}"))
                .header(signature::HEADER, sig)
                .header("content-type", "application/json")
                .body(b)
                .send()
        };
        let r = post("/info", json!({"name": "a.uk"}), "wrong")
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        let r = post("/info", json!({"name": "a.uk"}), "s3cret")
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.json::<Value>().await.unwrap()["expires_on"], "2027-02-02");
        let r = post("/info", json!({"name": "b.uk"}), "s3cret")
            .await
            .unwrap();
        assert_eq!(r.status(), 422);
        let r = post(
            "/renew",
            json!({"name": "a.uk", "years": 2, "expires_on": "2027-02-02"}),
            "s3cret",
        )
        .await
        .unwrap();
        assert_eq!(r.json::<Value>().await.unwrap()["expires_on"], "2029-02-02");
        // Not implemented: refused.
        let r = post("/domains", json!({}), "s3cret").await.unwrap();
        assert_eq!(r.status(), 422);
    }
}
