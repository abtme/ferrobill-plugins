// SPDX-License-Identifier: Apache-2.0
//! Tests against a fake of NetEarthOne's HTTP API.

use super::*;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};

const KEY: &str = "k3y-s3cret";

/// 2027-02-02 12:34:56 UTC.
const FEB2: i64 = 1_801_571_696;

#[derive(Default)]
struct Fake {
    /// name -> (order ID, endtime, auto-renew)
    orders: Vec<(String, u64, i64, bool)>,
    /// Every renew.json form.
    renews: Vec<HashMap<String, String>>,
    /// Renewals are accepted but the expiry date doesn't move (pending at the registry).
    pending: bool,
    /// Answer everything with HTTP 502 and an HTML page.
    down: bool,
}

type S = Arc<Mutex<Fake>>;

fn api_error(msg: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"status": "ERROR", "message": msg})),
    )
        .into_response()
}

fn authed(q: &HashMap<String, String>) -> bool {
    q.get("auth-userid").map(String::as_str) == Some("12345")
        && q.get("api-key").map(String::as_str) == Some(KEY)
}

fn gate(s: &Fake, q: &HashMap<String, String>) -> Option<Response> {
    if s.down {
        return Some((StatusCode::BAD_GATEWAY, "<html>bad gateway</html>").into_response());
    }
    if !authed(q) {
        return Some(api_error("Invalid auth-userid or api-key"));
    }
    None
}

async fn search(State(s): State<S>, Query(q): Query<HashMap<String, String>>) -> Response {
    let s = s.lock().unwrap();
    if let Some(r) = gate(&s, &q) {
        return r;
    }
    let size: usize = q["no-of-records"].parse().unwrap();
    let page: usize = q["page-no"].parse().unwrap();
    let mut out = serde_json::Map::new();
    let rows: Vec<_> = s.orders.iter().skip((page - 1) * size).take(size).collect();
    out.insert("recsindb".into(), json!(s.orders.len().to_string()));
    out.insert("recsonpage".into(), json!(rows.len().to_string()));
    for (i, (name, id, end, auto)) in rows.into_iter().enumerate() {
        let status = if name.starts_with("gone") {
            "Deleted"
        } else {
            "Active"
        };
        out.insert(
            (i + 1).to_string(),
            json!({"entity.description": name, "orders.orderid": id.to_string(),
                   "orders.endtime": end.to_string(), "orders.autorenew": auto.to_string(),
                   "entity.currentstatus": status}),
        );
    }
    Json(Value::Object(out)).into_response()
}

async fn orderid(State(s): State<S>, Query(q): Query<HashMap<String, String>>) -> Response {
    let s = s.lock().unwrap();
    if let Some(r) = gate(&s, &q) {
        return r;
    }
    let name = &q["domain-name"];
    match s.orders.iter().find(|o| &o.0 == name) {
        Some(o) => Json(json!(o.1)).into_response(),
        None => api_error(&format!("Website doesn't exist for {name}")),
    }
}

async fn details(State(s): State<S>, Query(q): Query<HashMap<String, String>>) -> Response {
    let s = s.lock().unwrap();
    if let Some(r) = gate(&s, &q) {
        return r;
    }
    assert_eq!(q["options"], "OrderDetails");
    match s.orders.iter().find(|o| o.1.to_string() == q["order-id"]) {
        Some((name, id, end, auto)) => Json(json!({"orderid": id.to_string(),
            "domainname": name, "endtime": end.to_string(), "recurring": auto.to_string()}))
        .into_response(),
        None => api_error("You are not allowed to perform this action"),
    }
}

