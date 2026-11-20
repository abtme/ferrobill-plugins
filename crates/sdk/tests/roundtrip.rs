// SPDX-License-Identifier: Apache-2.0
//! A toy plugin served by the SDK, called with the SDK's client (as ferrobill would).

use ferrobill_plugin_sdk::{
    Action, ActionAnswer, ActionRequest, Customer, Error, Job, Manifest, Product, Provisioner,
    Service, client::Client, router,
};
use serde_json::{Map, Value, json};

struct Toy;

impl Provisioner for Toy {
    fn manifest(&self) -> Manifest {
        serde_json::from_value(
            json!({"name": "toy", "version": "1", "kinds": ["provisioning"],
            "actions": [{"name": "sso", "label": "Log in", "client": true}]}),
        )
        .unwrap()
    }
    async fn provision(&self, job: Job) -> Result<Map<String, Value>, Error> {
        match job.action {
            Action::Create if job.service.field("domain").is_none() => {
                Err(Error::refused("domain is required"))
            }
            Action::Create => Ok(json!({"username": "acme1"}).as_object().unwrap().clone()),
            Action::Suspend => Err(Error::retry("panel down")),
            _ => Ok(Map::new()),
        }
    }
    async fn action(&self, r: ActionRequest) -> Result<ActionAnswer, Error> {
        Ok(ActionAnswer {
            redirect_url: Some(format!("https://panel.example/login/{}", r.service.id)),
            ..Default::default()
        })
    }
}

async fn start() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(Toy, "s3cret")).await });
    format!("http://{addr}")
}

fn job(action: Action, metadata: Value) -> Job {
    Job {
        job_id: "j1".into(),
        action,
        service: Service {
            id: "s1".into(),
            label: "acme.example".into(),
            quantity: 1,
            status: "active".into(),
            metadata: metadata.as_object().unwrap().clone(),
            provisioning: Map::new(),
        },
        customer: Customer {
            id: "c1".into(),
            name: "Acme".into(),
            email: None,
        },
        product: Product {
            id: "p1".into(),
            name: "Hosting".into(),
        },
        config: json!({"package": "basic"}),
        previous_config: None,
        server: None,
    }
}

#[tokio::test]
async fn jobs_and_actions_round_trip_with_the_right_statuses() {
    let url = start().await;
    let c = Client::new(&url, "s3cret").unwrap();
    assert_eq!(c.manifest().await.unwrap().name, "toy");
    let data = c
        .provision(&job(Action::Create, json!({"domain": "acme.example"})))
        .await
        .unwrap();
    assert_eq!(data["username"], "acme1");
    assert_eq!(
        c.provision(&job(Action::Create, json!({}))).await,
        Err(Error::Refused("domain is required".into()))
    );
    assert!(matches!(
        c.provision(&job(Action::Suspend, json!({}))).await,
        Err(Error::Retry(_))
    ));
    let req = ActionRequest {
        action: "sso".into(),
        service: job(Action::Create, json!({})).service,
        customer: Customer {
            id: "c1".into(),
            name: String::new(),
            email: None,
        },
        product: Product {
            id: "p1".into(),
            name: String::new(),
        },
        config: Value::Null,
        server: None,
        by: "client".into(),
    };
    let a = c.action(&req).await.unwrap();
    assert_eq!(
        a.redirect_url.as_deref(),
        Some("https://panel.example/login/s1")
    );
}

#[tokio::test]
async fn a_wrong_secret_is_refused_before_the_plugin_sees_anything() {
    let url = start().await;
    let c = Client::new(&url, "wrong").unwrap();
    match c.manifest().await {
        Err(Error::Retry(d)) => assert!(d.starts_with("401"), "{d}"),
        other => panic!("{other:?}"),
    }
    // Health checks need no signature.
    let body = reqwest::get(format!("{url}/healthz"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(body, "ok");
}
