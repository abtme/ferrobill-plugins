// SPDX-License-Identifier: Apache-2.0
//! Nominet (.uk) registrar plugin for ferrobill: a domain's expiry date (`info`) and renewals
//! (`renew`) over Nominet's EPP service, and the tag's domains (`domains`) from Nominet's List
//! API. See README.md.
//!
//! Nominet allows EPP only from the IP addresses registered for the tag, so run the plugin where
//! its traffic leaves from one of those.

pub mod epp;

use std::sync::Arc;

use epp::{EppError, Session};
use ferrobill_plugin_sdk::Error;
use ferrobill_plugin_sdk::registrar::{
    DomainInfo, DomainsPage, DomainsRequest, InfoRequest, ListedDomain, Registrar, RenewAnswer,
    RenewRequest,
};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};

/// Nominet's EPP service: live and test (testbed).
pub const LIVE: &str = "epp.nominet.org.uk:700";
pub const TESTBED: &str = "testbed-epp.nominet.org.uk:700";
const LIST_API: &str = "https://restapi.nominet.org.uk";

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// How the plugin reaches Nominet.
#[derive(Clone)]
pub struct Config {
    /// `host:port` of the EPP service ([`LIVE`] or [`TESTBED`]).
    pub epp: String,
    /// The registrar tag (the EPP login).
    pub tag: String,
    pub password: String,
    /// The List API key, for `domains` (none: the plugin doesn't offer it).
    pub list_api_key: Option<String>,
    pub list_api: String,
    /// Tests only: talk EPP over plain TCP.
    pub plaintext: bool,
}

pub struct Nominet {
    config: Config,
    tls: tokio_rustls::TlsConnector,
    http: reqwest::Client,
}

impl Nominet {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        let found = rustls_native_certs::load_native_certs();
        for cert in found.certs {
            let _ = roots.add(cert);
        }
        if roots.is_empty() {
            anyhow::bail!("no CA certificates found (the image needs its CA bundle)");
        }
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            config,
            tls: tokio_rustls::TlsConnector::from(Arc::new(tls)),
            http: ferrobill_plugin_sdk::http_client(std::time::Duration::from_secs(60))?,
        })
    }

    /// A logged-in session.
    async fn session(&self) -> Result<Session<Box<dyn Io>>, EppError> {
        let tcp = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tokio::net::TcpStream::connect(&self.config.epp),
        )
        .await
        .map_err(|_| EppError::Io(format!("{}: connect timed out", self.config.epp)))?
        .map_err(|e| EppError::Io(format!("{}: {e}", self.config.epp)))?;
        let stream: Box<dyn Io> = if self.config.plaintext {
            Box::new(tcp)
        } else {
            let host = self
                .config
                .epp
                .split(':')
                .next()
                .unwrap_or_default()
                .to_owned();
            let name = rustls::pki_types::ServerName::try_from(host)
                .map_err(|e| EppError::Io(e.to_string()))?;
            Box::new(
                self.tls
                    .connect(name, tcp)
                    .await
                    .map_err(|e| EppError::Io(format!("TLS: {e}")))?,
            )
        };
        let (mut s, _greeting) = Session::start(stream).await?;
        s.login(&self.config.tag, &self.config.password).await?;
        Ok(s)
    }
}

/// An EPP failure as the plugin protocol's answer: retry or refused.
fn plugin_error(e: EppError) -> Error {
    if e.retryable() {
        Error::retry(e.to_string())
    } else {
        Error::refused(e.to_string())
    }
}

/// `18-Dec-2026` (the List API) as `2026-12-18`.
fn list_date(s: &str) -> Option<String> {
    let mut p = s.split('-');
    let day: u32 = p.next()?.parse().ok()?;
    let mon = p.next()?.to_ascii_lowercase();
    let month = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|m| *m == mon)?
        + 1;
    let year: u32 = p.next()?.parse().ok()?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

impl Registrar for Nominet {
    fn manifest(&self) -> Value {
        let mut ops = vec!["info", "renew"];
        if self.config.list_api_key.is_some() {
            ops.insert(0, "domains");
        }
        json!({"name": "nominet", "version": env!("CARGO_PKG_VERSION"), "kinds": ["registrar"],
               "registrar_operations": ops, "tlds": ["uk"]})
    }

