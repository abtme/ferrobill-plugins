// SPDX-License-Identifier: Apache-2.0
//! The plugin against a fake Gandi v5 API (the routes it uses, answering as Gandi documents
//! them at https://api.gandi.net/docs/domains/): the paged listing with its Total-Count header,
//! domain details, and renewals that Gandi accepts (202) before doing them.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ferrobill_gandi::{Config, Gandi};
use ferrobill_plugin_sdk::Error;
use ferrobill_plugin_sdk::registrar::{
    DomainsRequest, InfoRequest, ListedDomain, Registrar, RenewRequest,
};
use serde_json::{Value, json};

const TOKEN: &str = "pat_good";

#[derive(Default)]
struct Fake {
    /// fqdn -> registry_ends_at.
    domains: Mutex<BTreeMap<String, String>>,
    /// Renewals accepted but not done yet: fqdn -> (years, reads of the domain left before
    /// it is done).
    queued: Mutex<BTreeMap<String, (i64, u32)>>,
    /// How many reads of the domain a renewal takes to be done (0: done at once).
    delay: Mutex<u32>,
    /// Every renewal request: (fqdn, query, body).
    renewals: Mutex<Vec<(String, String, Value)>>,
    /// Every listing query.
    lists: Mutex<Vec<String>>,
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"cause": "x", "code": status.as_u16(), "message": message, "object": "y"})),
    )
        .into_response()
}

fn authorized(h: &HeaderMap) -> Option<Response> {
    match h.get("authorization").and_then(|v| v.to_str().ok()) {
        Some(v) if v == format!("Bearer {TOKEN}") => None,
        _ => Some(error(StatusCode::UNAUTHORIZED, "Bad authentication")),
    }
}

fn param(q: &str, k: &str) -> Option<String> {
    q.split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{k}=")).map(str::to_owned))
}

async fn list(State(f): State<Arc<Fake>>, h: HeaderMap, RawQuery(q): RawQuery) -> Response {
    if let Some(r) = authorized(&h) {
        return r;
    }
    let q = q.unwrap_or_default();
    f.lists.lock().unwrap().push(q.clone());
    let page: usize = param(&q, "page").map_or(1, |p| p.parse().unwrap());
    let per: usize = param(&q, "per_page").map_or(100, |p| p.parse().unwrap());
    let d = f.domains.lock().unwrap();
    let items: Vec<Value> = d
        .iter()
        .skip((page - 1) * per)
        .take(per)
        .map(|(n, e)| json!({"fqdn": n, "tld": "x", "dates": {"registry_ends_at": e}}))
        .collect();
    let n = d.len().to_string();
    (
        [("Total-Count", n.clone()), ("Filtered-Count", n)],
        Json(Value::Array(items)),
    )
        .into_response()
}

async fn details(State(f): State<Arc<Fake>>, h: HeaderMap, Path(fqdn): Path<String>) -> Response {
    if let Some(r) = authorized(&h) {
        return r;
    }
    match fqdn.as_str() {
        "down.com" => return error(StatusCode::BAD_GATEWAY, "upstream"),
        "busy.com" => return error(StatusCode::TOO_MANY_REQUESTS, "slow down"),
        _ => {}
    }
    // A queued renewal is done after enough reads.
    {
        let mut q = f.queued.lock().unwrap();
        if let Some((years, left)) = q.get_mut(&fqdn) {
            if *left == 0 {
                let years = *years;
                q.remove(&fqdn);
                let mut d = f.domains.lock().unwrap();
                let e = d.get_mut(&fqdn).unwrap();
                let y: i64 = e[..4].parse().unwrap();
                *e = format!("{}{}", y + years, &e[4..]);
            } else {
                *left -= 1;
            }
        }
    }
    match f.domains.lock().unwrap().get(&fqdn) {
        Some(e) => Json(json!({"fqdn": fqdn, "dates": {
            "registry_created_at": "2020-01-01T00:00:00Z", "updated_at": "2020-01-01T00:00:00Z",
            "registry_ends_at": e}}))
        .into_response(),
        None => error(StatusCode::NOT_FOUND, "The domain does not exist"),
    }
}

async fn renew(
    State(f): State<Arc<Fake>>,
    h: HeaderMap,
    Path(fqdn): Path<String>,
    RawQuery(q): RawQuery,
    body: Bytes,
) -> Response {
    if let Some(r) = authorized(&h) {
        return r;
    }
    let body: Value = serde_json::from_slice(&body).unwrap();
    f.renewals
        .lock()
        .unwrap()
        .push((fqdn.clone(), q.unwrap_or_default(), body.clone()));
    if fqdn == "poor.com" {
        return error(StatusCode::PAYMENT_REQUIRED, "Not enough prepaid credit");
    }
    if fqdn == "flaky.com" {
        return error(StatusCode::SERVICE_UNAVAILABLE, "maintenance");
    }
    let years = body["duration"].as_i64().unwrap_or(1);
    if years > 9 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"status": "error", "errors": [
                {"location": "body", "name": "duration", "description": "must be <= 9"}]})),
        )
            .into_response();
    }
    let delay = *f.delay.lock().unwrap();
    f.queued.lock().unwrap().insert(fqdn, (years, delay));
    (
        StatusCode::ACCEPTED,
        Json(json!({"message": "The request has been accepted."})),
    )
        .into_response()
}

