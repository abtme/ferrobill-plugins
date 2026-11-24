// SPDX-License-Identifier: Apache-2.0
//! NetEarthOne registrar plugin for ferrobill: the reseller account's domains (`domains`), a
//! domain's expiry date (`info`) and renewals (`renew`) over NetEarthOne's HTTP API (the
//! LogicBoxes / ResellerClub API at `httpapi.com`). See README.md.
//!
//! The API takes the reseller's `auth-userid` and `api-key` as request parameters. On GET they
//! go in the query string, so no URL is ever put in an error or a log line, and the key is
//! blanked out of anything the API answers.

use std::time::Duration;

use ferrobill_plugin_sdk::Error;
use ferrobill_plugin_sdk::registrar::{
    DomainInfo, DomainsPage, DomainsRequest, InfoRequest, ListedDomain, Registrar, RenewAnswer,
    RenewRequest,
};
use serde_json::{Value, json};

/// The live API and the test (sandbox) one.
pub const LIVE: &str = "https://httpapi.com/api";
pub const TEST: &str = "https://test.httpapi.com/api";

/// Orders per page of `domains/search.json` (the API's maximum).
const PAGE_SIZE: u64 = 500;

/// How the renewal is paid for. `NoInvoice`: no invoice is raised for the customer and the
/// order is renewed, the cost taken from the reseller's own balance. (`PayInvoice` would take
/// it from the customer's debit account and leave the order pending if that is short;
/// `KeepInvoice` renews and leaves the customer an invoice to pay; `OnlyAdd` raises the invoice
/// and leaves the renewal pending.)
const INVOICE_OPTION: &str = "NoInvoice";

/// How the plugin reaches NetEarthOne.
#[derive(Clone)]
pub struct Config {
    /// The API's base URL, ending `/api` ([`LIVE`] or [`TEST`]).
    pub url: String,
    pub auth_userid: String,
    pub api_key: String,
    /// After a renewal, how often and how many times to read the expiry date again while it
    /// hasn't moved yet.
    pub recheck_every: Duration,
    pub rechecks: u32,
}

impl Config {
    /// From the environment: NETEARTHONE_AUTH_USERID, NETEARTHONE_API_KEY; NETEARTHONE_URL
    /// optional (`live`, the default, `test`, or a base URL ending `/api`).
    pub fn from_env() -> anyhow::Result<Self> {
        let url = match std::env::var("NETEARTHONE_URL").ok().as_deref() {
            None | Some("") | Some("live") => LIVE.to_owned(),
            Some("test") => TEST.to_owned(),
            Some(other) => other.trim_end_matches('/').to_owned(),
        };
        Ok(Self {
            url,
            auth_userid: ferrobill_plugin_sdk::env("NETEARTHONE_AUTH_USERID")?,
            api_key: ferrobill_plugin_sdk::env("NETEARTHONE_API_KEY")?,
            recheck_every: Duration::from_secs(3),
            rechecks: 5,
        })
    }
}

pub struct NetEarthOne {
    config: Config,
    http: reqwest::Client,
}

/// What `domains/details.json` says about an order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Order {
    /// The expiry time at the registry, unix seconds.
    endtime: i64,
    /// Auto-renewal (`recurring`), if the API said.
    recurring: Option<bool>,
}

