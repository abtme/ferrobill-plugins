// SPDX-License-Identifier: Apache-2.0
//! The plugin against a fake HestiaCP API (`/api/`: hash auth, `cmd` + `argN`, `returncode=yes`
//! answers the return code, otherwise the command's output), called the way ferrobill calls it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::{Router, body::Bytes, extract::State, http::StatusCode, routing::post};
use ferrobill_hestiacp::{Hestia, ServerConfig};
use ferrobill_plugin_sdk::{
    Action, ActionRequest, Customer, Error, Job, Product, Server, Service, client::Client, router,
};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone)]
struct User {
    package: String,
    suspended: bool,
    password: String,
    domains: Vec<String>,
}

#[derive(Default)]
struct Fake {
    users: Mutex<BTreeMap<String, User>>,
    changes: Mutex<Vec<String>>,
    /// Fail the next v-add-domain with this code (a create that stops half way).
    fail_domain: Mutex<Option<i32>>,
}

async fn api(State(fake): State<Arc<Fake>>, body: Bytes) -> (StatusCode, String) {
    let f: BTreeMap<String, String> = serde_urlencoded::from_bytes(&body).unwrap();
    if f.get("hash").map(String::as_str) != Some("AK:SK") {
        return (
            StatusCode::UNAUTHORIZED,
            "Error: authentication failed".into(),
        );
    }
    let arg = |i: usize| f.get(&format!("arg{i}")).cloned().unwrap_or_default();
    let rc = f.get("returncode").map(String::as_str) == Some("yes");
    let mut users = fake.users.lock().unwrap();
    let mut changes = fake.changes.lock().unwrap();
    let code = |c: i32| (StatusCode::OK, c.to_string());
    assert!(
        rc || f["cmd"].starts_with("v-list-"),
        "change commands use returncode"
    );
    match f["cmd"].as_str() {
        "v-list-users" => {
            let o: Map<String, Value> = users
                .iter()
                .map(|(k, u)| {
                    (
                        k.clone(),
                        json!({"PACKAGE": u.package,
                        "SUSPENDED": if u.suspended { "yes" } else { "no" }}),
                    )
                })
                .collect();
            (StatusCode::OK, Value::Object(o).to_string())
        }
        "v-list-web-domains" => match users.get(&arg(1)) {
            Some(u) => {
                let o: Map<String, Value> =
                    u.domains.iter().map(|d| (d.clone(), json!({}))).collect();
                (StatusCode::OK, Value::Object(o).to_string())
            }
            None => (StatusCode::BAD_REQUEST, "Error: user doesn't exist".into()),
        },
        "v-add-user" => {
            if users.contains_key(&arg(1)) {
                return code(4);
            }
            assert!(arg(3).contains('@'));
            users.insert(
                arg(1),
                User {
                    package: arg(4),
                    suspended: false,
                    password: arg(2),
                    domains: vec![],
                },
            );
            changes.push(format!("add-user {}", arg(1)));
            code(0)
        }
        "v-add-domain" => {
            if let Some(c) = fake.fail_domain.lock().unwrap().take() {
                return code(c);
            }
            let Some(u) = users.get_mut(&arg(1)) else {
                return code(3);
            };
            if u.domains.contains(&arg(2)) {
                return code(4);
            }
            u.domains.push(arg(2));
            changes.push(format!("add-domain {} {}", arg(1), arg(2)));
            code(0)
        }
        "v-suspend-user" | "v-unsuspend-user" => {
            let want = f["cmd"] == "v-suspend-user";
            let Some(u) = users.get_mut(&arg(1)) else {
                return code(3);
            };
            if u.suspended == want {
                return code(if want { 5 } else { 6 });
            }
            u.suspended = want;
            changes.push(format!("{} {}", f["cmd"], arg(1)));
            code(0)
        }
        "v-change-user-package" => {
            let Some(u) = users.get_mut(&arg(1)) else {
                return code(3);
            };
            u.package = arg(2);
            changes.push(format!("package {} {}", arg(1), arg(2)));
            code(0)
        }
        "v-change-user-password" => {
            let Some(u) = users.get_mut(&arg(1)) else {
                return code(3);
            };
            u.password = arg(2);
            code(0)
        }
        "v-delete-user" => {
            if users.remove(&arg(1)).is_none() {
                return code(3);
            }
            changes.push(format!("delete {}", arg(1)));
            code(0)
        }
        _ => code(1),
    }
}

async fn start_fake() -> (Arc<Fake>, String) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/api/", post(api))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (fake, url)
}

