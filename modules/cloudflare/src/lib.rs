// SPDX-License-Identifier: Apache-2.0
//! Cloudflare Registrar plugin for ferrobill: the account's domains (`domains`) and one domain's
//! expiry date (`info`) from Cloudflare's Registrar API. See README.md.
//!
//! Cloudflare renews its domains itself, from the account's payment method, when a domain's
//! auto-renew is on; its API has no renewal call. So `renew` is refused, and ferrobill uses this
//! plugin only to list domains and keep their expiry dates current.
//!
//! API (Registrations, which replaced the Registrar Domains endpoints):
//! - list: `GET /accounts/{account_id}/registrar/registrations?per_page=50&cursor=...`
//!   <https://developers.cloudflare.com/api/resources/registrar/subresources/registrations/methods/list/>
//! - one: `GET /accounts/{account_id}/registrar/registrations/{domain_name}`
//!   <https://developers.cloudflare.com/api/resources/registrar/subresources/registrations/methods/get/>

use ferrobill_plugin_sdk::Error;
use ferrobill_plugin_sdk::registrar::{
    DomainInfo, DomainsPage, DomainsRequest, InfoRequest, ListedDomain, Registrar, RenewAnswer,
    RenewRequest,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Cloudflare's API.
pub const API: &str = "https://api.cloudflare.com/client/v4";
/// The most the list call answers in one page.
const PER_PAGE: u32 = 50;

/// How the plugin reaches Cloudflare.
#[derive(Clone)]
pub struct Config {
    /// The API's base URL ([`API`]).
    pub url: String,
    /// An API token that can read the account's Registrar domains.
    pub token: String,
    pub account_id: String,
}

impl Config {
    /// From the environment: CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID; CLOUDFLARE_URL
    /// optional (default [`API`]).
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            url: ferrobill_plugin_sdk::env("CLOUDFLARE_URL").unwrap_or_else(|_| API.to_owned()),
            token: ferrobill_plugin_sdk::env("CLOUDFLARE_API_TOKEN")?,
            account_id: ferrobill_plugin_sdk::env("CLOUDFLARE_ACCOUNT_ID")?,
        })
    }
}

pub struct Cloudflare {
    config: Config,
    http: reqwest::Client,
}

/// A registration, as the API answers it (only the fields the plugin uses).
#[derive(Debug, Deserialize)]
struct Registration {
    domain_name: String,
    /// RFC 3339; null only while a registration or transfer is pending.
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ResultInfo {
    /// The next page's cursor; empty on the last page.
    #[serde(default)]
    cursor: Option<String>,
}

/// Cloudflare's answer envelope.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<Message>,
    result: Option<T>,
    #[serde(default)]
    result_info: Option<ResultInfo>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: String,
}

/// Cloudflare's error messages, for the reason shown in ferrobill.
fn messages(errors: &[Message]) -> String {
    let all: Vec<String> = errors
        .iter()
        .map(|m| match m.code {
            Some(c) => format!("{} ({c})", m.message),
            None => m.message.clone(),
        })
        .collect();
    if all.is_empty() {
        "no reason given".into()
    } else {
        all.join("; ")
    }
}

/// `2027-02-02T10:00:00Z` as `2027-02-02`.
fn day(expires_at: &str) -> Option<String> {
    let d = expires_at.get(..10)?;
    let b = d.as_bytes();
    let ok = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    ok.then(|| d.to_owned())
}

/// A domain name safe to put in the URL path: letters, digits, `-` and `.`, with a dot.
fn checked_name(name: &str) -> Result<String, Error> {
    let n = name.trim().trim_end_matches('.').to_ascii_lowercase();
    let ok = !n.is_empty()
        && n.len() <= 253
        && n.contains('.')
        && !n.starts_with('.')
        && !n.contains("..")
        && n.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.');
    if ok {
        Ok(n)
    } else {
        Err(Error::refused(format!("{name:?} is not a domain name")))
    }
}