impl NetEarthOne {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            config,
            http: ferrobill_plugin_sdk::http_client(Duration::from_secs(60))?,
        })
    }

    fn auth(&self) -> Vec<(&'static str, String)> {
        vec![
            ("auth-userid", self.config.auth_userid.clone()),
            ("api-key", self.config.api_key.clone()),
        ]
    }

    /// `text` without the API key in it.
    fn redact(&self, text: &str) -> String {
        if self.config.api_key.is_empty() {
            text.to_owned()
        } else {
            text.replace(&self.config.api_key, "[api-key]")
        }
    }

    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<Value, Error> {
        let mut url = reqwest::Url::parse(&format!("{}{path}", self.config.url))
            .map_err(|e| Error::refused(format!("NETEARTHONE_URL: {e}")))?;
        url.query_pairs_mut()
            .extend_pairs(self.auth())
            .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
        let res = self.http.get(url).send().await;
        self.answer(path, res).await
    }

    /// A POST, with the parameters (and so the key) in the form body.
    async fn post(&self, path: &str, params: &[(&str, String)]) -> Result<Value, Error> {
        let mut form = self.auth();
        form.extend(params.iter().map(|(k, v)| (*k, v.clone())));
        let res = self
            .http
            .post(format!("{}{path}", self.config.url))
            .form(&form)
            .send()
            .await;
        self.answer(path, res).await
    }

    async fn answer(
        &self,
        path: &str,
        res: Result<reqwest::Response, reqwest::Error>,
    ) -> Result<Value, Error> {
        // `without_url`: the URL holds the key.
        let res =
            res.map_err(|e| Error::retry(format!("NetEarthOne {path}: {}", e.without_url())))?;
        let status = res.status().as_u16();
        let body = res
            .text()
            .await
            .map_err(|e| Error::retry(format!("NetEarthOne {path}: {}", e.without_url())))?;
        check(path, status, &self.redact(&body))
    }

    /// The order ID of a domain in the account.
    async fn order_id(&self, name: &str) -> Result<String, Error> {
        let v = self
            .get("/domains/orderid.json", &[("domain-name", name.to_owned())])
            .await?;
        text(&v).ok_or_else(|| {
            Error::retry(format!(
                "NetEarthOne /domains/orderid.json: no order ID for {name} in {v}"
            ))
        })
    }

    async fn order(&self, order_id: &str) -> Result<Order, Error> {
        let v = self
            .get(
                "/domains/details.json",
                &[
                    ("order-id", order_id.to_owned()),
                    ("options", "OrderDetails".to_owned()),
                ],
            )
            .await?;
        let endtime = text(&v["endtime"])
            .and_then(|t| t.parse::<i64>().ok())
            .ok_or_else(|| {
                Error::retry(format!(
                    "NetEarthOne /domains/details.json: order {order_id} has no endtime"
                ))
            })?;
        let recurring = match text(&v["recurring"]).as_deref() {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        };
        Ok(Order { endtime, recurring })
    }
}

/// An API answer as JSON, or the protocol's error. The API answers its own errors (an unknown
/// domain, a wrong `exp-date`, no permission) as `{"status": "ERROR", "message": ...}`, often
/// with HTTP 500: those are refused, whatever the HTTP status. Otherwise a 5xx (or a body that
/// isn't JSON) is retried and a 4xx refused.
fn check(path: &str, status: u16, body: &str) -> Result<Value, Error> {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    if let Some(v) = &parsed
        && v["status"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("error"))
    {
        let msg = ["message", "error", "description"]
            .iter()
            .find_map(|k| v[*k].as_str().filter(|s| !s.is_empty()))
            .map(str::to_owned)
            .unwrap_or_else(|| short(body));
        return Err(Error::refused(format!("NetEarthOne {path}: {msg}")));
    }
    if (500..600).contains(&status) {
        return Err(Error::retry(format!(
            "NetEarthOne {path}: HTTP {status}: {}",
            short(body)
        )));
    }
    if !(200..300).contains(&status) {
        return Err(Error::refused(format!(
            "NetEarthOne {path}: HTTP {status}: {}",
            short(body)
        )));
    }
    parsed.ok_or_else(|| {
        Error::retry(format!(
            "NetEarthOne {path}: the answer is not JSON: {}",
            short(body)
        ))
    })
}

fn short(s: &str) -> String {
    let s = s.trim();
    match s.char_indices().nth(200) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_owned(),
    }
}

