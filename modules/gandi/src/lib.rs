// SPDX-License-Identifier: Apache-2.0
//! Gandi registrar plugin for ferrobill: the organization's domains (`domains`), a domain's
//! expiry date (`info`) and renewals (`renew`) over Gandi's v5 API
//! (<https://api.gandi.net/docs/domains/>). See README.md.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ferrobill_plugin_sdk::Error;
use ferrobill_plugin_sdk::registrar::{
    DomainInfo, DomainsPage, DomainsRequest, InfoRequest, ListedDomain, Registrar, RenewAnswer,
    RenewRequest,
};
use reqwest::{RequestBuilder, StatusCode};
use serde_json::{Value, json};

/// Gandi's v5 API: live and sandbox (<https://api.gandi.net/docs/reference/>).
pub const LIVE: &str = "https://api.gandi.net/v5";
pub const SANDBOX: &str = "https://api.sandbox.gandi.net/v5";

/// How the plugin reaches Gandi.
#[derive(Clone)]
pub struct Config {
    /// The API base, ending in `/v5` ([`LIVE`] or [`SANDBOX`]).
    pub url: String,
    /// A personal access token (sent as `Authorization: Bearer <token>`), or a whole
    /// `Authorization` value with its scheme, such as `Apikey <key>` for an old API key.
    pub token: String,
    /// The organization that pays for renewals and whose domains are listed (none: the
    /// token's default).
    pub sharing_id: Option<String>,
    /// Domains per page of the listing.
    pub per_page: u32,
    /// After a renewal is accepted: how many times to read the expiry date again, and how long
    /// to wait before each, for it to move.
    pub polls: u32,
    pub poll_every: Duration,
    /// How long an accepted renewal whose date hasn't moved yet blocks sending another for the
    /// same domain and date (answered "retry" instead).
    pub pending_for: Duration,
}

impl Config {
    /// From the environment: GANDI_TOKEN; GANDI_URL (`live`, the default, `sandbox`, or an API
    /// base URL) and GANDI_SHARING_ID optional.
    pub fn from_env() -> anyhow::Result<Self> {
        let url = match std::env::var("GANDI_URL").unwrap_or_default().trim() {
            "" | "live" => LIVE.to_owned(),
            "sandbox" => SANDBOX.to_owned(),
            other => other.trim_end_matches('/').to_owned(),
        };
        let sharing_id = ferrobill_plugin_sdk::env("GANDI_SHARING_ID").ok();
        if let Some(id) = &sharing_id
            && !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            anyhow::bail!("GANDI_SHARING_ID: letters, digits and '-' only (an organization id)");
        }
        Ok(Self {
            url,
            token: ferrobill_plugin_sdk::env("GANDI_TOKEN")?,
            sharing_id,
            ..Self::new(String::new(), String::new())
        })
    }

    /// The defaults for everything but the API base and the token.
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            sharing_id: None,
            per_page: 100,
            polls: 5,
            poll_every: Duration::from_secs(2),
            pending_for: Duration::from_secs(24 * 3600),
        }
    }
}

pub struct Gandi {
    config: Config,
    http: reqwest::Client,
    /// Renewals Gandi accepted whose date hadn't moved yet: `name expires_on` -> when.
    pending: Mutex<HashMap<String, Instant>>,
}