impl Cloudflare {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            config,
            http: ferrobill_plugin_sdk::http_client(std::time::Duration::from_secs(30))?,
        })
    }

    /// GET `path` (under the account's `registrar/`) and unwrap Cloudflare's envelope. Network
    /// errors, 429 and 5xx: retry. Any other failure (4xx, `success: false`): refused, with
    /// Cloudflare's messages.
    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<(T, Option<ResultInfo>), Error> {
        let url = format!(
            "{}/accounts/{}/registrar/{path}",
            self.config.url.trim_end_matches('/'),
            self.config.account_id
        );
        let mut url = reqwest::Url::parse(&url)
            .map_err(|e| Error::refused(format!("CLOUDFLARE_URL: {e}")))?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let res = self
            .http
            .get(url)
            .bearer_auth(&self.config.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Error::retry(format!("Cloudflare: {e}")))?;
        let status = res.status();
        let body = res
            .bytes()
            .await
            .map_err(|e| Error::retry(format!("Cloudflare: {e}")))?;
        let parsed: Option<Envelope<T>> = serde_json::from_slice(&body).ok();
        let why = || {
            parsed
                .as_ref()
                .map(|e| messages(&e.errors))
                .unwrap_or_else(|| "no reason given".into())
        };
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(Error::retry(format!(
                "Cloudflare: HTTP {status}: {}",
                why()
            )));
        }
        if !status.is_success() {
            return Err(Error::refused(format!(
                "Cloudflare: HTTP {status}: {}",
                why()
            )));
        }
        let Some(env) = parsed else {
            return Err(Error::retry(format!(
                "Cloudflare: HTTP {status}, but the answer isn't the API's JSON"
            )));
        };
        if !env.success {
            return Err(Error::refused(format!(
                "Cloudflare: {}",
                messages(&env.errors)
            )));
        }
        match env.result {
            Some(r) => Ok((r, env.result_info)),
            None => Err(Error::retry("Cloudflare: the answer has no result")),
        }
    }
}

impl Registrar for Cloudflare {
    fn manifest(&self) -> Value {
        json!({"name": "cloudflare", "version": env!("CARGO_PKG_VERSION"), "kinds": ["registrar"],
               "registrar_operations": ["domains", "info"]})
    }

    /// One page of the account's registrations (50 a page); Cloudflare's cursor is ferrobill's.
    async fn domains(&self, r: DomainsRequest) -> Result<DomainsPage, Error> {
        let mut query = vec![("per_page", PER_PAGE.to_string())];
        if let Some(c) = r.cursor.filter(|c| !c.is_empty()) {
            query.push(("cursor", c));
        }
        let (list, info): (Vec<Registration>, _) = self.get("registrations", &query).await?;
        let domains = list
            .into_iter()
            .map(|d| ListedDomain {
                name: d.domain_name.to_ascii_lowercase(),
                expires_on: d.expires_at.as_deref().and_then(day),
            })
            .collect();
        let cursor = info.and_then(|i| i.cursor).filter(|c| !c.is_empty());
        Ok(DomainsPage {
            domains,
            more: cursor.is_some(),
            cursor,
        })
    }

    async fn info(&self, r: InfoRequest) -> Result<DomainInfo, Error> {
        let name = checked_name(&r.name)?;
        let (d, _): (Registration, _) = self.get(&format!("registrations/{name}"), &[]).await?;
        let Some(at) = d.expires_at else {
            // Pending registration or transfer in: the date comes once it's done.
            return Err(Error::retry(format!(
                "{name} has no expiry date at Cloudflare yet (a registration or transfer is pending)"
            )));
        };
        let Some(expires_on) = day(&at) else {
            return Err(Error::refused(format!(
                "Cloudflare answered an expiry date for {name} that isn't a date: {at:?}"
            )));
        };
        Ok(DomainInfo {
            expires_on,
            registrar: None,
        })
    }