async fn start_plugin(url: &str, secret_key: &str) -> Client {
    let mut servers = BTreeMap::new();
    servers.insert(
        "h1".to_owned(),
        ServerConfig {
            url: url.to_owned(),
            access_key: "AK".into(),
            secret_key: secret_key.into(),
            ip: String::new(),
        },
    );
    let plugin = Hestia::new(servers).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(plugin, "s")).await });
    Client::new(url, "s").unwrap()
}

fn job(action: Action, provisioning: &Map<String, Value>, package: &str) -> Job {
    Job {
        job_id: "j".into(),
        action,
        service: Service {
            id: "01HSERVICE".into(),
            label: String::new(),
            quantity: 1,
            status: "active".into(),
            metadata: json!({"domain": "acme-shop.example"})
                .as_object()
                .unwrap()
                .clone(),
            provisioning: provisioning.clone(),
        },
        customer: Customer {
            id: "c".into(),
            name: "Acme Ltd".into(),
            email: Some("ap@acme.example".into()),
        },
        product: Product {
            id: "p".into(),
            name: "Hosting".into(),
        },
        config: json!({"package": package}),
        previous_config: None,
        server: Some(Server { name: "h1".into() }),
    }
}

#[tokio::test]
async fn a_service_lifecycle_every_job_twice_and_a_half_done_create() {
    let (fake, url) = start_fake().await;
    let c = start_plugin(&url, "SK").await;

    // The first create stops after v-add-user (the domain fails, retryably); the retry finishes it.
    *fake.fail_domain.lock().unwrap() = Some(20);
    let r = c
        .provision(&job(Action::Create, &Map::new(), "default"))
        .await;
    assert!(matches!(r, Err(Error::Retry(_))), "{r:?}");
    let data = c
        .provision(&job(Action::Create, &Map::new(), "default"))
        .await
        .unwrap();
    let user = data["username"].as_str().unwrap().to_owned();
    assert!(user.starts_with("acmeshop"), "{user}");
    assert_eq!(
        c.provision(&job(Action::Create, &Map::new(), "default"))
            .await
            .unwrap(),
        data
    );
    assert_eq!(
        fake.users.lock().unwrap()[&user].domains,
        ["acme-shop.example"]
    );

    for a in [
        Action::Suspend,
        Action::Suspend,
        Action::Unsuspend,
        Action::Unsuspend,
    ] {
        c.provision(&job(a, &data, "default")).await.unwrap();
    }
    for _ in 0..2 {
        c.provision(&job(Action::ChangePackage, &data, "pro"))
            .await
            .unwrap();
    }
    let a = c
        .action(&ActionRequest {
            action: "reset_password".into(),
            service: job(Action::Create, &data, "pro").service,
            customer: job(Action::Create, &data, "pro").customer,
            product: job(Action::Create, &data, "pro").product,
            config: json!({"package": "pro"}),
            server: None,
            by: "client".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        fake.users.lock().unwrap()[&user].password,
        a.secret.unwrap()
    );
    for _ in 0..2 {
        c.provision(&job(Action::Terminate, &data, "pro"))
            .await
            .unwrap();
    }
    assert_eq!(
        *fake.changes.lock().unwrap(),
        [
            format!("add-user {user}"),
            format!("add-domain {user} acme-shop.example"),
            format!("v-suspend-user {user}"),
            format!("v-unsuspend-user {user}"),
            format!("package {user} pro"),
            format!("delete {user}"),
        ]
    );
}

#[tokio::test]
async fn refusals_and_retries() {
    let (fake, url) = start_fake().await;
    let c = start_plugin(&url, "SK").await;
    // Someone else's account under the chosen name: refused.
    fake.users.lock().unwrap().insert(
        "taken".into(),
        User {
            package: "default".into(),
            suspended: false,
            password: String::new(),
            domains: vec!["other.example".into()],
        },
    );
    let mut j = job(Action::Create, &Map::new(), "default");
    j.service.metadata.insert("username".into(), json!("taken"));
    let r = c.provision(&j).await;
    assert!(
        matches!(r, Err(Error::Refused(ref d)) if d.contains("already taken")),
        "{r:?}"
    );
    // A HestiaCP refusal (bad package, code 2 E_INVALID-like) is refused, not retried.
    *fake.fail_domain.lock().unwrap() = Some(2);
    let r = c
        .provision(&job(Action::Create, &Map::new(), "default"))
        .await;
    assert!(
        matches!(r, Err(Error::Refused(ref d)) if d.contains("v-add-domain")),
        "{r:?}"
    );
    // A wrong key: retried.
    let bad = start_plugin(&url, "WRONG").await;
    let r = bad
        .provision(&job(Action::Create, &Map::new(), "default"))
        .await;
    assert!(
        matches!(r, Err(Error::Retry(ref d)) if d.contains("access key")),
        "{r:?}"
    );
}
