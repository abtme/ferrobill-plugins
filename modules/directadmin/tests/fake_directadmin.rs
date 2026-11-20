// SPDX-License-Identifier: Apache-2.0
//! The plugin against a fake DirectAdmin (the CMD_API_* commands it uses, answering as
//! DirectAdmin does: url-encoded `error=0|1&text=...`), called the way ferrobill calls it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::{Router, body::Bytes, extract::State, http::HeaderMap, http::StatusCode, routing::post};
use ferrobill_directadmin::{DirectAdmin, ServerConfig};
use ferrobill_plugin_sdk::{
    Action, ActionRequest, Customer, Error, Job, Product, Server, Service, client::Client, router,
};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq)]
struct User {
    domain: String,
    package: String,
    suspended: bool,
    password: String,
}

#[derive(Default)]
struct Fake {
    users: Mutex<BTreeMap<String, User>>,
    /// Every command that changed something, in order.
    changes: Mutex<Vec<String>>,
}

fn ok() -> (StatusCode, String) {
    (StatusCode::OK, "error=0&text=Success&details=".into())
}

fn err(text: &str) -> (StatusCode, String) {
    (
        StatusCode::OK,
        serde_urlencoded::to_string([("error", "1"), ("text", text), ("details", "")]).unwrap(),
    )
}

async fn command(
    State((fake, cmd)): State<(Arc<Fake>, &'static str)>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Basic YWRtaW46azN5") {
        return (StatusCode::UNAUTHORIZED, "<html>login</html>".into());
    }
    let f: BTreeMap<String, String> = serde_urlencoded::from_bytes(&body).unwrap();
    let get = |k: &str| f.get(k).cloned().unwrap_or_default();
    let mut users = fake.users.lock().unwrap();
    match cmd {
        "CMD_API_SHOW_USER_CONFIG" => match users.get(&get("user")) {
            Some(u) => (
                StatusCode::OK,
                serde_urlencoded::to_string([
                    ("domain", u.domain.as_str()),
                    ("package", u.package.as_str()),
                    ("suspended", if u.suspended { "yes" } else { "no" }),
                    ("username", get("user").as_str()),
                ])
                .unwrap(),
            ),
            None => err("Unable to show user config"),
        },
        "CMD_API_ACCOUNT_USER" => {
            assert_eq!(get("action"), "create");
            assert_eq!(get("passwd"), get("passwd2"));
            assert_eq!(get("ip"), "192.0.2.10");
            assert_eq!(get("notify"), "no");
            if users.contains_key(&get("username")) {
                return err("That username already exists");
            }
            users.insert(
                get("username"),
                User {
                    domain: get("domain"),
                    package: get("package"),
                    suspended: false,
                    password: get("passwd"),
                },
            );
            fake.changes
                .lock()
                .unwrap()
                .push(format!("create {}", get("username")));
            ok()
        }
        "CMD_API_SELECT_USERS" => {
            let user = get("select0");
            let Some(u) = users.get_mut(&user) else {
                return err("no such user");
            };
            if get("delete") == "yes" && get("confirmed") == "Confirm" {
                users.remove(&user);
                fake.changes.lock().unwrap().push(format!("delete {user}"));
            } else if get("location") == "CMD_SELECT_USERS" {
                // Like DirectAdmin's older versions: the button toggles. The plugin must only
                // press it when the state differs.
                u.suspended = !u.suspended;
                fake.changes
                    .lock()
                    .unwrap()
                    .push(format!("{} {user}", get("suspend").to_lowercase()));
            } else {
                return err("bad request");
            }
            ok()
        }
        "CMD_API_MODIFY_USER" => {
            assert_eq!(get("action"), "package");
            let Some(u) = users.get_mut(&get("user")) else {
                return err("no such user");
            };
            u.package = get("package");
            fake.changes.lock().unwrap().push(format!(
                "package {} {}",
                get("user"),
                get("package")
            ));
            ok()
        }
        "CMD_API_USER_PASSWD" => {
            assert_eq!(get("passwd"), get("passwd2"));
            let Some(u) = users.get_mut(&get("username")) else {
                return err("no such user");
            };
            u.password = get("passwd");
            ok()
        }
        _ => (StatusCode::NOT_FOUND, String::new()),
    }
}

async fn start_fake() -> (Arc<Fake>, String) {
    let fake = Arc::new(Fake::default());
    let mut app = Router::new();
    for cmd in [
        "CMD_API_SHOW_USER_CONFIG",
        "CMD_API_ACCOUNT_USER",
        "CMD_API_SELECT_USERS",
        "CMD_API_MODIFY_USER",
        "CMD_API_USER_PASSWD",
    ] {
        app = app.route(
            &format!("/{cmd}"),
            post(command).with_state((fake.clone(), cmd)),
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (fake, url)
}

async fn start_plugin(da_url: &str, key: &str) -> Client {
    let mut servers = BTreeMap::new();
    servers.insert(
        "da1".to_owned(),
        ServerConfig {
            url: da_url.to_owned(),
            username: "admin".into(),
            login_key: key.into(),
            ip: "192.0.2.10".into(),
        },
    );
    let plugin = DirectAdmin::new(servers).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(plugin, "pluginsecret")).await });
    Client::new(url, "pluginsecret").unwrap()
}

