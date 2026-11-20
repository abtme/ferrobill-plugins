// SPDX-License-Identifier: Apache-2.0
//! HestiaCP provisioning for ferrobill.
//!
//! One HestiaCP user per ferrobill service, with the service's domain as its first web domain,
//! made through HestiaCP's API (`/api/`, an access key) by running its `v-*` commands:
//!
//! | ferrobill job | HestiaCP |
//! |---|---|
//! | `create` | `v-add-user` with the product's `package`, then `v-add-domain` |
//! | `suspend` / `unsuspend` | `v-suspend-user` / `v-unsuspend-user` |
//! | `change_package` | `v-change-user-package` |
//! | `terminate` | `v-delete-user` (the user's files, mail and databases go with it) |
//!
//! Every job first lists the users (`v-list-users json`), so a job sent twice does nothing the
//! second time, and a `create` that stopped between its two steps is finished. The password is
//! made here and never stored or sent to ferrobill; the `reset_password` action gives one, once.

use std::collections::BTreeMap;
use std::time::Duration;

use ferrobill_plugin_sdk::util::{domain_of, password, username_for};
use ferrobill_plugin_sdk::{
    Action, ActionAnswer, ActionRequest, ActionSpec, Error, Job, Manifest, Provisioner, Service,
    ServiceField,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// One HestiaCP server, as configured (the servers file).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// `https://hestia1.example.com:8083`
    pub url: String,
    /// An API access key (Server settings -> Access keys) and its secret.
    pub access_key: String,
    pub secret_key: String,
    /// The IP address new domains go on; empty: HestiaCP's default.
    #[serde(default)]
    pub ip: String,
}

/// The servers file: server name -> settings. The names are what ferrobill's servers use.
pub type Servers = BTreeMap<String, ServerConfig>;

pub struct Hestia {
    servers: Servers,
    http: reqwest::Client,
}

impl std::fmt::Debug for Hestia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the keys.
        f.debug_struct("Hestia")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// What HestiaCP says about a user (`v-list-users json`).
#[derive(Debug, Clone, PartialEq)]
struct User {
    package: String,
    suspended: bool,
}

/// HestiaCP's return codes that matter here (func/main.sh).
const E_NOTEXIST: i32 = 3;
const E_EXISTS: i32 = 4;
const E_SUSPENDED: i32 = 5;
const E_UNSUSPENDED: i32 = 6;

/// Codes worth retrying: disk, load average, connection, database, restart.
fn transient(code: i32) -> bool {
    matches!(code, 13 | 14 | 15 | 17 | 20)
}

impl Hestia {
    pub fn new(servers: Servers) -> anyhow::Result<Self> {
        anyhow::ensure!(!servers.is_empty(), "no HestiaCP servers configured");
        for (name, s) in &servers {
            anyhow::ensure!(
                s.url.starts_with("https://") || s.url.starts_with("http://"),
                "server {name}: url must start with https://"
            );
        }
        Ok(Hestia {
            servers,
            // v-add-user and v-delete-user can take a while on a busy server.
            http: ferrobill_plugin_sdk::http_client(Duration::from_secs(120))?,
        })
    }