impl Gandi {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            config,
            http: ferrobill_plugin_sdk::http_client(Duration::from_secs(60))?,
            pending: Mutex::new(HashMap::new()),
        })
    }

    fn auth(&self) -> String {
        let t = self.config.token.trim();
        if t.contains(' ') {
            t.to_owned()
        } else {
            format!("Bearer {t}")
        }
    }

    fn sharing_query(&self) -> String {
        match &self.config.sharing_id {
            Some(id) => format!("sharing_id={id}"),
            None => String::new(),
        }
    }

    /// Send a request; a 2xx answer, or the error: retry for no answer, 429 and 5xx, refused
    /// (with Gandi's message) for any other 4xx.
    async fn send(&self, req: RequestBuilder, what: &str) -> Result<reqwest::Response, Error> {
        let res = req
            .header("Authorization", self.auth())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Error::retry(format!("Gandi ({what}): {e}")))?;
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let body = res.text().await.unwrap_or_default();
        let e = format!("Gandi ({what}): HTTP {status}: {}", message(&body));
        Err(
            if status.is_client_error()
                && status != StatusCode::TOO_MANY_REQUESTS
                && status != StatusCode::REQUEST_TIMEOUT
            {
                Error::refused(e)
            } else {
                Error::retry(e)
            },
        )
    }

    async fn json(&self, req: RequestBuilder, what: &str) -> Result<(Value, Counts), Error> {
        let res = self.send(req, what).await?;
        let header = |n: &str| {
            res.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let counts = Counts {
            total: header("Total-Count"),
            filtered: header("Filtered-Count"),
        };
        let v = res
            .json()
            .await
            .map_err(|e| Error::retry(format!("Gandi ({what}): not JSON: {e}")))?;
        Ok((v, counts))
    }

    /// The domain's expiry date at the registry (`dates.registry_ends_at`), `YYYY-MM-DD`.
    async fn expiry(&self, name: &str) -> Result<String, Error> {
        let (v, _) = self
            .json(
                self.http
                    .get(format!("{}/domain/domains/{name}", self.config.url)),
                name,
            )
            .await?;
        v["dates"]["registry_ends_at"]
            .as_str()
            .and_then(day)
            .ok_or_else(|| {
                Error::refused(format!(
                    "Gandi has no registry expiry date for {name} (dates.registry_ends_at)"
                ))
            })
    }

    fn pending_key(r: &RenewRequest, name: &str) -> String {
        format!("{name} {}", r.expires_on)
    }
}

#[derive(Debug, Clone, Copy)]
struct Counts {
    total: Option<u64>,
    filtered: Option<u64>,
}

/// Gandi's error message from an error body (`{"message": ...}`, or a validation answer's
/// `{"errors": [{"description": ...}]}`), else the body itself, shortened.
fn message(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        let b = body.trim();
        return if b.is_empty() {
            "(no message)".into()
        } else {
            b.chars().take(300).collect()
        };
    };
    let mut parts = Vec::new();
    for k in ["message", "cause"] {
        if let Some(m) = v[k].as_str().filter(|m| !m.is_empty()) {
            parts.push(m.to_owned());
            break;
        }
    }
    for e in v["errors"].as_array().into_iter().flatten() {
        if let Some(d) = e["description"].as_str() {
            match e["name"].as_str() {
                Some(n) if !n.is_empty() => parts.push(format!("{n}: {d}")),
                _ => parts.push(d.to_owned()),
            }
        }
    }
    if parts.is_empty() {
        v.to_string().chars().take(300).collect()
    } else {
        parts.join("; ")
    }
}

/// `2027-02-02T13:14:15Z` (Gandi) as `2027-02-02`.
pub fn day(s: &str) -> Option<String> {
    let d = s.get(..10)?;
    let ok = d.char_indices().all(|(i, c)| match i {
        4 | 7 => c == '-',
        _ => c.is_ascii_digit(),
    });
    ok.then(|| d.to_owned())
}

/// The name as Gandi knows it (lower case, no trailing dot, ASCII: IDNs as `xn--`), or
/// refused: it goes into the URL path.
fn domain_name(name: &str) -> Result<String, Error> {
    let n = name.trim().trim_end_matches('.').to_ascii_lowercase();
    let ok = n.contains('.')
        && n.len() <= 253
        && n.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    if ok {
        Ok(n)
    } else {
        Err(Error::refused(format!(
            "{name:?} is not a domain name (IDNs in their xn-- form)"
        )))
    }
}

impl Registrar for Gandi {
    fn manifest(&self) -> Value {
        json!({"name": "gandi", "version": env!("CARGO_PKG_VERSION"), "kinds": ["registrar"],
               "registrar_operations": ["domains", "info", "renew"]})
    }