fn job(action: Action, provisioning: &Map<String, Value>, package: &str) -> Job {
    Job {
        job_id: "j".into(),
        action,
        service: Service {
            id: "01HSERVICE".into(),
            label: "Acme hosting".into(),
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
            name: "Acme".into(),
            email: Some("ap@acme.example".into()),
        },
        product: Product {
            id: "p".into(),
            name: "Hosting".into(),
        },
        config: json!({"package": package}),
        previous_config: None,
        server: Some(Server { name: "da1".into() }),
    }
}

#[tokio::test]
async fn a_service_lifecycle_and_every_job_twice() {
    let (fake, da) = start_fake().await;
    let c = start_plugin(&da, "k3y").await;

    let m = c.manifest().await.unwrap();
    assert_eq!(m.servers, ["da1"]);
    assert!(
        m.service_fields
            .iter()
            .any(|f| f.name == "domain" && f.required)
    );

    // create, twice (the first answer "lost"): one account.
    let data = c
        .provision(&job(Action::Create, &Map::new(), "basic"))
        .await
        .unwrap();
    assert_eq!(data["server"], "da1");
    assert_eq!(data["domain"], "acme-shop.example");
    let user = data["username"].as_str().unwrap().to_owned();
    assert!(user.starts_with("acmeshop") && user.len() == 10, "{user}");
    assert_eq!(
        c.provision(&job(Action::Create, &Map::new(), "basic"))
            .await
            .unwrap(),
        data
    );
    // The account's password is never in what ferrobill gets back.
    let made = fake.users.lock().unwrap()[&user].clone();
    assert!(
        !serde_json::to_string(&data)
            .unwrap()
            .contains(&made.password)
    );

    for action in [Action::Suspend, Action::Suspend] {
        c.provision(&job(action, &data, "basic")).await.unwrap();
    }
    assert!(fake.users.lock().unwrap()[&user].suspended);
    for action in [Action::Unsuspend, Action::Unsuspend] {
        c.provision(&job(action, &data, "basic")).await.unwrap();
    }
    assert!(!fake.users.lock().unwrap()[&user].suspended);

    for _ in 0..2 {
        c.provision(&job(Action::ChangePackage, &data, "pro"))
            .await
            .unwrap();
    }
    assert_eq!(fake.users.lock().unwrap()[&user].package, "pro");

    // reset_password: a new one, shown once.
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
    let new = a.secret.unwrap();
    assert_eq!(fake.users.lock().unwrap()[&user].password, new);
    assert_ne!(new, made.password);

    for _ in 0..2 {
        c.provision(&job(Action::Terminate, &data, "pro"))
            .await
            .unwrap();
    }
    assert!(fake.users.lock().unwrap().is_empty());

    assert_eq!(
        *fake.changes.lock().unwrap(),
        [
            format!("create {user}"),
            format!("suspend {user}"),
            format!("unsuspend {user}"),
            format!("package {user} pro"),
            format!("delete {user}"),
        ]
    );
}

#[tokio::test]
async fn refusals_and_retries() {
    let (fake, da) = start_fake().await;
    let c = start_plugin(&da, "k3y").await;

    // No package: refused, nothing made.
    let r = c.provision(&job(Action::Create, &Map::new(), "")).await;
    assert!(
        matches!(r, Err(Error::Refused(ref d)) if d.contains("package")),
        "{r:?}"
    );

    // A chosen username that someone else has: refused.
    fake.users.lock().unwrap().insert(
        "taken".into(),
        User {
            domain: "other.example".into(),
            package: "basic".into(),
            suspended: false,
            password: String::new(),
        },
    );
    let mut j = job(Action::Create, &Map::new(), "basic");
    j.service.metadata.insert("username".into(), json!("taken"));
    let r = c.provision(&j).await;
    assert!(
        matches!(r, Err(Error::Refused(ref d)) if d.contains("already taken")),
        "{r:?}"
    );

    // An unknown server name: refused.
    let mut j = job(Action::Create, &Map::new(), "basic");
    j.server = Some(Server { name: "da9".into() });
    assert!(matches!(c.provision(&j).await, Err(Error::Refused(_))));

    // A wrong login key: retried (fixable in the plugin's settings without touching the job).
    let bad = start_plugin(&da, "wrong").await;
    let r = bad
        .provision(&job(Action::Create, &Map::new(), "basic"))
        .await;
    assert!(
        matches!(r, Err(Error::Retry(ref d)) if d.contains("login key")),
        "{r:?}"
    );

    // Terminate before any create: nothing to do.
    c.provision(&job(Action::Terminate, &Map::new(), "basic"))
        .await
        .unwrap();
    assert_eq!(fake.changes.lock().unwrap().len(), 0);
}
