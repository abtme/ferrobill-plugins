// SPDX-License-Identifier: Apache-2.0
//! Domain Name API (domainnameapi.com reseller) registrar plugin for ferrobill: the account's
//! domains (`domains`), a domain's expiry date (`info`) and renewals (`renew`) over the
//! reseller REST API ("Public Gateway API", `https://api.domainresellerapi.com/api/v1`). See
//! README.md.
//!
//! `GET /domains/info` also answers the domain's contacts (names, addresses, ...): the plugin
//! reads only the expiry date from it and never logs or returns anything else.

use std::time::Duration;

use ferrobill_plugin_sdk::Error;
use ferrobill_plugin_sdk::registrar::{
    DomainInfo, DomainsPage, DomainsRequest, InfoRequest, ListedDomain, Registrar, RenewAnswer,
    RenewRequest,
};
use reqwest::{RequestBuilder, StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// The API: live and the test system (OTE).
pub const LIVE: &str = "https://api.domainresellerapi.com/api/v1";
pub const OTE: &str = "https://ote.domainresellerapi.com/api/v1";
/// Domains per page of the listing.
const PAGE: usize = 200;
/// Reads of the expiry date after a renewal, `settle` apart, before giving up for now.
const CHECKS: u32 = 3;

/// How the plugin reaches the API.
#[derive(Clone)]
pub struct Config {
    /// Base URL, up to and including `/api/v1` ([`LIVE`] or [`OTE`]).
    pub url: String,
    /// The reseller id (a UUID): the `__reseller` header.
    pub reseller_id: String,
    /// The API key: the `X-API-KEY` header.
    pub api_key: String,
    /// The wait between reads of the expiry date after a renewal.
    pub settle: Duration,
}

impl Config {
    /// From the environment: DNA_API_KEY, DNA_RESELLER_ID; DNA_URL optional (`live`, the
    /// default, `ote`, or a base URL).
    pub fn from_env() -> anyhow::Result<Self> {
        let url = match std::env::var("DNA_URL").unwrap_or_default().trim() {
            "" | "live" => LIVE.to_owned(),
            "ote" => OTE.to_owned(),
            other => other.trim_end_matches('/').to_owned(),
        };
        Ok(Self {
            url,
            reseller_id: ferrobill_plugin_sdk::env("DNA_RESELLER_ID")?,
            api_key: ferrobill_plugin_sdk::env("DNA_API_KEY")?,
            settle: Duration::from_secs(3),
        })
    }
}

pub struct DomainNameApi {
    config: Config,
    http: reqwest::Client,
}

/// A page of `GET /domains` (only what the plugin uses).
#[derive(Deserialize)]
struct DomainList {
    #[serde(default)]
    items: Option<Vec<DomainItem>>,
    #[serde(rename = "totalCount", default)]
    total_count: Option<u64>,
}

#[derive(Deserialize)]
struct DomainItem {
    #[serde(rename = "domainName", default)]
    domain_name: Option<String>,
    #[serde(rename = "expirationDate", default)]
    expiration_date: Option<String>,
}

/// `GET /domains/info`: only the expiry date. Everything else (contacts) is never read.
#[derive(Deserialize)]
struct Info {
    #[serde(rename = "expirationDate", default)]
    expiration_date: Option<String>,
}

/// `POST /domains/renew`'s answer.
#[derive(Deserialize)]
struct RenewResult {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    message: Option<String>,
    #[serde(rename = "expirationDate", default)]
    expiration_date: Option<String>,
}

impl DomainNameApi {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            config,
            http: ferrobill_plugin_sdk::http_client(Duration::from_secs(60))?,
        })
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url, Error> {
        let mut url = Url::parse(&format!("{}/{path}", self.config.url.trim_end_matches('/')))
            .map_err(|e| Error::refused(format!("DNA_URL: {e}")))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        what: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, Error> {
        let url = self.url(path, query)?;
        self.send(what, self.http.get(url)).await
    }

    async fn post<T: DeserializeOwned>(
        &self,
        what: &str,
        path: &str,
        body: &Value,
    ) -> Result<T, Error> {
        let url = self.url(path, &[])?;
        self.send(what, self.http.post(url).json(body)).await
    }

    /// Send with the API's headers; map the answer to the plugin protocol's errors: no answer,
    /// 5xx, 408 and 429 are retried; any other failure is refused with the API's message.
    async fn send<T: DeserializeOwned>(&self, what: &str, req: RequestBuilder) -> Result<T, Error> {
        let res = req
            .header("X-API-KEY", &self.config.api_key)
            .header("__reseller", &self.config.reseller_id)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Error::retry(format!("{what}: {}", e.without_url())))?;
        let status = res.status();
        let body = res
            .bytes()
            .await
            .map_err(|e| Error::retry(format!("{what}: {}", e.without_url())))?;
        if !status.is_success() {
            let detail = match api_message(&body) {
                Some(m) => format!("{what}: HTTP {}: {m}", status.as_u16()),
                None => format!("{what}: HTTP {status}"),
            };
            return Err(if retryable(status) {
                Error::retry(detail)
            } else {
                Error::refused(detail)
            });
        }
        // The error never quotes the body: info's answer holds contact details.
        serde_json::from_slice(&body).map_err(|e| {
            Error::retry(format!(
                "{what}: unexpected answer from the API ({:?} at line {}, column {})",
                e.classify(),
                e.line(),
                e.column()
            ))
        })
    }

    /// The domain's expiry date at the registrar.
    async fn expiry(&self, name: &str) -> Result<String, Error> {
        let info: Info = self
            .get("domains/info", "domains/info", &[("DomainName", name)])
            .await?;
        info.expiration_date
            .as_deref()
            .and_then(day)
            .ok_or_else(|| Error::retry(format!("{name}: the API answered no expiry date")))
    }
}