    async fn renew(&self, r: RenewRequest) -> Result<RenewAnswer, Error> {
        Err(Error::refused(format!(
            "Cloudflare renews {} itself (auto-renew, from the account's payment method); its API \
             has no renewal call. Check auto-renew is on in the Cloudflare dashboard; the new \
             expiry date shows here after Cloudflare renews it",
            r.name
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Path, Query},
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use std::collections::HashMap;

    /// Three registrations, two a page; any token but "good" is refused as Cloudflare does.
    fn fake() -> Router {
        let regs = [
            ("a.com", Some("2027-02-02T10:00:00Z")),
            ("B.dev", Some("2026-12-18T00:00:00Z")),
            ("c.net", None),
        ];
        let authed = |h: &HeaderMap| {
            h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer good")
        };
        let no_auth = || {
            (
                StatusCode::FORBIDDEN,
                Json(json!({"success": false, "result": null, "messages": [],
                    "errors": [{"code": 10000, "message": "Authentication error"}]})),
            )
        };
        let reg = |(n, e): (&str, Option<&str>)| {
            json!({"domain_name": n, "expires_at": e, "auto_renew": true, "locked": true,
                   "privacy_mode": "redaction",
                   "status": if e.is_some() {"active"} else {"transfer_pending"}})
        };
        Router::new()
            .route(
                "/accounts/acc1/registrar/registrations",
                get(move |h: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                    if !authed(&h) {
                        return no_auth();
                    }
                    assert_eq!(q.get("per_page").map(String::as_str), Some("50"));
                    let (items, next) = match q.get("cursor").map(String::as_str) {
                        None => (&regs[..2], "page2"),
                        Some("page2") => (&regs[2..], ""),
                        Some(_) => {
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(json!({"success": false, "result": null,
                                    "errors": [{"code": 10000,
                                                "message": "Invalid pagination cursor"}]})),
                            );
                        }
                    };
                    let items: Vec<Value> = items.iter().copied().map(reg).collect();
                    (
                        StatusCode::OK,
                        Json(json!({"success": true, "errors": [], "messages": [],
                            "result": items,
                            "result_info": {"count": items.len(), "cursor": next, "per_page": 50}})),
                    )
                }),
            )
            .route(
                "/accounts/acc1/registrar/registrations/{name}",
                get(move |h: HeaderMap, Path(name): Path<String>| async move {
                    if !authed(&h) {
                        return no_auth();
                    }
                    match name.as_str() {
                        "busy.com" => (
                            StatusCode::TOO_MANY_REQUESTS,
                            Json(json!({"success": false, "result": null,
                                "errors": [{"code": 971, "message": "Please wait"}]})),
                        ),
                        "broken.com" => (StatusCode::BAD_GATEWAY, Json(json!(null))),
                        "odd.com" => (
                            StatusCode::OK,
                            Json(json!({"success": false, "result": null,
                                "errors": [{"code": 1234, "message": "Something odd"}]})),
                        ),
                        _ => match regs.iter().find(|r| r.0.eq_ignore_ascii_case(&name)) {
                            Some(r) => (
                                StatusCode::OK,
                                Json(json!({"success": true, "errors": [], "messages": [],
                                    "result": reg(*r)})),
                            ),
                            None => (
                                StatusCode::NOT_FOUND,
                                Json(json!({"success": false, "result": null, "messages": [],
                                    "errors": [{"code": 10000, "message": "Domain not found"}]})),
                            ),
                        },
                    }
                }),
            )
    }

    async fn plugin(token: &str) -> Cloudflare {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, fake()).await.unwrap() });
        Cloudflare::new(Config {
            url,
            token: token.into(),
            account_id: "acc1".into(),
        })
        .unwrap()
    }

    fn info_req(name: &str) -> InfoRequest {
        InfoRequest { name: name.into() }
    }

    #[tokio::test]
    async fn domains_are_listed_a_page_at_a_time() {
        let p = plugin("good").await;
        let first = p.domains(DomainsRequest::default()).await.unwrap();
        assert_eq!(
            first.domains,
            [
                ListedDomain {
                    name: "a.com".into(),
                    expires_on: Some("2027-02-02".into())
                },
                ListedDomain {
                    name: "b.dev".into(),
                    expires_on: Some("2026-12-18".into())
                }
            ]
        );
        assert!(first.more);
        assert_eq!(first.cursor.as_deref(), Some("page2"));
        let second = p
            .domains(DomainsRequest {
                cursor: first.cursor,
            })
            .await
            .unwrap();
        assert_eq!(
            second.domains,
            [ListedDomain {
                name: "c.net".into(),
                expires_on: None
            }]
        );
        assert!(!second.more);
        assert_eq!(second.cursor, None);
        let e = p
            .domains(DomainsRequest {
                cursor: Some("junk".into()),
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");
        assert!(e.detail().contains("Invalid pagination cursor"), "{e}");
    }

    #[tokio::test]
    async fn info_not_found_and_pending() {
        let p = plugin("good").await;
        let i = p.info(info_req("A.com.")).await.unwrap();
        assert_eq!(i.expires_on, "2027-02-02");
        let e = p.info(info_req("nope.com")).await.unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");
        assert!(e.detail().contains("Domain not found"), "{e}");
        // A transfer in that hasn't finished: no date yet, ask again later.
        assert!(matches!(
            p.info(info_req("c.net")).await,
            Err(Error::Retry(_))
        ));
        // Never sent to Cloudflare.
        assert!(matches!(
            p.info(info_req("../x/y.com")).await,
            Err(Error::Refused(_))
        ));
    }

    #[tokio::test]
    async fn errors_map_to_retry_or_refused() {
        let p = plugin("good").await;
        let e = p.info(info_req("busy.com")).await.unwrap_err();
        assert!(matches!(e, Error::Retry(_)), "{e}");
        assert!(e.detail().contains("Please wait"), "{e}");
        assert!(matches!(
            p.info(info_req("broken.com")).await,
            Err(Error::Retry(_))
        ));
        let e = p.info(info_req("odd.com")).await.unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");
        assert!(e.detail().contains("Something odd (1234)"), "{e}");
        // A token without Registrar read.
        let bad = plugin("bad").await;
        let e = bad.domains(DomainsRequest::default()).await.unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");
        assert!(e.detail().contains("Authentication error"), "{e}");
        // Nothing listening.
        let gone = Cloudflare::new(Config {
            url: "http://127.0.0.1:1".into(),
            token: "good".into(),
            account_id: "acc1".into(),
        })
        .unwrap();
        assert!(matches!(
            gone.info(info_req("a.com")).await,
            Err(Error::Retry(_))
        ));
    }

    #[tokio::test]
    async fn renew_is_refused_and_not_offered() {
        let p = plugin("good").await;
        assert_eq!(p.manifest()["name"], "cloudflare");
        assert_eq!(p.manifest()["kinds"], json!(["registrar"]));
        assert_eq!(
            p.manifest()["registrar_operations"],
            json!(["domains", "info"])
        );
        let e = p
            .renew(RenewRequest {
                name: "a.com".into(),
                years: 1,
                expires_on: "2027-02-02".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");
        assert!(e.detail().contains("renews a.com itself"), "{e}");
    }

    #[test]
    fn dates_and_names() {
        assert_eq!(day("2027-02-02T10:00:00Z").as_deref(), Some("2027-02-02"));
        assert_eq!(day("2027-02-02").as_deref(), Some("2027-02-02"));
        assert_eq!(day("02-Feb-2027"), None);
        assert_eq!(day(""), None);
        assert_eq!(checked_name("Example.COM.").unwrap(), "example.com");
        for bad in [
            "",
            "com",
            ".a.com",
            "a..com",
            "a.com/x",
            "a.com?x=1",
            "a b.com",
        ] {
            assert!(checked_name(bad).is_err(), "{bad}");
        }
    }

    /// Read-only, against the real API (GETs only):
    /// `CLOUDFLARE_ACCOUNT_ID=... CLOUDFLARE_API_TOKEN=... cargo test -p ferrobill-cloudflare
    /// live -- --ignored --nocapture`. Lists every page, then reads the first domain's date.
    #[tokio::test]
    #[ignore]
    async fn live() {
        let p = Cloudflare::new(Config {
            url: API.into(),
            token: std::env::var("CLOUDFLARE_API_TOKEN").expect("CLOUDFLARE_API_TOKEN"),
            account_id: std::env::var("CLOUDFLARE_ACCOUNT_ID").expect("CLOUDFLARE_ACCOUNT_ID"),
        })
        .unwrap();
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let page = p.domains(DomainsRequest { cursor }).await.expect("list");
            all.extend(page.domains);
            if !page.more {
                break;
            }
            cursor = page.cursor;
        }
        println!("{} domains", all.len());
        if let Some(d) = all.first() {
            let i = p.info(info_req(&d.name)).await.expect("info");
            println!("{}: expires {}", d.name, i.expires_on);
            assert_eq!(Some(i.expires_on), d.expires_on);
        }
    }
}