    async fn domains(&self, _r: DomainsRequest) -> Result<DomainsPage, Error> {
        let Some(key) = &self.config.list_api_key else {
            return Err(Error::refused("NOMINET_LIST_API_KEY isn't set"));
        };
        let res = self
            .http
            .get(format!(
                "{}/rest/domains/all?pageSize=10000",
                self.config.list_api
            ))
            .basic_auth(&self.config.tag, Some(key))
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Error::retry(format!("List API: {e}")))?;
        let status = res.status();
        if !status.is_success() {
            let e = format!("List API: HTTP {status}");
            return Err(if status.is_server_error() {
                Error::retry(e)
            } else {
                Error::refused(e)
            });
        }
        let body: Value = res
            .json()
            .await
            .map_err(|e| Error::retry(format!("List API: {e}")))?;
        let domains = body["domainNames"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some(ListedDomain {
                    name: d["domainName"].as_str()?.to_ascii_lowercase(),
                    expires_on: d["expiry"].as_str().and_then(list_date),
                })
            })
            .collect();
        Ok(DomainsPage {
            domains,
            cursor: None,
            more: false,
        })
    }

    async fn info(&self, r: InfoRequest) -> Result<DomainInfo, Error> {
        let mut s = self.session().await.map_err(plugin_error)?;
        let found = s.expiry(&r.name).await;
        s.logout().await;
        let (expires_on, registrar) = found.map_err(plugin_error)?;
        Ok(DomainInfo {
            expires_on,
            registrar,
        })
    }

    /// Safe to repeat: the expiry date is read first. Already later than the date asked about:
    /// it was renewed (a retry after a lost answer, or renewed elsewhere), so that date is the
    /// answer. Earlier: refused (ferrobill's date is wrong; check before renewing). The same:
    /// renewed from exactly that date (the registry refuses any other).
    async fn renew(&self, r: RenewRequest) -> Result<RenewAnswer, Error> {
        if !(1..=10).contains(&r.years) {
            return Err(Error::refused("years: 1 to 10 for .uk"));
        }
        let mut s = self.session().await.map_err(plugin_error)?;
        let result = async {
            let (current, _) = s.expiry(&r.name).await?;
            if current > r.expires_on {
                return Ok(Ok(current));
            }
            if current < r.expires_on {
                return Ok(Err(format!(
                    "{} expires on {current} at Nominet, not {}: check the domain before renewing",
                    r.name, r.expires_on
                )));
            }
            s.renew(&r.name, &current, r.years).await.map(Ok)
        }
        .await;
        s.logout().await;
        match result {
            Ok(Ok(expires_on)) => Ok(RenewAnswer { expires_on }),
            Ok(Err(why)) => Err(Error::refused(why)),
            Err(e) => Err(plugin_error(e)),
        }
    }
}