fn retryable(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
}

/// The message in an error answer: `{"error": {"message", "details", "validationErrors"}}` (the
/// gateway's usual shape) or a flat `{"message"}`; a short non-JSON body as it is.
fn api_message(body: &[u8]) -> Option<String> {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        let e = if v["error"].is_object() {
            &v["error"]
        } else {
            &v
        };
        let mut parts: Vec<String> = Vec::new();
        for key in ["message", "details"] {
            if let Some(s) = e[key].as_str().map(str::trim).filter(|s| !s.is_empty())
                && !parts.iter().any(|p| p == s)
            {
                parts.push(s.to_owned());
            }
        }
        for ve in e["validationErrors"].as_array().into_iter().flatten() {
            if let Some(s) = ve["message"].as_str().filter(|s| !s.is_empty()) {
                parts.push(s.to_owned());
            }
        }
        return (!parts.is_empty()).then(|| parts.join(": "));
    }
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    (!text.is_empty() && text.len() <= 200 && !text.starts_with('<')).then(|| text.to_owned())
}

/// `2027-11-09T12:00:00` (or a bare date) as `2027-11-09`; `None` for anything else, and for
/// .NET's empty date (`0001-01-01`).
fn day(s: &str) -> Option<String> {
    let d = s.trim().get(..10)?;
    let b = d.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..4) && b[4] == b'-' && digits(5..7) && b[7] == b'-' && digits(8..10)) {
        return None;
    }
    let month: u32 = d[5..7].parse().ok()?;
    let dom: u32 = d[8..10].parse().ok()?;
    if d < "1900" || !(1..=12).contains(&month) || !(1..=31).contains(&dom) {
        return None;
    }
    Some(d.to_owned())
}

/// A domain name as the API expects it: trimmed, lower case, not empty.
fn domain_name(name: &str) -> Result<String, Error> {
    let n = name.trim().trim_end_matches('.').to_ascii_lowercase();
    if n.is_empty() || !n.contains('.') || n.contains(char::is_whitespace) {
        return Err(Error::refused(format!("{name:?} is not a domain name")));
    }
    Ok(n)
}

impl Registrar for DomainNameApi {
    fn manifest(&self) -> Value {
        json!({"name": "domainnameapi", "version": env!("CARGO_PKG_VERSION"),
               "kinds": ["registrar"], "registrar_operations": ["domains", "info", "renew"]})
    }

    /// One page of up to 200 domains; the cursor is the number already listed (`SkipCount`).
    async fn domains(&self, r: DomainsRequest) -> Result<DomainsPage, Error> {
        let skip: usize = match r.cursor.as_deref() {
            None | Some("") => 0,
            Some(c) => c
                .parse()
                .map_err(|_| Error::refused(format!("cursor {c:?} is not from this plugin")))?,
        };
        let (page, skip_s) = (PAGE.to_string(), skip.to_string());
        let list: DomainList = self
            .get(
                "domains",
                "domains",
                &[("MaxResultCount", &page), ("SkipCount", &skip_s)],
            )
            .await?;
        let items = list.items.unwrap_or_default();
        let got = items.len();
        let domains = items
            .into_iter()
            .filter_map(|d| {
                let name = d.domain_name?.trim().to_ascii_lowercase();
                (!name.is_empty()).then(|| ListedDomain {
                    name,
                    expires_on: d.expiration_date.as_deref().and_then(day),
                })
            })
            .collect();
        let next = skip + got;
        let more = got > 0
            && match list.total_count {
                Some(total) => (next as u64) < total,
                None => got == PAGE,
            };
        Ok(DomainsPage {
            domains,
            cursor: more.then(|| next.to_string()),
            more,
        })
    }

