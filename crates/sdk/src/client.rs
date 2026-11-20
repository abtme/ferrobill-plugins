// SPDX-License-Identifier: Apache-2.0
//! Calls a plugin the way ferrobill does: signed JSON, 422 = refused, anything else not 2xx =
//! retry. For module tests and for trying a module against a real server
//! (`ferrobill-plugin-cli`).

use serde_json::{Map, Value};

use crate::{ActionAnswer, ActionRequest, Error, Job, Manifest, signature};

#[derive(Debug, Clone)]
pub struct Client {
    url: String,
    secret: String,
    http: reqwest::Client,
}

impl Client {
    /// `url` is the plugin's base URL (no trailing slash needed).
    pub fn new(url: impl Into<String>, secret: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Client {
            url: url.into().trim_end_matches('/').to_owned(),
            secret: secret.into(),
            http: crate::http_client(std::time::Duration::from_secs(30))?,
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Value, Error> {
        let bytes = body.unwrap_or_default();
        let sig = signature::sign(&self.secret, signature::now(), &bytes);
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.url))
            .header(signature::HEADER, sig);
        if !bytes.is_empty() {
            req = req.header("content-type", "application/json").body(bytes);
        }
        let res = req
            .send()
            .await
            .map_err(|e| Error::retry(format!("could not reach the plugin: {e}")))?;
        let status = res.status();
        let text = res.text().await.map_err(|e| Error::retry(e.to_string()))?;
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(json);
        }
        let detail = json["detail"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| text.chars().take(300).collect());
        if status.as_u16() == 422 {
            Err(Error::Refused(detail))
        } else {
            Err(Error::Retry(format!("{status}: {detail}")))
        }
    }

    pub async fn manifest(&self) -> Result<Manifest, Error> {
        let v = self.call(reqwest::Method::GET, "/manifest", None).await?;
        serde_json::from_value(v).map_err(|e| Error::retry(format!("bad manifest: {e}")))
    }

    /// Send a job; the answer's `data`.
    pub async fn provision(&self, job: &Job) -> Result<Map<String, Value>, Error> {
        let body = serde_json::to_vec(job).map_err(|e| Error::refused(e.to_string()))?;
        let v = self
            .call(reqwest::Method::POST, "/provision", Some(body))
            .await?;
        Ok(v.get("data")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default())
    }

    pub async fn action(&self, request: &ActionRequest) -> Result<ActionAnswer, Error> {
        let body = serde_json::to_vec(request).map_err(|e| Error::refused(e.to_string()))?;
        let v = self
            .call(reqwest::Method::POST, "/action", Some(body))
            .await?;
        serde_json::from_value(v).map_err(|e| Error::retry(format!("bad answer: {e}")))
    }
}
