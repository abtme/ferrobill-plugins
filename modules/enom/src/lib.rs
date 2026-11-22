// SPDX-License-Identifier: Apache-2.0
//! eNom registrar plugin for ferrobill: the reseller account's domains (`domains`), a domain's
//! expiry date (`info`) and renewals (`renew`) over eNom's reseller API
//! (`https://reseller.enom.com/interface.asp`). See README.md.
//!
//! Every call is one form POST with `uid`, `pw`, `responsetype=XML` and `command`; eNom answers
//! `<interface-response>` with `ErrCount` and, when it is above 0, `<errors><Err1>...`. eNom
//! accepts API calls only from the IP addresses added to the account, so run the plugin where its
//! traffic leaves from one of those.

pub mod xml;

use ferrobill_plugin_sdk::Error;
use ferrobill_plugin_sdk::registrar::{
    DomainInfo, DomainsPage, DomainsRequest, InfoRequest, ListedDomain, Registrar, RenewAnswer,
    RenewRequest,
};
use serde_json::{Value, json};

/// eNom's reseller API: live and test (a separate account and login).
pub const LIVE: &str = "https://reseller.enom.com/interface.asp";
pub const TEST: &str = "https://resellertest.enom.com/interface.asp";
/// Domains per `GetDomains` page (eNom's maximum).
const PAGE: u32 = 100;

/// How the plugin reaches eNom.
#[derive(Clone)]
pub struct Config {
    /// The `interface.asp` URL ([`LIVE`] or [`TEST`]).
    pub url: String,
    /// The account's login ID.
    pub uid: String,
    /// Its password or API token.
    pub pw: String,
}

pub struct Enom {
    config: Config,
    http: reqwest::Client,
}

/// `example.co.uk` as eNom's `SLD` (`example`) and `TLD` (`co.uk`).
fn split(name: &str) -> Result<(String, String), Error> {
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    match name.split_once('.') {
        Some((sld, tld)) if !sld.is_empty() && !tld.is_empty() && !tld.ends_with('.') => {
            Ok((sld.to_owned(), tld.to_owned()))
        }
        _ => Err(Error::refused(format!("{name:?} is not a domain name"))),
    }
}

/// A `YYYY-MM-DD` date (so it compares as text).
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// A date in either of eNom's forms (`M/D/YYYY[ time]` or `YYYY-MM-DD[...]`) as `YYYY-MM-DD`.
fn any_date(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() >= 10 && is_iso_date(&s[..10]) {
        return Some(s[..10].to_owned());
    }
    xml::date(s)
}

impl Enom {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            config,
            http: ferrobill_plugin_sdk::http_client(std::time::Duration::from_secs(60))?,
        })
    }

    /// One API call: the answer's XML, or why not (retry: eNom unreachable, its own failures;
    /// refused: anything eNom says is wrong with the request or the login).
    async fn call(&self, command: &str, params: &[(&str, &str)]) -> Result<String, Error> {
        let mut form: Vec<(&str, &str)> = vec![
            ("uid", &self.config.uid),
            ("pw", &self.config.pw),
            ("responsetype", "XML"),
            ("command", command),
        ];
        form.extend_from_slice(params);
        // A POST, so the login is never part of a URL (or of an error message quoting one).
        let res = self
            .http
            .post(&self.config.url)
            .form(&form)
            .send()
            .await
            .map_err(|e| Error::retry(format!("eNom {command}: {}", e.without_url())))?;
        let status = res.status();
        if !status.is_success() {
            let e = format!("eNom {command}: HTTP {status}");
            return Err(
                if status.is_server_error()
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::REQUEST_TIMEOUT
                {
                    Error::retry(e)
                } else {
                    Error::refused(e)
                },
            );
        }
        let body = res
            .text()
            .await
            .map_err(|e| Error::retry(format!("eNom {command}: {}", e.without_url())))?;
        if xml::text_of(&body, "ErrCount").is_none() {
            // A maintenance page or a cut-off answer.
            return Err(Error::retry(format!(
                "eNom {command}: not an eNom answer (no ErrCount)"
            )));
        }
        if let Some(e) = xml::error(&body) {
            let e2 = format!("{command}: {e}");
            return Err(if e.retryable() {
                Error::retry(e2)
            } else {
                Error::refused(e2)
            });
        }
        Ok(body)
    }

    /// The domain's expiry date (`YYYY-MM-DD`) and registrar, from `GetDomainInfo`.
    async fn expiry(&self, sld: &str, tld: &str) -> Result<(String, Option<String>), Error> {
        let body = self
            .call("GetDomainInfo", &[("sld", sld), ("tld", tld)])
            .await?;
        let raw = xml::text_of(&body, "expiration").ok_or_else(|| {
            Error::retry(format!(
                "eNom GetDomainInfo {sld}.{tld}: no expiration in the answer"
            ))
        })?;
        let expires_on = any_date(&raw).ok_or_else(|| {
            Error::retry(format!(
                "eNom GetDomainInfo {sld}.{tld}: can't read the expiration {raw:?}"
            ))
        })?;
        let registrar = xml::text_of(&body, "registrar").filter(|r| !r.is_empty());
        Ok((expires_on, registrar))
    }
}