async fn renew(State(s): State<S>, body: Bytes) -> Response {
    let form: HashMap<String, String> = serde_urlencoded::from_bytes(&body).unwrap();
    let mut s = s.lock().unwrap();
    if let Some(r) = gate(&s, &form) {
        return r;
    }
    s.renews.push(form.clone());
    let pending = s.pending;
    let Some(o) = s
        .orders
        .iter_mut()
        .find(|o| o.1.to_string() == form["order-id"])
    else {
        return api_error("You are not allowed to perform this action");
    };
    if form["exp-date"] != o.2.to_string() {
        return api_error("Invalid expiry date");
    }
    assert_eq!(form["invoice-option"], "NoInvoice");
    assert_eq!(form["auto-renew"], o.3.to_string());
    let years: i64 = form["years"].parse().unwrap();
    if !pending {
        let days = o.2.div_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        o.2 = days_from_civil(y + years, m, d) * 86_400 + o.2.rem_euclid(86_400);
    }
    Json(json!({"entityid": o.1.to_string(), "actionstatus": "Success", "eaqid": "777"}))
        .into_response()
}

async fn fake(state: Fake) -> (String, S) {
    let s: S = Arc::new(Mutex::new(state));
    let app = Router::new()
        .route("/api/domains/search.json", get(search))
        .route("/api/domains/orderid.json", get(orderid))
        .route("/api/domains/details.json", get(details))
        .route("/api/domains/renew.json", post(renew))
        .with_state(s.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, s)
}

fn plugin(url: &str) -> NetEarthOne {
    NetEarthOne::new(Config {
        url: url.into(),
        auth_userid: "12345".into(),
        api_key: KEY.into(),
        recheck_every: Duration::from_millis(5),
        rechecks: 2,
    })
    .unwrap()
}

fn one(name: &str, id: u64, end: i64) -> (String, u64, i64, bool) {
    (name.into(), id, end, false)
}

fn renew_req(name: &str, from: &str, years: u32) -> RenewRequest {
    RenewRequest {
        name: name.into(),
        years,
        expires_on: from.into(),
    }
}

#[test]
fn dates() {
    assert_eq!(utc_date(0), "1970-01-01");
    assert_eq!(utc_date(FEB2), "2027-02-02");
    // The last second of a day and the first of the next.
    assert_eq!(utc_date(1_801_612_799), "2027-02-02");
    assert_eq!(utc_date(1_801_612_800), "2027-02-03");
    // The owner's real order: 1797524467 is 2026-12-17 16:21:07 UTC.
    assert_eq!(utc_date(1_797_524_467), "2026-12-17");
    assert_eq!(utc_date(951_782_400), "2000-02-29");
    assert_eq!(utc_date(-86_400), "1969-12-31");
    for days in [-800_000, -1, 0, 1, 11_016, 20_000, 2_932_896] {
        let (y, m, d) = civil_from_days(days);
        assert_eq!(days_from_civil(y, m, d), days);
    }
    assert_eq!(days_from_civil(2027, 2, 2) * 86_400, FEB2 - 45_296);
    assert_eq!(valid_date("2027-02-02"), Some("2027-02-02"));
    assert_eq!(valid_date("2028-02-29"), Some("2028-02-29"));
    assert_eq!(valid_date("2027-02-29"), None);
    assert_eq!(valid_date("2027-13-01"), None);
    assert_eq!(valid_date("2027-2-2"), None);
}

#[test]
fn api_answers_and_errors() {
    // The API's own errors are refused even with HTTP 500.
    let e = check(
        "/x",
        500,
        r#"{"status":"ERROR","message":"Website doesn't exist"}"#,
    );
    assert_eq!(
        e,
        Err(Error::refused("NetEarthOne /x: Website doesn't exist"))
    );
    let e = check(
        "/x",
        200,
        r#"{"status":"error","error":"Invalid exp-date"}"#,
    );
    assert!(matches!(e, Err(Error::Refused(m)) if m.ends_with("Invalid exp-date")));
    assert!(matches!(
        check("/x", 503, "<html>down</html>"),
        Err(Error::Retry(_))
    ));
    assert!(matches!(
        check("/x", 403, "Forbidden"),
        Err(Error::Refused(_))
    ));
    assert!(matches!(check("/x", 200, "<html>"), Err(Error::Retry(_))));
    assert_eq!(check("/x", 200, "94384819"), Ok(json!(94384819)));
    assert_eq!(short(&"a".repeat(300)).len(), 203);
    assert_eq!(text(&json!("12 ")), Some("12".into()));
    assert_eq!(text(&json!(12)), Some("12".into()));
    assert_eq!(text(&json!("")), None);
}