    /// One page of `GET /domain/domains`; the cursor is the next page number.
    async fn domains(&self, r: DomainsRequest) -> Result<DomainsPage, Error> {
        let page: u64 = match r.cursor.as_deref() {
            None | Some("") => 1,
            Some(c) => c
                .parse()
                .ok()
                .filter(|p| *p >= 1)
                .ok_or_else(|| Error::refused(format!("cursor {c:?}: not a page number")))?,
        };
        let per_page = u64::from(self.config.per_page.max(1));
        let mut url = format!(
            "{}/domain/domains?page={page}&per_page={per_page}&sort_by=fqdn",
            self.config.url
        );
        let sharing = self.sharing_query();
        if !sharing.is_empty() {
            url = format!("{url}&{sharing}");
        }
        let (v, counts) = self.json(self.http.get(url), "domain list").await?;
        let items = v
            .as_array()
            .ok_or_else(|| Error::retry("Gandi (domain list): the answer is not a list"))?;
        let domains: Vec<ListedDomain> = items
            .iter()
            .filter_map(|d| {
                Some(ListedDomain {
                    name: d["fqdn"].as_str()?.to_ascii_lowercase(),
                    expires_on: d["dates"]["registry_ends_at"].as_str().and_then(day),
                })
            })
            .collect();
        let more = if items.is_empty() {
            false
        } else {
            match counts.filtered.or(counts.total) {
                Some(n) => page * per_page < n,
                None => items.len() as u64 >= per_page,
            }
        };
        Ok(DomainsPage {
            domains,
            cursor: more.then(|| (page + 1).to_string()),
            more,
        })
    }

    async fn info(&self, r: InfoRequest) -> Result<DomainInfo, Error> {
        let name = domain_name(&r.name)?;
        Ok(DomainInfo {
            expires_on: self.expiry(&name).await?,
            registrar: Some("Gandi".into()),
        })
    }