/// A string or number field as text.
fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Days since 1970-01-01 of a date in the proleptic Gregorian calendar (Howard Hinnant's
/// `days_from_civil`).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number from [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Unix seconds as the UTC date `YYYY-MM-DD`.
pub fn utc_date(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD`, checked (a real date), as written.
fn valid_date(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..].parse().ok()?;
    if !(1..=12).contains(&m) || d == 0 {
        return None;
    }
    (civil_from_days(days_from_civil(y, m, d)) == (y, m, d)).then_some(s)
}

impl Registrar for NetEarthOne {
    fn manifest(&self) -> Value {
        json!({"name": "netearthone", "version": env!("CARGO_PKG_VERSION"),
               "kinds": ["registrar"], "registrar_operations": ["domains", "info", "renew"]})
    }

    /// One page of `domains/search.json` (500 orders); the cursor is the next page number.
    /// Deleted and archived orders are left out.
    async fn domains(&self, r: DomainsRequest) -> Result<DomainsPage, Error> {
        let page: u64 = match r.cursor.as_deref() {
            None | Some("") => 1,
            Some(c) => c
                .parse()
                .ok()
                .filter(|p| *p >= 1)
                .ok_or_else(|| Error::refused(format!("bad cursor {c:?}")))?,
        };
        let v = self
            .get(
                "/domains/search.json",
                &[
                    ("no-of-records", PAGE_SIZE.to_string()),
                    ("page-no", page.to_string()),
                ],
            )
            .await?;
        let Some(map) = v.as_object() else {
            return Err(Error::retry(
                "NetEarthOne /domains/search.json: the answer is not an object",
            ));
        };
        let mut rows: Vec<(u64, &Value)> = map
            .iter()
            .filter_map(|(k, row)| Some((k.parse::<u64>().ok()?, row)))
            .collect();
        rows.sort_by_key(|(i, _)| *i);
        let domains = rows
            .into_iter()
            .filter(|(_, row)| {
                !matches!(
                    row["entity.currentstatus"].as_str(),
                    Some("Deleted" | "Archived")
                )
            })
            .filter_map(|(_, row)| {
                Some(ListedDomain {
                    name: text(&row["entity.description"])?.to_ascii_lowercase(),
                    expires_on: text(&row["orders.endtime"])
                        .and_then(|t| t.parse::<i64>().ok())
                        .map(utc_date),
                })
            })
            .collect();
        let total = text(&v["recsindb"])
            .and_then(|t| t.parse::<u64>().ok())
            .unwrap_or(0);
        let more = page * PAGE_SIZE < total;
        Ok(DomainsPage {
            domains,
            cursor: more.then(|| (page + 1).to_string()),
            more,
        })
    }

    async fn info(&self, r: InfoRequest) -> Result<DomainInfo, Error> {
        let id = self.order_id(&r.name).await?;
        let order = self.order(&id).await?;
        Ok(DomainInfo {
            expires_on: utc_date(order.endtime),
            registrar: Some("NetEarthOne".into()),
        })
    }

    /// Safe to repeat: the expiry date is read first. Already later than the date asked about:
    /// it was renewed (a retry after a lost answer, or renewed elsewhere), so that date is the
    /// answer. Earlier: refused (ferrobill's date is wrong; check before renewing). The same day:
    /// renewed with `exp-date` set to the exact current expiry time, which the API checks, so a
    /// second renewal from the same date can't go through once the first has moved it.
    async fn renew(&self, r: RenewRequest) -> Result<RenewAnswer, Error> {
        if !(1..=10).contains(&r.years) {
            return Err(Error::refused("years: 1 to 10"));
        }
        let wanted = valid_date(&r.expires_on).ok_or_else(|| {
            Error::refused(format!("expires_on {:?} is not YYYY-MM-DD", r.expires_on))
        })?;
        let id = self.order_id(&r.name).await?;
        let order = self.order(&id).await?;
        let current = utc_date(order.endtime);
        if current.as_str() > wanted {
            return Ok(RenewAnswer {
                expires_on: current,
            });
        }
        if current.as_str() < wanted {
            return Err(Error::refused(format!(
                "{} expires on {current} at NetEarthOne, not {wanted}: check the domain before renewing",
                r.name
            )));
        }
        // `auto-renew` is required: keep what the order has.
        let Some(recurring) = order.recurring else {
            return Err(Error::refused(format!(
                "NetEarthOne didn't say whether {} renews automatically (needed to renew without changing it)",
                r.name
            )));
        };
        let done = self
            .post(
                "/domains/renew.json",
                &[
                    ("order-id", id.clone()),
                    ("years", r.years.to_string()),
                    ("exp-date", order.endtime.to_string()),
                    ("invoice-option", INVOICE_OPTION.to_owned()),
                    ("auto-renew", recurring.to_string()),
                ],
            )
            .await?;
        let action = text(&done["actionstatus"]).unwrap_or_default();
        tracing::info!(name = %r.name, order = %id, years = r.years, %action, "renew sent");
        // The new expiry date, once the order shows it.
        for attempt in 0..=self.config.rechecks {
            if attempt > 0 {
                tokio::time::sleep(self.config.recheck_every).await;
            }
            if let Ok(now) = self.order(&id).await
                && now.endtime > order.endtime
            {
                return Ok(RenewAnswer {
                    expires_on: utc_date(now.endtime),
                });
            }
        }
        // Not moved yet: don't guess a date and don't let ferrobill send it again by itself.
        Err(Error::refused(format!(
            "{} renewal sent (order {id}, action status {:?}, action {}), but the expiry date \
             still shows {current}: check the order at NetEarthOne before renewing again",
            r.name,
            action,
            text(&done["eaqid"]).unwrap_or_else(|| "?".into()),
        )))
    }
}

#[cfg(test)]
mod tests;