#[test]
fn manifest() {
    let m = plugin("http://127.0.0.1:1/api").manifest();
    assert_eq!(m["name"], "netearthone");
    assert_eq!(m["kinds"], json!(["registrar"]));
    assert_eq!(
        m["registrar_operations"],
        json!(["domains", "info", "renew"])
    );
}

#[tokio::test]
async fn domains_page_through_the_search() {
    let orders = (0..1203)
        .map(|i| {
            let name = if i == 3 {
                "gone.com".to_owned()
            } else {
                format!("D{i}.com")
            };
            one(&name, 1000 + i as u64, FEB2 + i64::from(i) * 86_400)
        })
        .collect();
    let (url, _) = fake(Fake {
        orders,
        ..Fake::default()
    })
    .await;
    let p = plugin(&url);
    let first = p.domains(DomainsRequest::default()).await.unwrap();
    assert_eq!(first.domains.len(), 499, "the deleted one is left out");
    assert_eq!(first.domains[0].name, "d0.com");
    assert_eq!(first.domains[0].expires_on.as_deref(), Some("2027-02-02"));
    assert_eq!(first.domains[3].name, "d4.com");
    assert_eq!(first.domains[9].name, "d10.com", "in the API's order");
    assert!(first.more);
    assert_eq!(first.cursor.as_deref(), Some("2"));
    let second = p
        .domains(DomainsRequest {
            cursor: first.cursor,
        })
        .await
        .unwrap();
    assert_eq!(second.domains[0].name, "d500.com");
    assert_eq!(second.cursor.as_deref(), Some("3"));
    let third = p
        .domains(DomainsRequest {
            cursor: second.cursor,
        })
        .await
        .unwrap();
    assert_eq!(third.domains.len(), 203);
    assert!(!third.more);
    assert_eq!(third.cursor, None);
    assert!(matches!(
        p.domains(DomainsRequest {
            cursor: Some("x".into())
        })
        .await,
        Err(Error::Refused(_))
    ));
}

#[tokio::test]
async fn info_reads_the_order() {
    let (url, _) = fake(Fake {
        orders: vec![one("a.com", 11, FEB2)],
        ..Fake::default()
    })
    .await;
    let p = plugin(&url);
    let i = p
        .info(InfoRequest {
            name: "a.com".into(),
        })
        .await
        .unwrap();
    assert_eq!(i.expires_on, "2027-02-02");
    let e = p
        .info(InfoRequest {
            name: "b.com".into(),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Refused(m) if m.contains("doesn't exist for b.com")),
        "{e}"
    );
}

#[tokio::test]
async fn a_renewal_is_safe_to_repeat() {
    let (url, s) = fake(Fake {
        orders: vec![one("a.com", 11, FEB2), ("auto.com".into(), 12, FEB2, true)],
        ..Fake::default()
    })
    .await;
    let p = plugin(&url);
    let r = p.renew(renew_req("a.com", "2027-02-02", 2)).await.unwrap();
    assert_eq!(r.expires_on, "2029-02-02");
    // Asked again (an answer that went missing): already renewed, not renewed twice.
    let r = p.renew(renew_req("a.com", "2027-02-02", 2)).await.unwrap();
    assert_eq!(r.expires_on, "2029-02-02");
    {
        let s = s.lock().unwrap();
        assert_eq!(s.renews.len(), 1);
        let f = &s.renews[0];
        assert_eq!(f["exp-date"], FEB2.to_string(), "the exact current expiry");
        assert_eq!(f["years"], "2");
        assert_eq!(f["order-id"], "11");
        assert_eq!(f["auto-renew"], "false");
    }
    // Auto-renewal is kept as it was.
    let r = p
        .renew(renew_req("auto.com", "2027-02-02", 1))
        .await
        .unwrap();
    assert_eq!(r.expires_on, "2028-02-02");
    assert_eq!(s.lock().unwrap().renews[1]["auto-renew"], "true");
}