    async fn info(&self, r: InfoRequest) -> Result<DomainInfo, Error> {
        let name = domain_name(&r.name)?;
        Ok(DomainInfo {
            expires_on: self.expiry(&name).await?,
            registrar: Some("Domain Name API".into()),
        })
    }

    /// Safe to repeat: the expiry date is read first. Already later than the date asked about:
    /// it was renewed (a retry after a lost answer, or renewed elsewhere), so that date is the
    /// answer. Earlier: refused (ferrobill's date is wrong; check before renewing). The same:
    /// renewed, then the date is read again until it has moved.
    async fn renew(&self, r: RenewRequest) -> Result<RenewAnswer, Error> {
        let name = domain_name(&r.name)?;
        if !(1..=10).contains(&r.years) {
            return Err(Error::refused("years: 1 to 10"));
        }
        let from = day(&r.expires_on)
            .filter(|d| d.len() == r.expires_on.trim().len())
            .ok_or_else(|| {
                Error::refused(format!("expires_on {:?} is not YYYY-MM-DD", r.expires_on))
            })?;
        let current = self.expiry(&name).await?;
        if current > from {
            return Ok(RenewAnswer {
                expires_on: current,
            });
        }
        if current < from {
            return Err(Error::refused(format!(
                "{name} expires on {current} at Domain Name API, not {from}: check the domain \
                 before renewing"
            )));
        }
        let done: RenewResult = self
            .post(
                "domains/renew",
                "domains/renew",
                &json!({"domainName": name, "period": r.years}),
            )
            .await?;
        if !done.success {
            return Err(Error::refused(format!(
                "domains/renew: {}",
                done.message
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or_else(|| "the API did not renew the domain".into())
            )));
        }
        let reported = done
            .expiration_date
            .as_deref()
            .and_then(day)
            .filter(|d| *d > from);
        for i in 0..CHECKS {
            if i > 0 {
                tokio::time::sleep(self.config.settle).await;
            }
            match self.expiry(&name).await {
                Ok(now) if now > from => return Ok(RenewAnswer { expires_on: now }),
                _ => {}
            }
        }
        // The API said it renewed, with the new date, but its info doesn't show it yet: its
        // own answer is the date (asking again would renew twice while info lags).
        if let Some(expires_on) = reported {
            return Ok(RenewAnswer { expires_on });
        }
        Err(Error::retry(format!(
            "{name}: renewed, but the API still shows it expiring on {from}; ask again later"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};

    use axum::{
        Json, Router,
        extract::{Query, State},
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::{get, post},
    };

    const KEY: &str = "test-key";
    const RESELLER: &str = "00000000-0000-4000-8000-000000000001";

    #[derive(Default)]
    struct Fake {
        /// Domain -> expiry date (YYYY-MM-DD).
        domains: BTreeMap<String, String>,
        renews: Vec<(String, u32)>,
        /// Leave out totalCount from the listing.
        no_total: bool,
    }

    type Shared = Arc<Mutex<Fake>>;

    fn err(status: StatusCode, message: &str) -> Response {
        (
            status,
            Json(
                json!({"error": {"code": "X", "message": message, "details": null,
                                  "validationErrors": null}}),
            ),
        )
            .into_response()
    }

    fn authorised(h: &HeaderMap) -> bool {
        h.get("x-api-key").is_some_and(|v| v == KEY)
            && h.get("__reseller").is_some_and(|v| v == RESELLER)
    }

    fn plus_years(d: &str, years: u32) -> String {
        let y: u32 = d[..4].parse().unwrap();
        format!("{}{}", y + years, &d[4..])
    }

    async fn list(
        State(s): State<Shared>,
        h: HeaderMap,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        if !authorised(&h) {
            return err(StatusCode::UNAUTHORIZED, "Unauthorized");
        }
        let s = s.lock().unwrap();
        let skip: usize = q["SkipCount"].parse().unwrap();
        let max: usize = q["MaxResultCount"].parse().unwrap();
        let items: Vec<Value> = s
            .domains
            .iter()
            .skip(skip)
            .take(max)
            .map(|(n, e)| {
                json!({"domainName": n, "expirationDate": format!("{e}T12:00:00"),
                       "status": 0, "statusCode": "Active", "renewalMode": "AutoExpire",
                       "nameservers": null, "contacts": {"Registrant": "C1"}})
            })
            .collect();
        if s.no_total {
            Json(json!({"items": items})).into_response()
        } else {
            Json(json!({"items": items, "totalCount": s.domains.len()})).into_response()
        }
    }

    async fn info(
        State(s): State<Shared>,
        h: HeaderMap,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        if !authorised(&h) {
            return err(StatusCode::UNAUTHORIZED, "Unauthorized");
        }
        let name = q.get("DomainName").cloned().unwrap_or_default();
        match name.as_str() {
            "busy.com" => return StatusCode::TOO_MANY_REQUESTS.into_response(),
            "down.com" => return err(StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
            "garbled.com" => return "Private Person, 1 Secret St".into_response(),
            _ => {}
        }
        let s = s.lock().unwrap();
        let Some(ex) = s.domains.get(&name) else {
            return err(StatusCode::NOT_FOUND, "Domain not found");
        };
        // "lagging.com": info keeps the old date after a renewal.
        let shown = if name == "lagging.com" || name == "silent.com" {
            "2027-01-01".to_owned()
        } else {
            ex.clone()
        };
        Json(json!({
            "domainName": name, "expirationDate": format!("{shown}T12:00:00"),
            "status": "Active", "authCode": "s3cret-auth",
            "contacts": [{"contactType": "Registrant", "firstName": "Private",
                          "lastName": "Person", "eMail": "private@example.com"}]
        }))
        .into_response()
    }

    async fn renew(State(s): State<Shared>, h: HeaderMap, Json(b): Json<Value>) -> Response {
        if !authorised(&h) {
            return err(StatusCode::UNAUTHORIZED, "Unauthorized");
        }
        let name = b["domainName"].as_str().unwrap_or_default().to_owned();
        let period = b["period"].as_u64().unwrap_or(0) as u32;
        let mut s = s.lock().unwrap();
        s.renews.push((name.clone(), period));
        match name.as_str() {
            "poor.com" => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": {"code": "Dna.DepositService:Deposit:10002",
                        "message": "Insufficient balance.",
                        "details": "Your balance is not sufficient for this operation."}})),
                )
                    .into_response();
            }
            "flag.com" => {
                return Json(json!({"success": false,
                    "message": "Domain is not available for renewal",
                    "expirationDate": null}))
                .into_response();
            }
            _ => {}
        }
        let Some(ex) = s.domains.get_mut(&name) else {
            return err(StatusCode::NOT_FOUND, "Domain not found");
        };
        *ex = plus_years(ex, period);
        let reported = if name == "silent.com" {
            Value::Null
        } else {
            json!(format!("{ex}T12:00:00"))
        };
        Json(
            json!({"success": true, "message": "Command completed successfully",
                    "status": "Active", "expirationDate": reported}),
        )
        .into_response()
    }

    async fn api(domains: &[(&str, &str)]) -> (DomainNameApi, Shared) {
        let state: Shared = Arc::new(Mutex::new(Fake {
            domains: domains
                .iter()
                .map(|(n, e)| (n.to_string(), e.to_string()))
                .collect(),
            ..Fake::default()
        }));
        let app = Router::new()
            .route("/api/v1/domains", get(list))
            .route("/api/v1/domains/info", get(info))
            .route("/api/v1/domains/renew", post(renew))
            .with_state(state.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/v1", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (plugin(&url), state)
    }

    fn plugin(url: &str) -> DomainNameApi {
        DomainNameApi::new(Config {
            url: url.into(),
            reseller_id: RESELLER.into(),
            api_key: KEY.into(),
            settle: Duration::from_millis(5),
        })
        .unwrap()
    }

    fn renew_req(name: &str, from: &str, years: u32) -> RenewRequest {
        RenewRequest {
            name: name.into(),
            years,
            expires_on: from.into(),
        }
    }

    fn info_req(name: &str) -> InfoRequest {
        InfoRequest { name: name.into() }
    }

    fn renews(s: &Shared) -> usize {
        s.lock().unwrap().renews.len()
    }

    #[tokio::test]
    async fn the_listing_is_paged_with_skip_count() {
        let names: Vec<String> = (0..450).map(|i| format!("d{i:03}.com")).collect();
        let domains: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "2027-03-04")).collect();
        let (p, state) = api(&domains).await;
        for no_total in [false, true] {
            state.lock().unwrap().no_total = no_total;
            let mut all = Vec::new();
            let mut cursor = None;
            let mut pages = 0;
            loop {
                let page = p.domains(DomainsRequest { cursor }).await.unwrap();
                pages += 1;
                all.extend(page.domains);
                if !page.more {
                    assert_eq!(page.cursor, None);
                    break;
                }
                cursor = page.cursor;
            }
            assert_eq!(pages, 3);
            assert_eq!(all.len(), 450);
            assert_eq!(
                all[0],
                ListedDomain {
                    name: "d000.com".into(),
                    expires_on: Some("2027-03-04".into())
                }
            );
            assert_eq!(all[449].name, "d449.com");
        }
        let e = p
            .domains(DomainsRequest {
                cursor: Some("x".into()),
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");
    }

    #[tokio::test]
    async fn info_answers_only_the_expiry_date() {
        let (p, _) = api(&[("example.com", "2027-11-09")]).await;
        let i = p.info(info_req("Example.COM")).await.unwrap();
        assert_eq!(
            i,
            DomainInfo {
                expires_on: "2027-11-09".into(),
                registrar: Some("Domain Name API".into())
            }
        );
        let out = serde_json::to_string(&i).unwrap();
        assert!(!out.contains("Private") && !out.contains("s3cret"), "{out}");
        let e = p.info(info_req("nope.com")).await.unwrap_err();
        assert_eq!(
            e,
            Error::refused("domains/info: HTTP 404: Domain not found")
        );
        // An answer that isn't the JSON expected: retried, and its text never quoted.
        let e = p.info(info_req("garbled.com")).await.unwrap_err();
        assert!(matches!(e, Error::Retry(_)), "{e}");
        assert!(!e.detail().contains("Private"), "{e}");
        assert!(matches!(
            p.info(info_req("not a domain")).await,
            Err(Error::Refused(_))
        ));
    }

    #[tokio::test]
    async fn a_renewal_is_safe_to_repeat() {
        let (p, state) = api(&[("example.com", "2027-11-09")]).await;
        let a = p
            .renew(renew_req("example.com", "2027-11-09", 2))
            .await
            .unwrap();
        assert_eq!(a.expires_on, "2029-11-09");
        assert_eq!(
            state.lock().unwrap().renews,
            [("example.com".to_owned(), 2)]
        );
        // Asked again (an answer that went missing): already renewed, not renewed twice.
        let again = p
            .renew(renew_req("example.com", "2027-11-09", 2))
            .await
            .unwrap();
        assert_eq!(again, a);
        assert_eq!(renews(&state), 1);
        // A date later than the registrar's: refused, nothing renewed.
        let e = p
            .renew(renew_req("example.com", "2030-11-09", 1))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");
        assert!(e.detail().contains("check the domain"), "{e}");
        assert_eq!(renews(&state), 1);
    }

    #[tokio::test]
    async fn renewals_the_api_refuses_or_that_do_not_show() {
        let (p, state) = api(&[
            ("poor.com", "2027-01-01"),
            ("flag.com", "2027-01-01"),
            ("lagging.com", "2027-01-01"),
            ("silent.com", "2027-01-01"),
        ])
        .await;
        let e = p
            .renew(renew_req("poor.com", "2027-01-01", 1))
            .await
            .unwrap_err();
        assert_eq!(
            e,
            Error::refused(
                "domains/renew: HTTP 400: Insufficient balance.: Your balance is not sufficient \
                 for this operation."
            )
        );
        let e = p
            .renew(renew_req("flag.com", "2027-01-01", 1))
            .await
            .unwrap_err();
        assert_eq!(
            e,
            Error::refused("domains/renew: Domain is not available for renewal")
        );
        // Renewed, info still shows the old date, but the renewal's answer has the new one.
        let a = p
            .renew(renew_req("lagging.com", "2027-01-01", 1))
            .await
            .unwrap();
        assert_eq!(a.expires_on, "2028-01-01");
        // Renewed, and no sign of the new date anywhere: retried later.
        let e = p
            .renew(renew_req("silent.com", "2027-01-01", 1))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Retry(_)), "{e}");
        // Bad requests never reach the API.
        let before = renews(&state);
        for (from, years) in [("2027-01-01", 0), ("2027-01-01", 11), ("1 Jan 2027", 1)] {
            let e = p
                .renew(renew_req("poor.com", from, years))
                .await
                .unwrap_err();
            assert!(matches!(e, Error::Refused(_)), "{e}");
        }
        assert_eq!(renews(&state), before);
    }

    #[tokio::test]
    async fn errors_map_to_retry_or_refused() {
        let (p, state) = api(&[("example.com", "2027-11-09")]).await;
        for name in ["busy.com", "down.com"] {
            let e = p.info(info_req(name)).await.unwrap_err();
            assert!(matches!(e, Error::Retry(_)), "{name}: {e}");
            // Can't read the date first: nothing renewed.
            let e = p.renew(renew_req(name, "2027-11-09", 1)).await.unwrap_err();
            assert!(matches!(e, Error::Retry(_)), "{name}: {e}");
        }
        assert_eq!(renews(&state), 0);
        // Wrong credentials: refused with the API's message.
        let mut bad = plugin(&p.config.url);
        bad.config.api_key = "wrong".into();
        let e = bad.info(info_req("example.com")).await.unwrap_err();
        assert_eq!(e, Error::refused("domains/info: HTTP 401: Unauthorized"));
        // Nothing listening: retried.
        let e = plugin("http://127.0.0.1:1/api/v1")
            .info(info_req("example.com"))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Retry(_)), "{e}");
    }

    #[test]
    fn dates_messages_and_manifest() {
        assert_eq!(day("2027-11-09T12:00:00").as_deref(), Some("2027-11-09"));
        assert_eq!(
            day("2027-11-09T23:59:59.123Z").as_deref(),
            Some("2027-11-09")
        );
        assert_eq!(day("2027-11-09").as_deref(), Some("2027-11-09"));
        assert_eq!(day("0001-01-01T00:00:00"), None);
        assert_eq!(day("09/11/2027"), None);
        assert_eq!(day("2027-13-01"), None);
        assert_eq!(day(""), None);
        assert_eq!(
            api_message(br#"{"error":{"message":"A","details":"A"}}"#).as_deref(),
            Some("A")
        );
        assert_eq!(
            api_message(br#"{"error":{"message":"Invalid","validationErrors":[{"message":"Period must be 1-10"}]}}"#)
                .as_deref(),
            Some("Invalid: Period must be 1-10")
        );
        assert_eq!(
            api_message(br#"{"message":"Flat"}"#).as_deref(),
            Some("Flat")
        );
        assert_eq!(api_message(b""), None);
        assert_eq!(api_message(b"<html>gateway</html>"), None);
        assert_eq!(api_message(b"Too busy").as_deref(), Some("Too busy"));
        let m = plugin(LIVE).manifest();
        assert_eq!(m["name"], "domainnameapi");
        assert_eq!(m["kinds"], json!(["registrar"]));
        assert_eq!(
            m["registrar_operations"],
            json!(["domains", "info", "renew"])
        );
    }

    /// Read-only against the live API (never renews: that would cost money):
    /// `DNA_RESELLER_ID=... DNA_API_KEY=... cargo test -p ferrobill-domainnameapi live -- \
    /// --ignored --nocapture`. Lists every domain, then reads info for one and checks the two
    /// agree. Prints counts only.
    #[tokio::test]
    #[ignore]
    async fn live_read_only() {
        let p = DomainNameApi::new(Config::from_env().expect("DNA_* settings")).unwrap();
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let page = p.domains(DomainsRequest { cursor }).await.unwrap();
            all.extend(page.domains);
            if !page.more {
                break;
            }
            cursor = page.cursor;
        }
        let dated = all.iter().filter(|d| d.expires_on.is_some()).count();
        println!("{} domains listed, {dated} with an expiry date", all.len());
        assert!(!all.is_empty());
        let mut checked = 0;
        for d in all.iter().filter(|d| d.expires_on.is_some()).take(3) {
            let i = p.info(info_req(&d.name)).await.unwrap();
            assert_eq!(Some(i.expires_on), d.expires_on, "listing and info differ");
            checked += 1;
        }
        println!("info agrees with the listing for {checked} domains");
        let e = p
            .info(info_req("ferrobill-no-such-domain-7f3a.com"))
            .await
            .unwrap_err();
        println!("a domain not in the account: {e}");
    }
}