impl Registrar for Enom {
    fn manifest(&self) -> Value {
        json!({"name": "enom", "version": env!("CARGO_PKG_VERSION"), "kinds": ["registrar"],
               "registrar_operations": ["domains", "info", "renew"]})
    }

    /// One page of `GetDomains` (the domains the account owns); the cursor is eNom's `Start`.
    async fn domains(&self, r: DomainsRequest) -> Result<DomainsPage, Error> {
        let start: u32 = match r.cursor.as_deref() {
            None | Some("") => 1,
            Some(c) => c
                .parse()
                .ok()
                .filter(|s| *s >= 1)
                .ok_or_else(|| Error::refused(format!("cursor {c:?} is not a position")))?,
        };
        let start_s = start.to_string();
        let page_s = PAGE.to_string();
        let body = self
            .call(
                "GetDomains",
                &[("Tab", "IOwn"), ("Display", &page_s), ("Start", &start_s)],
            )
            .await?;
        let domains = xml::blocks(&body, "domain")
            .into_iter()
            .filter_map(|d| {
                let sld = xml::text_of(d, "sld")?;
                let tld = xml::text_of(d, "tld")?;
                if sld.is_empty() || tld.is_empty() {
                    return None;
                }
                Some(ListedDomain {
                    name: format!("{sld}.{tld}").to_ascii_lowercase(),
                    expires_on: xml::text_of(d, "expiration-date").and_then(|e| any_date(&e)),
                })
            })
            .collect();
        let num = |tag: &str| -> Option<u32> { xml::text_of(&body, tag)?.trim().parse().ok() };
        let next = num("NextRecords").filter(|n| *n > start);
        // NextRecords points past the end on the last page in some answers: trust the total too.
        let next = match (next, num("TotalDomainCount")) {
            (Some(n), Some(total)) if n > total => None,
            (n, _) => n,
        };
        Ok(DomainsPage {
            domains,
            cursor: next.map(|n| n.to_string()),
            more: next.is_some(),
        })
    }

    async fn info(&self, r: InfoRequest) -> Result<DomainInfo, Error> {
        let (sld, tld) = split(&r.name)?;
        let (expires_on, registrar) = self.expiry(&sld, &tld).await?;
        Ok(DomainInfo {
            expires_on,
            registrar,
        })
    }