#[tokio::test]
async fn renewals_that_are_refused() {
    let (url, s) = fake(Fake {
        orders: vec![one("a.com", 11, FEB2)],
        ..Fake::default()
    })
    .await;
    let p = plugin(&url);
    // A date later than the registry's: refused, nothing renewed.
    let e = p
        .renew(renew_req("a.com", "2028-02-02", 1))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Refused(m) if m.contains("check the domain")),
        "{e}"
    );
    // Not the account's domain.
    let e = p
        .renew(renew_req("b.com", "2027-02-02", 1))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    for bad in [
        renew_req("a.com", "2027-02-02", 0),
        renew_req("a.com", "2027-02-02", 11),
        renew_req("a.com", "2027-2-2", 1),
        renew_req("a.com", "2027-02-30", 1),
    ] {
        assert!(matches!(p.renew(bad).await, Err(Error::Refused(_))));
    }
    assert!(s.lock().unwrap().renews.is_empty());
}

#[tokio::test]
async fn a_renewal_whose_date_does_not_move_is_not_guessed() {
    let (url, s) = fake(Fake {
        orders: vec![one("a.com", 11, FEB2)],
        pending: true,
        ..Fake::default()
    })
    .await;
    let p = plugin(&url);
    let e = p
        .renew(renew_req("a.com", "2027-02-02", 1))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Refused(m) if m.contains("still shows 2027-02-02") && m.contains("777")),
        "{e}"
    );
    assert_eq!(s.lock().unwrap().renews.len(), 1);
}

#[tokio::test]
async fn errors_retry_or_refuse_and_never_show_the_key() {
    let (url, s) = fake(Fake {
        orders: vec![one("a.com", 11, FEB2)],
        down: true,
        ..Fake::default()
    })
    .await;
    let p = plugin(&url);
    let e = p
        .info(InfoRequest {
            name: "a.com".into(),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Retry(m) if m.contains("HTTP 502")),
        "{e}"
    );
    assert!(!e.detail().contains(KEY));
    s.lock().unwrap().down = false;

    // A wrong key: the API's error, refused.
    let mut wrong = plugin(&url);
    wrong.config.api_key = "nope".into();
    let e = wrong.domains(DomainsRequest::default()).await.unwrap_err();
    assert!(
        matches!(&e, Error::Refused(m) if m.contains("Invalid auth-userid")),
        "{e}"
    );

    // Nothing listening: retried, and the message has no URL (it would hold the key).
    let dead = plugin("http://127.0.0.1:1/api");
    let e = dead
        .info(InfoRequest {
            name: "a.com".into(),
        })
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Retry(_)), "{e}");
    assert!(
        !e.detail().contains(KEY) && !e.detail().contains("api-key"),
        "{e}"
    );

    // The key echoed back in an answer is blanked out.
    assert_eq!(p.redact(&format!("bad key {KEY}")), "bad key [api-key]");
}

/// Read-only against the live API (never renews: that costs money):
/// `NETEARTHONE_AUTH_USERID=... NETEARTHONE_API_KEY=... cargo test -p ferrobill-netearthone
/// live_read_only -- --ignored --nocapture`. Lists the first page, then reads the first
/// domain's expiry through `info` and checks they agree.
#[tokio::test]
#[ignore]
async fn live_read_only() {
    let p = NetEarthOne::new(Config::from_env().expect("NETEARTHONE_* settings")).unwrap();
    let page = p.domains(DomainsRequest::default()).await.unwrap();
    println!(
        "{} domains on page 1, more: {}",
        page.domains.len(),
        page.more
    );
    let first = page.domains.first().expect("at least one domain");
    let info = p
        .info(InfoRequest {
            name: first.name.clone(),
        })
        .await
        .unwrap();
    println!("{}: expires {}", first.name, info.expires_on);
    assert_eq!(Some(info.expires_on), first.expires_on.clone());
    let e = p
        .info(InfoRequest {
            name: "no-such-domain-ferrobill-test.com".into(),
        })
        .await
        .unwrap_err();
    println!("unknown domain: {e}");
    assert!(matches!(e, Error::Refused(_)));
}
