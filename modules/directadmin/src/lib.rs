// SPDX-License-Identifier: Apache-2.0
//! DirectAdmin provisioning for ferrobill.
//!
//! One DirectAdmin user per ferrobill service, made through DirectAdmin's API (the `CMD_API_*`
//! commands) as an admin or reseller signed in with a login key:
//!
//! | ferrobill job | DirectAdmin |
//! |---|---|
//! | `create` | `CMD_API_ACCOUNT_USER` (action=create) with the product's `package` |
//! | `suspend` / `unsuspend` | `CMD_API_SELECT_USERS` (only if not already in that state) |
//! | `change_package` | `CMD_API_MODIFY_USER` (action=package) |
//! | `terminate` | `CMD_API_SELECT_USERS` (delete) |
//!
//! Each job first reads the user (`CMD_API_SHOW_USER_CONFIG`), so a job sent twice does nothing
//! the second time. The account's password is made here and never stored or sent to ferrobill;
//! the customer gets one from the `reset_password` action, shown to them once.

use std::collections::BTreeMap;
use std::time::Duration;

use ferrobill_plugin_sdk::util::{domain_of, password, username_for};
use ferrobill_plugin_sdk::{
    Action, ActionAnswer, ActionRequest, ActionSpec, Error, Job, Manifest, Provisioner, Service,
    ServiceField,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// One DirectAdmin server, as configured (the servers file).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// `https://da1.example.com:2222`
    pub url: String,
    /// The admin or reseller that owns the accounts.
    pub username: String,
    /// A login key of that user (Login Keys in DirectAdmin), with the commands above allowed.
    pub login_key: String,
    /// The IP address new accounts get.
    pub ip: String,
}

/// The servers file: server name -> settings. The names are what ferrobill's servers use.
pub type Servers = BTreeMap<String, ServerConfig>;

pub struct DirectAdmin {
    servers: Servers,
    http: reqwest::Client,
}

impl std::fmt::Debug for DirectAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the login keys.
        f.debug_struct("DirectAdmin")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// What DirectAdmin says about a user (`CMD_API_SHOW_USER_CONFIG`).
#[derive(Debug, Clone, PartialEq)]
struct UserConfig {
    domain: Option<String>,
    package: Option<String>,
    suspended: bool,
}