    /// Safe to repeat: the expiry date is read first. Already later than the date asked about:
    /// it was renewed (a retry after a lost answer, or renewed elsewhere), so that date is the
    /// answer. Earlier: refused (ferrobill's date is wrong; check before renewing). The same:
    /// `Extend`, then the new date is read back. eNom itself also refuses a second renewal of a
    /// domain within 24 hours (the plugin never sends `OverrideOrder`).
    async fn renew(&self, r: RenewRequest) -> Result<RenewAnswer, Error> {
        if !(1..=10).contains(&r.years) {
            return Err(Error::refused("years: 1 to 10"));
        }
        if !is_iso_date(&r.expires_on) {
            return Err(Error::refused(format!(
                "expires_on {:?} is not YYYY-MM-DD",
                r.expires_on
            )));
        }
        let (sld, tld) = split(&r.name)?;
        let name = format!("{sld}.{tld}");
        let (current, _) = self.expiry(&sld, &tld).await?;
        if current > r.expires_on {
            return Ok(RenewAnswer {
                expires_on: current,
            });
        }
        if current < r.expires_on {
            return Err(Error::refused(format!(
                "{name} expires on {current} at eNom, not {}: check the domain before renewing",
                r.expires_on
            )));
        }
        let years = r.years.to_string();
        let body = self
            .call(
                "Extend",
                &[("SLD", &sld), ("TLD", &tld), ("NumYears", &years)],
            )
            .await?;
        let order = xml::text_of(&body, "OrderID").unwrap_or_default();
        if let Some(code) = xml::text_of(&body, "RRPCode").filter(|c| c != "200") {
            let text = xml::text_of(&body, "RRPText").unwrap_or_default();
            return Err(Error::refused(format!(
                "eNom Extend {name}: RRPCode {code} {text} (order {order}): check the domain"
            )));
        }
        // Read the new date back; if eNom doesn't show it yet, the date the Extend answer gave.
        let (after, _) = self.expiry(&sld, &tld).await?;
        if after > current {
            return Ok(RenewAnswer { expires_on: after });
        }
        if let Some(d) = xml::text_of(&body, "RegistryExpDate")
            .and_then(|d| any_date(&d))
            .filter(|d| *d > current)
        {
            return Ok(RenewAnswer { expires_on: d });
        }
        // Renewed, but no new date to answer with yet: asked again, the date will have moved on
        // (answered without renewing), or eNom refuses a second renewal within 24 hours.
        Err(Error::retry(format!(
            "eNom Extend {name}: renewed (order {order}) but eNom still shows {current}"
        )))
    }
}

impl Config {
    /// From the environment: ENOM_UID, ENOM_PW (password or API token); ENOM_URL optional
    /// (`live`, the default; `test`; or an `interface.asp` URL).
    pub fn from_env() -> anyhow::Result<Self> {
        let url = match ferrobill_plugin_sdk::env("ENOM_URL").ok().as_deref() {
            None | Some("live") => LIVE.to_owned(),
            Some("test") => TEST.to_owned(),
            Some(other) => other.to_owned(),
        };
        Ok(Self {
            url,
            uid: ferrobill_plugin_sdk::env("ENOM_UID")?,
            pw: ferrobill_plugin_sdk::env("ENOM_PW")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Bytes, http::StatusCode, response::IntoResponse, routing::post};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// The fake eNom's state: domains (name -> `YYYY-MM-DD`) and every command it was sent.
    #[derive(Default)]
    struct Fake {
        domains: BTreeMap<String, String>,
        seen: Vec<String>,
        /// Extend moves the date, but GetDomainInfo goes on showing the old one.
        stale_info: bool,
        /// Extend answers RegistryExpDate (as a real answer does).
        registry_exp_date: bool,
    }

    type Shared = Arc<Mutex<Fake>>;

    fn us_date(iso: &str) -> String {
        let (y, m, d) = (&iso[..4], &iso[5..7], &iso[8..10]);
        format!(
            "{}/{}/{y}",
            m.parse::<u32>().unwrap(),
            d.parse::<u32>().unwrap()
        )
    }

    fn answer(command: &str, inner: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><interface-response>{inner}\
             <Command>{}</Command><Language>eng</Language><ErrCount>0</ErrCount>\
             <ResponseCount>0</ResponseCount><Done>true</Done></interface-response>",
            command.to_ascii_uppercase()
        )
    }

    fn failure(msg: &str, number: &str, kind: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><interface-response><ErrCount>1\
             </ErrCount><errors><Err1>{msg}</Err1></errors><ResponseCount>1</ResponseCount>\
             <responses><response><ResponseNumber>{number}</ResponseNumber><ResponseString>\
             {kind}</ResponseString></response></responses><Done>true</Done>\
             </interface-response>"
        )
    }