impl Config {
    /// From the environment: NOMINET_EPP (`live`, `testbed` or host:port), NOMINET_TAG,
    /// NOMINET_EPP_PASSWORD; NOMINET_LIST_API_KEY optional.
    pub fn from_env() -> anyhow::Result<Self> {
        let epp = match ferrobill_plugin_sdk::env("NOMINET_EPP")?.as_str() {
            "live" => LIVE.to_owned(),
            "testbed" => TESTBED.to_owned(),
            other => other.to_owned(),
        };
        Ok(Self {
            epp,
            tag: ferrobill_plugin_sdk::env("NOMINET_TAG")?,
            password: ferrobill_plugin_sdk::env("NOMINET_EPP_PASSWORD")?,
            list_api_key: ferrobill_plugin_sdk::env("NOMINET_LIST_API_KEY").ok(),
            list_api: LIST_API.to_owned(),
            plaintext: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epp::fake;

    fn plugin(addr: &str) -> Nominet {
        Nominet::new(Config {
            epp: addr.into(),
            tag: "ABTME".into(),
            password: "pw".into(),
            list_api_key: None,
            list_api: LIST_API.into(),
            plaintext: true,
        })
        .unwrap()
    }

    /// A registry holding one domain, a.uk, expiring on `expires` (moved on by a renewal).
    async fn registry(expires: &str) -> (String, fake::Seen) {
        let state = Arc::new(std::sync::Mutex::new(expires.to_owned()));
        fake::serve(move |cmd| {
            let mut ex = state.lock().unwrap();
            if cmd.contains("<login>") || cmd.contains("<logout/>") {
                fake::ok("")
            } else if !cmd.contains("<domain:name>a.uk</domain:name>") {
                fake::fail(2303, "Object does not exist")
            } else if cmd.contains("<domain:info") {
                fake::ok(&format!(
                    "<resData><domain:infData xmlns:domain=\"d\"><domain:clID>ABTME</domain:clID>\
                     <domain:exDate>{ex}T00:00:00.0Z</domain:exDate></domain:infData></resData>"
                ))
            } else if cmd.contains("<domain:renew") {
                if !cmd.contains(&format!("<domain:curExpDate>{ex}</domain:curExpDate>")) {
                    return fake::fail(2306, "Parameter value policy error");
                }
                let years: i32 = cmd.split("unit=\"y\">").nth(1).unwrap()[..1]
                    .parse()
                    .unwrap();
                let y: i32 = ex[..4].parse().unwrap();
                *ex = format!("{}{}", y + years, &ex[4..]);
                fake::ok(&format!(
                    "<resData><domain:renData xmlns:domain=\"d\"><domain:exDate>{ex}T00:00:00.0Z\
                     </domain:exDate></domain:renData></resData>"
                ))
            } else {
                fake::fail(2000, "Unknown command")
            }
        })
        .await
    }

    fn renew_req(from: &str, years: u32) -> RenewRequest {
        RenewRequest {
            name: "a.uk".into(),
            years,
            expires_on: from.into(),
        }
    }

    #[tokio::test]
    async fn info_and_a_renewal_that_is_safe_to_repeat() {
        let (addr, seen) = registry("2027-02-02").await;
        let p = plugin(&addr);
        let i = p
            .info(InfoRequest {
                name: "a.uk".into(),
            })
            .await
            .unwrap();
        assert_eq!(i.expires_on, "2027-02-02");
        assert_eq!(i.registrar.as_deref(), Some("ABTME"));
        let e = p
            .info(InfoRequest {
                name: "b.uk".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{e}");

        assert_eq!(
            p.renew(renew_req("2027-02-02", 2))
                .await
                .unwrap()
                .expires_on,
            "2029-02-02"
        );
        // Asked again (an answer that went missing): already renewed, not renewed twice.
        assert_eq!(
            p.renew(renew_req("2027-02-02", 2))
                .await
                .unwrap()
                .expires_on,
            "2029-02-02"
        );
        let renews = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.contains("<domain:renew"))
            .count();
        assert_eq!(renews, 1);
        // A date later than the registry's: refused, nothing renewed.
        let e = p.renew(renew_req("2030-02-02", 1)).await.unwrap_err();
        assert!(e.detail().contains("check the domain"), "{e}");
        assert!(matches!(
            p.renew(renew_req("2029-02-02", 11)).await,
            Err(Error::Refused(_))
        ));
        // Every session logs out.
        let seen = seen.lock().unwrap();
        let logins = seen.iter().filter(|c| c.contains("<login>")).count();
        let logouts = seen.iter().filter(|c| c.contains("<logout/>")).count();
        assert_eq!(logins, logouts);
    }

    #[test]
    fn manifest_and_list_dates() {
        let mut p = plugin("127.0.0.1:1");
        assert_eq!(
            p.manifest()["registrar_operations"],
            json!(["info", "renew"])
        );
        p.config.list_api_key = Some("k".into());
        assert_eq!(
            p.manifest()["registrar_operations"],
            json!(["domains", "info", "renew"])
        );
        assert_eq!(list_date("18-Dec-2026").as_deref(), Some("2026-12-18"));
        assert_eq!(list_date("2-feb-2027").as_deref(), Some("2027-02-02"));
        assert_eq!(list_date("2027-02-02"), None);
    }

    #[tokio::test]
    async fn domains_come_from_the_list_api() {
        use axum::{Json, Router, routing::get};
        let app = Router::new().route(
            "/rest/domains/all",
            get(|| async {
                Json(json!({"domainNames": [
                    {"domainName": "8P.uk", "expiry": "18-Dec-2027"},
                    {"domainName": "x6.uk", "expiry": "02-Feb-2027"}]}))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let mut p = plugin("127.0.0.1:1");
        p.config.list_api_key = Some("k".into());
        p.config.list_api = url;
        let page = p.domains(DomainsRequest::default()).await.unwrap();
        assert_eq!(
            page.domains,
            [
                ListedDomain {
                    name: "8p.uk".into(),
                    expires_on: Some("2027-12-18".into())
                },
                ListedDomain {
                    name: "x6.uk".into(),
                    expires_on: Some("2027-02-02".into())
                }
            ]
        );
        assert!(!page.more);
    }

    /// Against Nominet's testbed (needs the tag's testbed password and an allowed IP):
    /// `NOMINET_TESTBED_TAG=... NOMINET_TESTBED_PASSWORD=... cargo test -p ferrobill-nominet
    /// testbed -- --ignored --nocapture`. Registers a throwaway domain there (the testbed has
    /// no real ones), then checks info and a renewal that is safe to repeat.
    #[tokio::test]
    #[ignore]
    async fn testbed() {
        let tag = std::env::var("NOMINET_TESTBED_TAG").expect("NOMINET_TESTBED_TAG");
        let password = std::env::var("NOMINET_TESTBED_PASSWORD").expect("NOMINET_TESTBED_PASSWORD");
        let p = Nominet::new(Config {
            epp: TESTBED.into(),
            tag,
            password,
            list_api_key: None,
            list_api: LIST_API.into(),
            plaintext: false,
        })
        .unwrap();

        // A contact and domain to work on (testbed only: create commands exist only here).
        let suffix: String = {
            let mut b = [0u8; 5];
            getrandom::getrandom(&mut b).unwrap();
            b.iter().map(|x| format!("{x:02x}")).collect()
        };
        let name = format!("ferrobill-{suffix}.co.uk");
        let mut s = p.session().await.expect("testbed login");
        let contact = format!("fb-{suffix}");
        let ns = "urn:ietf:params:xml:ns:contact-1.0";
        s.call(&epp::test_command(&format!(
            "<create><contact:create xmlns:contact=\"{ns}\"><contact:id>{contact}</contact:id>\
             <contact:postalInfo type=\"int\"><contact:name>ferrobill test</contact:name>\
             <contact:addr><contact:street>Buckingham Palace</contact:street>\
             <contact:city>London</contact:city><contact:pc>SW1A 1AA</contact:pc>\
             <contact:cc>GB</contact:cc></contact:addr></contact:postalInfo>\
             <contact:email>test@example.com</contact:email>\
             <contact:authInfo><contact:pw>x{suffix}Y!</contact:pw></contact:authInfo>\
             </contact:create></create>"
        )))
        .await
        .expect("contact:create");
        let nd = "urn:ietf:params:xml:ns:domain-1.0";
        s.call(&epp::test_command(&format!(
            "<create><domain:create xmlns:domain=\"{nd}\"><domain:name>{name}</domain:name>\
             <domain:period unit=\"y\">1</domain:period><domain:registrant>{contact}</domain:registrant>\
             <domain:authInfo><domain:pw>x{suffix}Y!</domain:pw></domain:authInfo>\
             </domain:create></create>"
        )))
        .await
        .expect("domain:create");
        s.logout().await;

        let i = p.info(InfoRequest { name: name.clone() }).await.unwrap();
        println!("{name}: expires {} ({:?})", i.expires_on, i.registrar);
        let renewed = p
            .renew(RenewRequest {
                name: name.clone(),
                years: 1,
                expires_on: i.expires_on.clone(),
            })
            .await
            .unwrap();
        println!("renewed to {}", renewed.expires_on);
        assert!(renewed.expires_on > i.expires_on);
        let again = p
            .renew(RenewRequest {
                name: name.clone(),
                years: 1,
                expires_on: i.expires_on.clone(),
            })
            .await
            .unwrap();
        assert_eq!(again, renewed, "a repeat doesn't renew twice");
        let now = p.info(InfoRequest { name }).await.unwrap();
        assert_eq!(now.expires_on, renewed.expires_on);
    }
}