    /// The server a job or action is for: the one ferrobill names, else the one an earlier job
    /// used, else the product's `server` setting, else the only one configured.
    fn server<'a>(
        &'a self,
        named: Option<&str>,
        service: &Service,
        config: &Value,
    ) -> Result<(&'a str, &'a ServerConfig), Error> {
        let name = named
            .or_else(|| service.provisioned("server"))
            .or_else(|| config["server"].as_str());
        match name {
            Some(n) => self
                .servers
                .get_key_value(n)
                .map(|(k, v)| (k.as_str(), v))
                .ok_or_else(|| {
                    Error::refused(format!(
                        "no HestiaCP server called {n:?} is configured in this plugin"
                    ))
                }),
            None if self.servers.len() == 1 => {
                let (k, v) = self.servers.iter().next().expect("one server");
                Ok((k.as_str(), v))
            }
            None => Err(Error::refused(
                "several HestiaCP servers are configured: name one (the product's server group, or `server` in its provisioning config)",
            )),
        }
    }

    async fn post(
        &self,
        server: &ServerConfig,
        cmd: &str,
        args: &[&str],
        returncode: bool,
    ) -> Result<(u16, String), Error> {
        let hash = format!("{}:{}", server.access_key, server.secret_key);
        let keys: Vec<String> = (1..=args.len()).map(|i| format!("arg{i}")).collect();
        let mut form: Vec<(&str, &str)> = vec![("hash", &hash), ("cmd", cmd)];
        if returncode {
            form.push(("returncode", "yes"));
        }
        for (k, v) in keys.iter().zip(args) {
            form.push((k.as_str(), v));
        }
        let res = self
            .http
            .post(format!("{}/api/", server.url.trim_end_matches('/')))
            .form(&form)
            .send()
            .await
            .map_err(|e| Error::retry(format!("could not reach HestiaCP: {e}")))?;
        let status = res.status().as_u16();
        let text = res
            .text()
            .await
            .map_err(|e| Error::retry(format!("HestiaCP's answer: {e}")))?;
        if status == 401 || status == 403 || text.contains("Error: authentication failed") {
            return Err(Error::retry(format!(
                "HestiaCP refused the access key ({status}): check the key, its secret and its permissions, and that the plugin's IP may use the API"
            )));
        }
        Ok((status, text))
    }

    /// Run a command; its return code (0 = done).
    async fn run(&self, server: &ServerConfig, cmd: &str, args: &[&str]) -> Result<i32, Error> {
        let (status, text) = self.post(server, cmd, args, true).await?;
        text.trim().parse::<i32>().map_err(|_| {
            Error::retry(format!(
                "{cmd}: HestiaCP answered {status} without a return code"
            ))
        })
    }

    /// Run a command that must succeed (or end in one of `fine`).
    async fn must(
        &self,
        server: &ServerConfig,
        cmd: &str,
        args: &[&str],
        fine: &[i32],
    ) -> Result<(), Error> {
        match self.run(server, cmd, args).await? {
            0 => Ok(()),
            c if fine.contains(&c) => Ok(()),
            c if transient(c) => Err(Error::retry(format!(
                "{cmd} failed with HestiaCP code {c} (busy or a service down): will try again"
            ))),
            c => Err(Error::refused(format!(
                "{cmd} failed with HestiaCP code {c}"
            ))),
        }
    }

    /// A command's JSON output.
    async fn json(&self, server: &ServerConfig, cmd: &str, args: &[&str]) -> Result<Value, Error> {
        let (status, text) = self.post(server, cmd, args, false).await?;
        if !(200..300).contains(&status) {
            return Err(Error::retry(format!("{cmd}: HestiaCP answered {status}")));
        }
        serde_json::from_str(&text)
            .map_err(|_| Error::retry(format!("{cmd}: HestiaCP's answer is not JSON")))
    }

    async fn user(&self, server: &ServerConfig, name: &str) -> Result<Option<User>, Error> {
        let all = self.json(server, "v-list-users", &["json"]).await?;
        Ok(all.get(name).map(|u| User {
            package: u["PACKAGE"].as_str().unwrap_or_default().to_owned(),
            suspended: u["SUSPENDED"].as_str() == Some("yes"),
        }))
    }

    async fn web_domains(&self, server: &ServerConfig, user: &str) -> Result<Vec<String>, Error> {
        let v = self
            .json(server, "v-list-web-domains", &[user, "json"])
            .await?;
        Ok(v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default())
    }

    async fn create(
        &self,
        job: &Job,
        name: &str,
        server: &ServerConfig,
    ) -> Result<Map<String, Value>, Error> {
        let domain = domain_of(&job.service)?;
        let package = job.config["package"]
            .as_str()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                Error::refused(
                    "the product's provisioning config needs a `package` (a HestiaCP package name)",
                )
            })?;
        let chosen = job.service.field("username").map(str::to_lowercase);
        let username = match &chosen {
            Some(u) => {
                check_username(u)?;
                u.clone()
            }
            None => username_for(&domain, &job.service.id, 12),
        };
        let data = json!({"server": name, "username": username, "domain": domain})
            .as_object()
            .cloned()
            .unwrap_or_default();
        if self.user(server, &username).await?.is_some() {
            let domains = self.web_domains(server, &username).await?;
            if domains.iter().any(|d| d == &domain) {
                return Ok(data); // sent before; the answer was lost
            }
            if !domains.is_empty() {
                return Err(Error::refused(format!(
                    "the HestiaCP username {username:?} is already taken by another account"
                )));
            }
            // Made by an earlier try that stopped before its domain: finish it.
        } else {
            let email = job.customer.email.as_deref().ok_or_else(|| {
                Error::refused("the customer has no email address (HestiaCP needs one)")
            })?;
            let pw = password();
            let name_arg = if job.customer.name.trim().is_empty() {
                username.as_str()
            } else {
                job.customer.name.trim()
            };
            self.must(
                server,
                "v-add-user",
                &[&username, &pw, email, package, name_arg],
                &[],
            )
            .await?;
        }
        let mut args = vec![username.as_str(), domain.as_str()];
        if !server.ip.is_empty() {
            args.push(&server.ip);
        }
        self.must(server, "v-add-domain", &args, &[E_EXISTS])
            .await?;
        Ok(data)
    }

    fn username_of(service: &Service) -> Result<&str, Error> {
        service.provisioned("username").ok_or_else(|| {
            Error::refused(
                "this service has no HestiaCP account yet (no username from a create job)",
            )
        })
    }

    async fn set_suspended(
        &self,
        service: &Service,
        server: &ServerConfig,
        suspend: bool,
    ) -> Result<Map<String, Value>, Error> {
        let username = Self::username_of(service)?;
        let Some(user) = self.user(server, username).await? else {
            return Err(Error::refused(format!("HestiaCP has no user {username:?}")));
        };
        if user.suspended != suspend {
            if suspend {
                self.must(server, "v-suspend-user", &[username], &[E_SUSPENDED])
                    .await?;
            } else {
                self.must(server, "v-unsuspend-user", &[username], &[E_UNSUSPENDED])
                    .await?;
            }
        }
        Ok(Map::new())
    }

    async fn change_package(
        &self,
        job: &Job,
        server: &ServerConfig,
    ) -> Result<Map<String, Value>, Error> {
        let username = Self::username_of(&job.service)?;
        let package = job.config["package"]
            .as_str()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                Error::refused("the new product's provisioning config needs a `package`")
            })?;
        let Some(user) = self.user(server, username).await? else {
            return Err(Error::refused(format!("HestiaCP has no user {username:?}")));
        };
        if user.package != package {
            self.must(server, "v-change-user-package", &[username, package], &[])
                .await?;
        }
        Ok(Map::new())
    }

    async fn terminate(
        &self,
        service: &Service,
        server: &ServerConfig,
    ) -> Result<Map<String, Value>, Error> {
        let Some(username) = service.provisioned("username") else {
            return Ok(Map::new()); // never made
        };
        self.must(server, "v-delete-user", &[username], &[E_NOTEXIST])
            .await?;
        Ok(Map::new())
    }
}