    /// Safe to repeat: the expiry date is read first. Already later than the date asked about:
    /// it was renewed (a retry after a lost answer, or renewed elsewhere), so that date is the
    /// answer. Earlier: refused (ferrobill's date is wrong; check before renewing). The same:
    /// renewed (`POST .../renew`), then the date read again until it moves. Gandi accepts a
    /// renewal (202) before doing it; if the date hasn't moved after a few reads the answer is
    /// "retry", and no second renewal is sent for that domain and date while one is pending
    /// (`pending_for`), so the repeat finds the new date.
    async fn renew(&self, r: RenewRequest) -> Result<RenewAnswer, Error> {
        let name = domain_name(&r.name)?;
        if !(1..=9).contains(&r.years) {
            return Err(Error::refused("years: 1 to 9 at Gandi"));
        }
        if day(&r.expires_on).as_deref() != Some(r.expires_on.as_str()) {
            return Err(Error::refused(format!(
                "expires_on {:?}: not YYYY-MM-DD",
                r.expires_on
            )));
        }
        let key = Self::pending_key(&r, &name);
        let current = self.expiry(&name).await?;
        if current > r.expires_on {
            self.pending.lock().unwrap().remove(&key);
            return Ok(RenewAnswer {
                expires_on: current,
            });
        }
        if current < r.expires_on {
            return Err(Error::refused(format!(
                "{name} expires on {current} at Gandi, not {}: check the domain before renewing",
                r.expires_on
            )));
        }
        {
            let mut pending = self.pending.lock().unwrap();
            pending.retain(|_, at| at.elapsed() < self.config.pending_for);
            if pending.contains_key(&key) {
                return Err(Error::retry(format!(
                    "a renewal of {name} from {current} was accepted by Gandi and its expiry date \
                     hasn't moved yet; not renewing again"
                )));
            }
        }

        let mut url = format!("{}/domain/domains/{name}/renew", self.config.url);
        let sharing = self.sharing_query();
        if !sharing.is_empty() {
            url = format!("{url}?{sharing}");
        }
        self.send(
            self.http.post(url).json(&json!({"duration": r.years})),
            &format!("renew {name}"),
        )
        .await?;
        self.pending
            .lock()
            .unwrap()
            .insert(key.clone(), Instant::now());
        tracing::info!(domain = %name, years = r.years, from = %current, "renewal accepted by Gandi");

        let mut last = current;
        for i in 0..=self.config.polls {
            if i > 0 {
                tokio::time::sleep(self.config.poll_every).await;
            }
            match self.expiry(&name).await {
                Ok(d) if d > r.expires_on => {
                    self.pending.lock().unwrap().remove(&key);
                    return Ok(RenewAnswer { expires_on: d });
                }
                Ok(d) => last = d,
                Err(e) => tracing::warn!(domain = %name, error = %e, "reading the date again"),
            }
        }
        Err(Error::retry(format!(
            "Gandi accepted the renewal of {name} but still shows it expiring on {last}; \
             ask again later (it won't be renewed twice)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_names_and_messages() {
        assert_eq!(day("2027-02-02T13:14:15Z").as_deref(), Some("2027-02-02"));
        assert_eq!(day("2027-02-02").as_deref(), Some("2027-02-02"));
        assert_eq!(day("02-Feb-2027"), None);
        assert_eq!(day("2027"), None);
        assert_eq!(domain_name("Example.COM.").unwrap(), "example.com");
        assert_eq!(domain_name("xn--bcher-kva.de").unwrap(), "xn--bcher-kva.de");
        for bad in [
            "com",
            "a/b.com",
            "a..com",
            "-a.com",
            "a.com?x=1",
            "bücher.de",
        ] {
            assert!(matches!(domain_name(bad), Err(Error::Refused(_))), "{bad}");
        }
        assert_eq!(
            message(
                r#"{"cause":"Forbidden","code":403,"message":"Access was denied","object":"x"}"#
            ),
            "Access was denied"
        );
        assert_eq!(
            message(
                r#"{"status":"error","errors":[{"location":"body","name":"duration","description":"too big"}]}"#
            ),
            "duration: too big"
        );
        assert_eq!(message("  oops "), "oops");
        assert_eq!(message(""), "(no message)");
    }

    #[test]
    fn auth_header_and_manifest() {
        let g = Gandi::new(Config::new(LIVE, "pat_abc")).unwrap();
        assert_eq!(g.auth(), "Bearer pat_abc");
        let g = Gandi::new(Config::new(LIVE, "Apikey k3y")).unwrap();
        assert_eq!(g.auth(), "Apikey k3y");
        assert_eq!(g.manifest()["name"], "gandi");
        assert_eq!(
            g.manifest()["registrar_operations"],
            json!(["domains", "info", "renew"])
        );
        assert_eq!(g.manifest()["kinds"], json!(["registrar"]));
    }

    /// Read-only, against the live API (nothing is renewed or changed: GETs only):
    /// `GANDI_TOKEN=... cargo test -p ferrobill-gandi live_read_only -- --ignored --nocapture`.
    /// Tries the token as a personal access token (Bearer) and, if refused, as an old API key.
    #[tokio::test]
    #[ignore]
    async fn live_read_only() {
        let token = std::env::var("GANDI_TOKEN").expect("GANDI_TOKEN");
        let mut cfg = Config::new(
            std::env::var("GANDI_URL").unwrap_or_else(|_| LIVE.into()),
            token.trim(),
        );
        cfg.per_page = 5;
        let mut g = Gandi::new(cfg.clone()).unwrap();
        let first = match g.domains(DomainsRequest::default()).await {
            Ok(p) => {
                println!("scheme: Bearer");
                p
            }
            Err(e) => {
                println!("Bearer refused ({}), trying Apikey", e.detail());
                cfg.token = format!("Apikey {}", token.trim());
                g = Gandi::new(cfg).unwrap();
                let p = g.domains(DomainsRequest::default()).await.unwrap();
                println!("scheme: Apikey");
                p
            }
        };
        println!(
            "page 1: {} domains, more: {}, cursor: {:?}",
            first.domains.len(),
            first.more,
            first.cursor
        );
        assert!(first.domains.len() <= 5);
        if first.more {
            let second = g
                .domains(DomainsRequest {
                    cursor: first.cursor.clone(),
                })
                .await
                .unwrap();
            println!("page 2: {} domains", second.domains.len());
            assert!(second.domains.iter().all(|d| !first.domains.contains(d)));
        }
        if let Some(d) = first.domains.first() {
            let i = g
                .info(InfoRequest {
                    name: d.name.clone(),
                })
                .await
                .unwrap();
            println!(
                "{}: expires {} (listing: {:?})",
                d.name, i.expires_on, d.expires_on
            );
            if let Some(listed) = &d.expires_on {
                assert_eq!(&i.expires_on, listed);
            }
        }
        let e = g
            .info(InfoRequest {
                name: "ferrobill-not-a-domain-here-0.com".into(),
            })
            .await
            .unwrap_err();
        println!("unknown domain: {e}");
        assert!(matches!(e, Error::Refused(_)), "{e}");
    }
}