async fn fake(domains: &[(&str, &str)]) -> (Arc<Fake>, String) {
    let f = Arc::new(Fake::default());
    for (n, e) in domains {
        f.domains.lock().unwrap().insert((*n).into(), (*e).into());
    }
    let app = Router::new()
        .route("/v5/domain/domains", get(list))
        .route("/v5/domain/domains/{fqdn}", get(details))
        .route("/v5/domain/domains/{fqdn}/renew", post(renew))
        .with_state(f.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v5", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (f, url)
}

fn plugin(url: &str) -> Gandi {
    let mut c = Config::new(url, TOKEN);
    c.poll_every = Duration::from_millis(5);
    c.polls = 3;
    Gandi::new(c).unwrap()
}

fn info(name: &str) -> InfoRequest {
    InfoRequest { name: name.into() }
}

fn renew_req(name: &str, from: &str, years: u32) -> RenewRequest {
    RenewRequest {
        name: name.into(),
        years,
        expires_on: from.into(),
    }
}

#[tokio::test]
async fn the_listing_is_paged() {
    let (f, url) = fake(&[
        ("a.com", "2027-01-01T10:00:00Z"),
        ("B.net", "2027-02-02T00:00:00Z"),
        ("c.org", "2028-03-03T00:00:00Z"),
        ("d.uk", "2029-04-04T00:00:00Z"),
        ("e.fr", "2030-05-05T00:00:00Z"),
    ])
    .await;
    let mut c = Config::new(&url, TOKEN);
    c.per_page = 2;
    c.sharing_id = Some("org-1".into());
    let g = Gandi::new(c).unwrap();
    let mut all = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let p = g.domains(DomainsRequest { cursor }).await.unwrap();
        pages += 1;
        all.extend(p.domains);
        if !p.more {
            assert_eq!(p.cursor, None);
            break;
        }
        cursor = p.cursor;
    }
    assert_eq!(pages, 3);
    assert_eq!(all.len(), 5);
    // Names in lower case, dates as YYYY-MM-DD.
    assert_eq!(
        all[0],
        ListedDomain {
            name: "b.net".into(),
            expires_on: Some("2027-02-02".into())
        }
    );
    assert_eq!(all[1].expires_on.as_deref(), Some("2027-01-01"));
    {
        let lists = f.lists.lock().unwrap();
        assert!(lists[2].contains("page=3"), "{lists:?}");
        assert!(lists.iter().all(|q| q.contains("per_page=2")));
        assert!(lists.iter().all(|q| q.contains("sharing_id=org-1")));
    }

    // A bad cursor is refused; a page past the end is empty and the last.
    assert!(matches!(
        g.domains(DomainsRequest {
            cursor: Some("x".into())
        })
        .await,
        Err(Error::Refused(_))
    ));
    let p = g
        .domains(DomainsRequest {
            cursor: Some("9".into()),
        })
        .await
        .unwrap();
    assert!(p.domains.is_empty() && !p.more);
}

#[tokio::test]
async fn info_and_errors() {
    let (_f, url) = fake(&[("a.com", "2027-02-02T13:14:15Z")]).await;
    let g = plugin(&url);
    let i = g.info(info("A.com.")).await.unwrap();
    assert_eq!(i.expires_on, "2027-02-02");

    let e = g.info(info("missing.com")).await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    assert!(e.detail().contains("does not exist"), "{e}");
    for name in ["down.com", "busy.com"] {
        let e = g.info(info(name)).await.unwrap_err();
        assert!(matches!(e, Error::Retry(_)), "{name}: {e}");
    }
    assert!(matches!(
        g.info(info("a.com/../x")).await,
        Err(Error::Refused(_))
    ));

    // A wrong token: refused with Gandi's message.
    let bad = Gandi::new(Config::new(&url, "pat_wrong")).unwrap();
    let e = bad.info(info("a.com")).await.unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    assert!(e.detail().contains("Bad authentication"), "{e}");

    // Nothing listening: retry.
    let gone = Gandi::new(Config::new("http://127.0.0.1:1/v5", TOKEN)).unwrap();
    assert!(matches!(
        gone.info(info("a.com")).await,
        Err(Error::Retry(_))
    ));
}

