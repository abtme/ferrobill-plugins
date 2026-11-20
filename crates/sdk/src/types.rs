// SPDX-License-Identifier: Apache-2.0
//! The JSON ferrobill and a provisioning plugin exchange (spec/protocol.md).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `GET /manifest`: what the plugin is and what it offers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// `provisioning` for the plugins in this repository.
    pub kinds: Vec<String>,
    /// What ferrobill asks for when a service of a product on this plugin is created (stored in
    /// the service's `metadata`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_fields: Vec<ServiceField>,
    /// Things ferrobill may ask for on demand (`POST /action`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionSpec>,
    /// The servers this plugin is configured for, by name. Credentials stay with the plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<String>,
    /// Anything else.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A field a service needs, such as its domain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceField {
    /// The key in the service's `metadata`.
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub required: bool,
    /// `text`, `domain` or `username`: how ferrobill checks it.
    #[serde(default = "text")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

fn text() -> String {
    "text".into()
}

/// An action offered on demand, such as signing in to the control panel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionSpec {
    pub name: String,
    pub label: String,
    /// Offered to the customer in the client area as well as to admins.
    #[serde(default)]
    pub client: bool,
}

/// What a provisioning job asks for.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Suspend,
    Unsuspend,
    Terminate,
    /// The service moved to another product on the same plugin: `config` is the new product's,
    /// `previous_config` the old one's.
    ChangePackage,
    /// An action this SDK doesn't know (a newer ferrobill); refused.
    #[serde(other)]
    Unknown,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Suspend => "suspend",
            Action::Unsuspend => "unsuspend",
            Action::Terminate => "terminate",
            Action::ChangePackage => "change_package",
            Action::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Service {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "one")]
    pub quantity: i64,
    #[serde(default)]
    pub status: String,
    /// What the customer or admin gave (the manifest's `service_fields`).
    #[serde(default)]
    pub metadata: Map<String, Value>,
    /// What earlier jobs answered as `data`, merged.
    #[serde(default)]
    pub provisioning: Map<String, Value>,
}

fn one() -> i64 {
    1
}

impl Service {
    /// A string from `metadata`, trimmed, if present and not empty.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.metadata
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// A string from `provisioning` (what earlier jobs answered).
    pub fn provisioned(&self, name: &str) -> Option<&str> {
        self.provisioning.get(name).and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Customer {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Product {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// The server a service is on (or, for `create`, the one ferrobill picked), by name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Server {
    pub name: String,
}

/// `POST /provision`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub job_id: String,
    pub action: Action,
    pub service: Service,
    pub customer: Customer,
    pub product: Product,
    /// The product's provisioning `config` (for example the hosting package).
    #[serde(default)]
    pub config: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_config: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Server>,
}

/// `POST /action`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionRequest {
    pub action: String,
    pub service: Service,
    pub customer: Customer,
    pub product: Product,
    #[serde(default)]
    pub config: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Server>,
    /// `admin` or `client`: who asked.
    #[serde(default)]
    pub by: String,
}

/// The answer to an action. ferrobill sends the person to `redirect_url`, shows `message`, shows
/// `secret` once (a new password) and never stores it, and merges `data` into the service's
/// provisioning data.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ActionAnswer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Map<String, Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_job_ferrobill_sends_today() {
        // As documented for ferrobill 0.7 (no server, no previous_config yet).
        let job: Job = serde_json::from_value(json!({
            "job_id": "j1", "action": "create",
            "service": {"id": "s1", "label": "acme.example", "quantity": 1, "status": "active",
                        "metadata": {}, "provisioning": {}},
            "customer": {"id": "c1", "name": "Acme Ltd", "email": "ap@acme.example"},
            "product": {"id": "p1", "name": "Web hosting"},
            "config": {"package": "basic"}}))
        .unwrap();
        assert_eq!(job.action, Action::Create);
        assert_eq!(job.server, None);
        assert_eq!(job.config["package"], "basic");
    }

    #[test]
    fn unknown_actions_parse_as_unknown() {
        let a: Action = serde_json::from_value(json!("migrate")).unwrap();
        assert_eq!(a, Action::Unknown);
        let a: Action = serde_json::from_value(json!("change_package")).unwrap();
        assert_eq!(a, Action::ChangePackage);
    }
}