    async fn handle(state: Shared, body: Bytes) -> axum::response::Response {
        let form: BTreeMap<String, String> = serde_urlencoded::from_bytes(&body).unwrap();
        let get = |k: &str| {
            form.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let command = get("command");
        let mut s = state.lock().unwrap();
        s.seen.push(command.clone());
        assert_eq!(get("responsetype"), "XML");
        if get("uid") != "reseller" || get("pw") != "token" {
            return failure(
                "Bad User name or Password",
                "304155",
                "Validation error; invalid ; loginid",
            )
            .into_response();
        }
        let name = format!("{}.{}", get("sld"), get("tld"));
        match command.as_str() {
            "GetDomains" => {
                // Two per page, whatever Display asks for, to see the paging.
                let start: usize = get("Start").parse().unwrap();
                let all: Vec<_> = s.domains.iter().collect();
                let page: String = all
                    .iter()
                    .skip(start - 1)
                    .take(2)
                    .map(|(n, e)| {
                        let (sld, tld) = n.split_once('.').unwrap();
                        format!(
                            "<domain><DomainNameID>1</DomainNameID><sld>{sld}</sld>\
                             <tld>{tld}</tld><ns-status>YES</ns-status><expiration-date>{}\
                             </expiration-date><auto-renew>No</auto-renew></domain>",
                            us_date(e)
                        )
                    })
                    .collect();
                let next = if start + 2 <= all.len() { start + 2 } else { 0 };
                answer(
                    "GetDomains",
                    &format!(
                        "<GetDomains><tab/><domain-list type=\"Registered\">{page}</domain-list>\
                         <NextRecords>{next}</NextRecords><TotalDomainCount>{}\
                         </TotalDomainCount></GetDomains>",
                        all.len()
                    ),
                )
                .into_response()
            }
            _ if name == "busy.com" => failure(
                "An unexpected error has occurred",
                "699999",
                "System error; unknown; unknown",
            )
            .into_response(),
            _ if name == "down.com" => {
                (StatusCode::SERVICE_UNAVAILABLE, "down for maintenance").into_response()
            }
            _ if name == "html.com" => "<html>maintenance</html>".into_response(),
            "GetDomainInfo" => match s.domains.get(&name) {
                Some(e) => {
                    let shown = if s.stale_info {
                        "2027-02-02".to_owned()
                    } else {
                        e.clone()
                    };
                    answer(
                        "GetDomainInfo",
                        &format!(
                            "<GetDomainInfo><domainname sld=\"x\" tld=\"y\" domainnameid=\"1\">\
                             {name}</domainname><status><expiration>{} 7:34:00 PM</expiration>\
                             <registrar>eNom, Inc.</registrar><registrationstatus>Registered\
                             </registrationstatus></status></GetDomainInfo>",
                            us_date(&shown)
                        ),
                    )
                    .into_response()
                }
                None => failure(
                    "Domain name not found",
                    "316153",
                    "Validation error; not found; domain name(s)",
                )
                .into_response(),
            },
            "Extend" => {
                let years: i32 = get("NumYears").parse().unwrap();
                let with_date = s.registry_exp_date;
                let Some(e) = s.domains.get_mut(&name) else {
                    return failure(
                        "Domain name not found",
                        "316153",
                        "Validation error; not found; domain name(s)",
                    )
                    .into_response();
                };
                let y: i32 = e[..4].parse().unwrap();
                *e = format!("{}{}", y + years, &e[4..]);
                let reg = if with_date {
                    format!(
                        "<RegistryExpDate>{} 10:00:00 AM</RegistryExpDate>",
                        us_date(e)
                    )
                } else {
                    String::new()
                };
                answer(
                    "Extend",
                    &format!(
                        "<Extension>successful</Extension><DomainName>{name}</DomainName>\
                         <OrderID>157614</OrderID><RRPCode>200</RRPCode><RRPText>Command \
                         completed successfully</RRPText><DomainInfo>{reg}</DomainInfo>"
                    ),
                )
                .into_response()
            }
            _ => failure(
                "Invalid command",
                "304150",
                "Validation error; invalid ; command",
            )
            .into_response(),
        }
    }

    /// A fake eNom holding `domains`, and the plugin pointed at it.
    async fn fake(domains: &[(&str, &str)]) -> (Enom, Shared) {
        let state: Shared = Arc::new(Mutex::new(Fake {
            domains: domains
                .iter()
                .map(|(n, e)| (n.to_string(), e.to_string()))
                .collect(),
            ..Fake::default()
        }));
        let s2 = state.clone();
        let app = Router::new().route(
            "/interface.asp",
            post(move |body: Bytes| handle(s2.clone(), body)),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/interface.asp", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let p = Enom::new(Config {
            url,
            uid: "reseller".into(),
            pw: "token".into(),
        })
        .unwrap();
        (p, state)
    }