impl DirectAdmin {
    pub fn new(servers: Servers) -> anyhow::Result<Self> {
        anyhow::ensure!(!servers.is_empty(), "no DirectAdmin servers configured");
        for (name, s) in &servers {
            anyhow::ensure!(
                s.url.starts_with("https://") || s.url.starts_with("http://"),
                "server {name}: url must start with https://"
            );
        }
        Ok(DirectAdmin {
            servers,
            http: ferrobill_plugin_sdk::http_client(Duration::from_secs(60))?,
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
                        "no DirectAdmin server called {n:?} is configured in this plugin"
                    ))
                }),
            None if self.servers.len() == 1 => {
                let (k, v) = self.servers.iter().next().expect("one server");
                Ok((k.as_str(), v))
            }
            None => Err(Error::refused(
                "several DirectAdmin servers are configured: name one (the product's server group, or `server` in its provisioning config)",
            )),
        }
    }

    /// Call a `CMD_API_*` command; its answer as key -> value.
    async fn call(
        &self,
        server: &ServerConfig,
        command: &str,
        form: &[(&str, &str)],
    ) -> Result<BTreeMap<String, String>, Error> {
        let url = format!("{}/{command}", server.url.trim_end_matches('/'));
        let res = self
            .http
            .post(&url)
            .basic_auth(&server.username, Some(&server.login_key))
            .form(form)
            .send()
            .await
            .map_err(|e| Error::retry(format!("could not reach DirectAdmin: {e}")))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| Error::retry(format!("DirectAdmin's answer: {e}")))?;
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(Error::retry(format!(
                "DirectAdmin refused the login key ({status}): check the username, key and the key's allowed commands"
            )));
        }
        if !status.is_success() {
            return Err(Error::retry(format!("DirectAdmin answered {status}")));
        }
        if text.trim_start().starts_with('<') {
            // A login page or an HTML error instead of an API answer.
            return Err(Error::retry(
                "DirectAdmin answered a web page, not an API answer: check the URL (port 2222) and the login key",
            ));
        }
        serde_urlencoded::from_str::<Vec<(String, String)>>(&text)
            .map(|v| v.into_iter().collect())
            .map_err(|e| Error::retry(format!("DirectAdmin's answer could not be read: {e}")))
    }

    /// Call a command that answers `error=0|1&text=...&details=...`; an error is refused with
    /// DirectAdmin's own words.
    async fn command(
        &self,
        server: &ServerConfig,
        command: &str,
        form: &[(&str, &str)],
    ) -> Result<(), Error> {
        let answer = self.call(server, command, form).await?;
        match answer.get("error").map(String::as_str) {
            Some("0") => Ok(()),
            Some(_) => Err(Error::refused(
                format!(
                    "DirectAdmin: {} {}",
                    answer.get("text").map(String::as_str).unwrap_or("error"),
                    answer.get("details").map(String::as_str).unwrap_or("")
                )
                .trim_end()
                .to_owned(),
            )),
            None => Err(Error::retry(format!(
                "{command}: DirectAdmin's answer has no error field"
            ))),
        }
    }

    /// The user, or None if DirectAdmin has no such user.
    async fn user(&self, server: &ServerConfig, user: &str) -> Result<Option<UserConfig>, Error> {
        let a = self
            .call(server, "CMD_API_SHOW_USER_CONFIG", &[("user", user)])
            .await?;
        if a.get("error").is_some_and(|e| e != "0") {
            return Ok(None);
        }
        if a.is_empty() {
            return Ok(None);
        }
        Ok(Some(UserConfig {
            domain: a.get("domain").cloned(),
            package: a.get("package").cloned(),
            suspended: a.get("suspended").is_some_and(|s| s == "yes"),
        }))
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
            .ok_or_else(|| Error::refused("the product's provisioning config needs a `package` (a DirectAdmin user package name)"))?;
        let chosen = job.service.field("username").map(str::to_lowercase);
        let username = match &chosen {
            Some(u) => {
                check_username(u)?;
                u.clone()
            }
            None => username_for(&domain, &job.service.id, 8),
        };
        let data = |username: &str| {
            json!({"server": name, "username": username, "domain": domain})
                .as_object()
                .cloned()
                .unwrap_or_default()
        };
        if let Some(existing) = self.user(server, &username).await? {
            // Sent before (the answer was lost), or the name belongs to someone else.
            if existing.domain.as_deref() == Some(domain.as_str()) {
                return Ok(data(&username));
            }
            return Err(Error::refused(format!(
                "the DirectAdmin username {username:?} is already taken by another account"
            )));
        }
        let email = job.customer.email.as_deref().ok_or_else(|| {
            Error::refused("the customer has no email address (DirectAdmin needs one)")
        })?;
        let password = password();
        self.command(
            server,
            "CMD_API_ACCOUNT_USER",
            &[
                ("action", "create"),
                ("add", "Submit"),
                ("username", &username),
                ("email", email),
                ("passwd", &password),
                ("passwd2", &password),
                ("domain", &domain),
                ("package", package),
                ("ip", &server.ip),
                ("notify", "no"),
            ],
        )
        .await?;
        Ok(data(&username))
    }

    fn username_of(service: &Service) -> Result<&str, Error> {
        service.provisioned("username").ok_or_else(|| {
            Error::refused(
                "this service has no DirectAdmin account yet (no username from a create job)",
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
            return Err(Error::refused(format!(
                "DirectAdmin has no user {username:?}"
            )));
        };
        if user.suspended != suspend {
            self.command(
                server,
                "CMD_API_SELECT_USERS",
                &[
                    ("location", "CMD_SELECT_USERS"),
                    ("suspend", if suspend { "Suspend" } else { "Unsuspend" }),
                    ("select0", username),
                ],
            )
            .await?;
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
            return Err(Error::refused(format!(
                "DirectAdmin has no user {username:?}"
            )));
        };
        if user.package.as_deref() != Some(package) {
            self.command(
                server,
                "CMD_API_MODIFY_USER",
                &[
                    ("action", "package"),
                    ("user", username),
                    ("package", package),
                ],
            )
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
            // Never made: nothing to remove.
            return Ok(Map::new());
        };
        if self.user(server, username).await?.is_some() {
            self.command(
                server,
                "CMD_API_SELECT_USERS",
                &[
                    ("confirmed", "Confirm"),
                    ("delete", "yes"),
                    ("select0", username),
                ],
            )
            .await?;
        }
        Ok(Map::new())
    }
}

impl Provisioner for DirectAdmin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "directadmin".into(),
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
                let password = password();
                self.command(
                    server,
                    "CMD_API_USER_PASSWD",
                    &[
                        ("username", username),
                        ("passwd", &password),
                        ("passwd2", &password),
                    ],
                )
                .await?;
                Ok(ActionAnswer {
                    message: Some(format!(
                        "New password for {username} at {}. It is shown once only.",
                        server.url
                    )),
                    secret: Some(password),
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

/// DirectAdmin usernames: lowercase letters and digits, starting with a letter, at most 10
/// (DirectAdmin's default limit).
fn check_username(u: &str) -> Result<(), Error> {
    let ok = (3..=10).contains(&u.len())
        && u.starts_with(|c: char| c.is_ascii_lowercase())
        && u.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(Error::refused(format!(
            "{u:?} can't be a DirectAdmin username: 3 to 10 lowercase letters and digits, starting with a letter"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_are_stable_short_and_valid() {
        let a = username_for("my-shop2024.example", "service-1", 8);
        assert_eq!(a, username_for("my-shop2024.example", "service-1", 8));
        assert_ne!(a, username_for("my-shop2024.example", "service-2", 8));
        assert!(a.starts_with("myshop20"));
        check_username(&a).unwrap();
        check_username(&username_for("123.example", "s", 8)).unwrap();
        assert!(check_username("Admin").is_err());
        assert!(check_username("1abc").is_err());
        assert!(check_username("ab").is_err());
        assert!(check_username("abcdefghijk").is_err());
    }
}