#[tokio::test]
async fn a_renewal_is_safe_to_repeat() {
    let (f, url) = fake(&[("a.com", "2027-02-02T13:14:15Z")]).await;
    let mut c = Config::new(&url, TOKEN);
    c.sharing_id = Some("org-1".into());
    c.poll_every = Duration::from_millis(5);
    let g = Gandi::new(c).unwrap();
    let r = g.renew(renew_req("a.com", "2027-02-02", 2)).await.unwrap();
    assert_eq!(r.expires_on, "2029-02-02");
    // Asked again (an answer that went missing): already renewed, not renewed twice.
    let again = g.renew(renew_req("a.com", "2027-02-02", 2)).await.unwrap();
    assert_eq!(again, r);
    {
        let renewals = f.renewals.lock().unwrap();
        assert_eq!(renewals.len(), 1);
        assert_eq!(renewals[0].0, "a.com");
        assert_eq!(renewals[0].1, "sharing_id=org-1");
        assert_eq!(renewals[0].2, json!({"duration": 2}));
    }
    // A date later than Gandi's: refused, nothing renewed.
    let e = g
        .renew(renew_req("a.com", "2030-02-02", 1))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    assert!(e.detail().contains("check the domain"), "{e}");
    // Out of range or malformed: refused before asking Gandi.
    for r in [
        renew_req("a.com", "2029-02-02", 0),
        renew_req("a.com", "2029-02-02", 10),
        renew_req("a.com", "2029-2-2", 1),
    ] {
        assert!(matches!(g.renew(r).await, Err(Error::Refused(_))));
    }
    assert_eq!(f.renewals.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_asynchronous_renewal() {
    let (f, url) = fake(&[("a.com", "2027-02-02T00:00:00Z")]).await;
    let g = plugin(&url);

    // Done after two more reads: the plugin waits for it.
    *f.delay.lock().unwrap() = 2;
    let r = g.renew(renew_req("a.com", "2027-02-02", 1)).await.unwrap();
    assert_eq!(r.expires_on, "2028-02-02");

    // Slower than the plugin waits (3 polls): retry, and the repeat doesn't renew again...
    *f.delay.lock().unwrap() = 6;
    let e = g
        .renew(renew_req("a.com", "2028-02-02", 1))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Retry(_)), "{e}");
    assert_eq!(f.renewals.lock().unwrap().len(), 2);
    // ...it answers retry (pending) until Gandi shows the new date, then that date.
    let mut tries = 0;
    let r = loop {
        tries += 1;
        assert!(tries < 10, "never done");
        match g.renew(renew_req("a.com", "2028-02-02", 1)).await {
            Ok(r) => break r,
            Err(e) => {
                assert!(matches!(e, Error::Retry(_)), "{e}");
                assert!(e.detail().contains("not renewing again"), "{e}");
            }
        }
    };
    assert!(tries > 1);
    assert_eq!(r.expires_on, "2029-02-02");
    assert_eq!(f.renewals.lock().unwrap().len(), 2, "renewed once");
}

#[tokio::test]
async fn renewal_refusals_and_retries() {
    let (f, url) = fake(&[
        ("poor.com", "2027-02-02T00:00:00Z"),
        ("flaky.com", "2027-02-02T00:00:00Z"),
    ])
    .await;
    let g = plugin(&url);
    let e = g
        .renew(renew_req("poor.com", "2027-02-02", 1))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    assert!(e.detail().contains("Not enough prepaid credit"), "{e}");
    let e = g
        .renew(renew_req("flaky.com", "2027-02-02", 1))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Retry(_)), "{e}");
    // Not accepted: a retry sends it again.
    let _ = g.renew(renew_req("flaky.com", "2027-02-02", 1)).await;
    assert_eq!(f.renewals.lock().unwrap().len(), 3);
    let e = g
        .renew(renew_req("missing.com", "2027-02-02", 1))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
}

/// Called the way ferrobill calls it: signed, over HTTP; refused is 422, retry 503.
#[tokio::test]
async fn through_the_signed_router() {
    use ferrobill_plugin_sdk::signature;
    let (_f, url) = fake(&[("a.com", "2027-02-02T00:00:00Z")]).await;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let router = ferrobill_plugin_sdk::registrar::router(plugin(&url), "s3cret");
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    let http = ferrobill_plugin_sdk::http_client(Duration::from_secs(5)).unwrap();
    let post = |path: &str, body: Value| {
        let b = body.to_string();
        let sig = signature::sign("s3cret", signature::now(), b.as_bytes());
        http.post(format!("{base}{path}"))
            .header(signature::HEADER, sig)
            .header("content-type", "application/json")
            .body(b)
            .send()
    };
    let r = post("/info", json!({"name": "a.com"})).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap()["expires_on"], "2027-02-02");
    let r = post("/info", json!({"name": "nope.com"})).await.unwrap();
    assert_eq!(r.status(), 422);
    let r = post("/info", json!({"name": "down.com"})).await.unwrap();
    assert_eq!(r.status(), 503);
    let r = post(
        "/renew",
        json!({"name": "a.com", "years": 1, "expires_on": "2027-02-02"}),
    )
    .await
    .unwrap();
    assert_eq!(r.json::<Value>().await.unwrap()["expires_on"], "2028-02-02");
    let r = post("/domains", json!({"cursor": null})).await.unwrap();
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["domains"][0]["name"], "a.com");
    assert_eq!(page["more"], false);
}