    fn renew_req(name: &str, from: &str, years: u32) -> RenewRequest {
        RenewRequest {
            name: name.into(),
            years,
            expires_on: from.into(),
        }
    }

    fn extends(s: &Shared) -> usize {
        s.lock()
            .unwrap()
            .seen
            .iter()
            .filter(|c| *c == "Extend")
            .count()
    }

    #[tokio::test]
    async fn domains_are_listed_page_by_page() {
        let (p, _) = fake(&[
            ("a.com", "2027-02-02"),
            ("b.co.uk", "2026-12-18"),
            ("c.net", "2028-07-07"),
        ])
        .await;
        let page = p.domains(DomainsRequest::default()).await.unwrap();
        assert_eq!(
            page.domains,
            [
                ListedDomain {
                    name: "a.com".into(),
                    expires_on: Some("2027-02-02".into())
                },
                ListedDomain {
                    name: "b.co.uk".into(),
                    expires_on: Some("2026-12-18".into())
                }
            ]
        );
        assert!(page.more);
        assert_eq!(page.cursor.as_deref(), Some("3"));
        let page = p
            .domains(DomainsRequest {
                cursor: page.cursor,
            })
            .await
            .unwrap();
        assert_eq!(page.domains.len(), 1);
        assert_eq!(page.domains[0].name, "c.net");
        assert!(!page.more);
        assert_eq!(page.cursor, None);
        assert!(matches!(
            p.domains(DomainsRequest {
                cursor: Some("x".into())
            })
            .await,
            Err(Error::Refused(_))
        ));
    }

    #[tokio::test]
    async fn info_reads_the_expiry_date() {
        let (p, _) = fake(&[("example.co.uk", "2027-02-02")]).await;
        let i = p
            .info(InfoRequest {
                name: "Example.CO.UK.".into(),
            })
            .await
            .unwrap();
        assert_eq!(i.expires_on, "2027-02-02");
        assert_eq!(i.registrar.as_deref(), Some("eNom, Inc."));
        let e = p
            .info(InfoRequest {
                name: "missing.com".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");
        assert!(e.detail().contains("not found"), "{e}");
        assert!(matches!(
            p.info(InfoRequest {
                name: "nodot".into()
            })
            .await,
            Err(Error::Refused(_))
        ));
    }

    #[tokio::test]
    async fn a_renewal_is_safe_to_repeat() {
        let (p, s) = fake(&[("a.com", "2027-02-02")]).await;
        assert_eq!(
            p.renew(renew_req("a.com", "2027-02-02", 2))
                .await
                .unwrap()
                .expires_on,
            "2029-02-02"
        );
        // Asked again (an answer that went missing): already renewed, not renewed twice.
        assert_eq!(
            p.renew(renew_req("a.com", "2027-02-02", 2))
                .await
                .unwrap()
                .expires_on,
            "2029-02-02"
        );
        assert_eq!(extends(&s), 1);
        // A date later than eNom's: refused, nothing renewed.
        let e = p
            .renew(renew_req("a.com", "2030-02-02", 1))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");
        assert!(e.detail().contains("check the domain"), "{e}");
        // Bad requests never reach eNom.
        for bad in [
            renew_req("a.com", "2029-02-02", 0),
            renew_req("a.com", "2029-02-02", 11),
            renew_req("a.com", "02/02/2029", 1),
            renew_req("nodot", "2029-02-02", 1),
        ] {
            assert!(matches!(p.renew(bad).await, Err(Error::Refused(_))));
        }
        assert_eq!(extends(&s), 1);
        // A domain eNom doesn't have: refused before any Extend.
        assert!(matches!(
            p.renew(renew_req("b.com", "2027-02-02", 1)).await,
            Err(Error::Refused(_))
        ));
        assert_eq!(extends(&s), 1);
    }

    #[tokio::test]
    async fn a_renewal_enom_does_not_show_yet() {
        let (p, s) = fake(&[("a.com", "2027-02-02")]).await;
        s.lock().unwrap().stale_info = true;
        s.lock().unwrap().registry_exp_date = true;
        // The date from the Extend answer.
        assert_eq!(
            p.renew(renew_req("a.com", "2027-02-02", 1))
                .await
                .unwrap()
                .expires_on,
            "2028-02-02"
        );
        // No date anywhere: retried, not answered with the old date.
        let (p, s) = fake(&[("a.com", "2027-02-02")]).await;
        s.lock().unwrap().stale_info = true;
        let e = p
            .renew(renew_req("a.com", "2027-02-02", 1))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Retry(_)), "{e}");
        assert!(e.detail().contains("order 157614"), "{e}");
    }