impl Provisioner for Hestia {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "hestiacp".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec!["provisioning".into()],
            service_fields: vec![
                ServiceField {
                    name: "domain".into(),
                    label: "Domain".into(),
                    required: true,
                    kind: "domain".into(),
                    help: Some("The account's main domain, e.g. example.com".into()),
                },
                ServiceField {
                    name: "username".into(),
                    label: "Username".into(),
                    required: false,
                    kind: "username".into(),
                    help: Some("Leave empty to make one from the domain".into()),
                },
            ],
            actions: vec![
                ActionSpec {
                    name: "reset_password".into(),
                    label: "Reset the control panel password".into(),
                    client: true,
                },
                ActionSpec {
                    name: "panel".into(),
                    label: "Open the control panel".into(),
                    client: true,
                },
            ],
            servers: self.servers.keys().cloned().collect(),
            extra: Map::new(),
        }
    }

    async fn provision(&self, job: Job) -> Result<Map<String, Value>, Error> {
        let (name, server) = self.server(
            job.server.as_ref().map(|s| s.name.as_str()),
            &job.service,
            &job.config,
        )?;
        match job.action {
            Action::Create => self.create(&job, name, server).await,
            Action::Suspend => self.set_suspended(&job.service, server, true).await,
            Action::Unsuspend => self.set_suspended(&job.service, server, false).await,
            Action::ChangePackage => self.change_package(&job, server).await,
            Action::Terminate => self.terminate(&job.service, server).await,
            Action::Unknown => Err(Error::refused("unknown action")),
        }
    }

    async fn action(&self, r: ActionRequest) -> Result<ActionAnswer, Error> {
        let (_, server) = self.server(
            r.server.as_ref().map(|s| s.name.as_str()),
            &r.service,
            &r.config,
        )?;
        match r.action.as_str() {
            "reset_password" => {
                let username = Self::username_of(&r.service)?;
                let pw = password();
                self.must(server, "v-change-user-password", &[username, &pw], &[])
                    .await?;
                Ok(ActionAnswer {
                    message: Some(format!(
                        "New password for {username} at {}. It is shown once only.",
                        server.url
                    )),
                    secret: Some(pw),
                    ..Default::default()
                })
            }
            "panel" => Ok(ActionAnswer {
                redirect_url: Some(server.url.clone()),
                ..Default::default()
            }),
            other => Err(Error::refused(format!("no action {other:?}"))),
        }
    }
}

/// HestiaCP usernames, kept simple: 3 to 16 lower case letters and digits, starting with a
/// letter (HestiaCP allows more, but not every service it runs does).
fn check_username(u: &str) -> Result<(), Error> {
    let ok = (3..=16).contains(&u.len())
        && u.starts_with(|c: char| c.is_ascii_lowercase())
        && u.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(Error::refused(format!(
            "{u:?} can't be a HestiaCP username here: 3 to 16 lower case letters and digits, starting with a letter"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        check_username(&username_for("my-shop.example", "s1", 12)).unwrap();
        assert!(check_username("admin1").is_ok());
        assert!(check_username("1admin").is_err());
        assert!(check_username("ab").is_err());
        assert!(check_username("a-b-c").is_err());
    }
}