    #[tokio::test]
    async fn errors_say_whether_to_retry() {
        let (p, _) = fake(&[]).await;
        let info = |n: &str| p.info(InfoRequest { name: n.to_owned() });
        // eNom's own system error, HTTP 5xx, and something that isn't an eNom answer: retry.
        for n in ["busy.com", "down.com", "html.com"] {
            let e = info(n).await.unwrap_err();
            assert!(matches!(e, Error::Retry(_)), "{n}: {e}");
        }
        // A bad login: refused, and the password is not in the message.
        let (mut p, _) = fake(&[("a.com", "2027-02-02")]).await;
        p.config.pw = "wrong-secret".into();
        let e = p
            .info(InfoRequest {
                name: "a.com".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");
        assert!(e.detail().contains("304155"), "{e}");
        assert!(!e.detail().contains("wrong-secret"), "{e}");
        // Nothing listening: retry.
        p.config.url = "http://127.0.0.1:1/interface.asp".into();
        assert!(matches!(
            p.domains(DomainsRequest::default()).await,
            Err(Error::Retry(_))
        ));
    }

    #[test]
    fn manifest_names_and_dates() {
        let p = Enom::new(Config {
            url: LIVE.into(),
            uid: "u".into(),
            pw: "p".into(),
        })
        .unwrap();
        let m = p.manifest();
        assert_eq!(m["name"], "enom");
        assert_eq!(m["kinds"], json!(["registrar"]));
        assert_eq!(
            m["registrar_operations"],
            json!(["domains", "info", "renew"])
        );
        assert_eq!(split("a.co.uk").unwrap(), ("a".into(), "co.uk".into()));
        assert!(split(".com").is_err());
        assert!(split("a.").is_err());
        assert_eq!(any_date("2028-02-02T10:00:00").unwrap(), "2028-02-02");
        assert_eq!(any_date("2/2/2028 10:00:00 AM").unwrap(), "2028-02-02");
    }

    /// Read-only, against the live account (never renews anything):
    /// `ENOM_UID=... ENOM_PW=... cargo test -p ferrobill-enom live_read_only -- --ignored
    /// --nocapture`. Lists the first page of domains and reads one domain's expiry date, and
    /// checks both agree. Only GetDomains and GetDomainInfo are sent.
    #[tokio::test]
    #[ignore]
    async fn live_read_only() {
        let p = Enom::new(Config {
            url: std::env::var("ENOM_URL").unwrap_or_else(|_| LIVE.into()),
            uid: std::env::var("ENOM_UID").expect("ENOM_UID"),
            pw: std::env::var("ENOM_PW").expect("ENOM_PW"),
        })
        .unwrap();
        let page = p.domains(DomainsRequest::default()).await.unwrap();
        println!(
            "{} domains on the first page, more: {}",
            page.domains.len(),
            page.more
        );
        let mut total = page.domains.len();
        let mut cursor = page.cursor.clone();
        while let Some(c) = cursor {
            let next = p.domains(DomainsRequest { cursor: Some(c) }).await.unwrap();
            total += next.domains.len();
            cursor = next.cursor;
        }
        println!("{total} domains in all");
        let Some(first) = page.domains.first() else {
            return;
        };
        let i = p
            .info(InfoRequest {
                name: first.name.clone(),
            })
            .await
            .unwrap();
        println!(
            "{}: listed {:?}, info {} ({:?})",
            first.name, first.expires_on, i.expires_on, i.registrar
        );
        assert_eq!(first.expires_on.as_deref(), Some(i.expires_on.as_str()));
        // A domain that isn't in the account: refused (eNom's "Domain name not found").
        let e = p
            .info(InfoRequest {
                name: "ferrobill-no-such-domain-4f1c.com".into(),
            })
            .await
            .unwrap_err();
        println!("missing domain: {e}");
        assert!(matches!(e, Error::Refused(_)), "{e}");
    }
}
