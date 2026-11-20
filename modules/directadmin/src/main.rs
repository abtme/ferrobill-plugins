// SPDX-License-Identifier: Apache-2.0
//! `ferrobill-directadmin`: the DirectAdmin plugin as a service.
//!
//! | Variable | |
//! |---|---|
//! | `FERROBILL_PLUGIN_SECRET` | the secret ferrobill gave when the plugin was added |
//! | `SERVERS_FILE` | JSON: `{"<server name>": {"url", "username", "login_key", "ip"}}` (default `/etc/ferrobill-directadmin/servers.json`) |
//! | `LISTEN` | default `0.0.0.0:8080` |

use ferrobill_directadmin::{DirectAdmin, Servers};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let secret = ferrobill_plugin_sdk::env("FERROBILL_PLUGIN_SECRET")?;
    let path = std::env::var("SERVERS_FILE")
        .unwrap_or_else(|_| "/etc/ferrobill-directadmin/servers.json".into());
    let text =
        std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
    let servers: Servers =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let plugin = DirectAdmin::new(servers)?;
    ferrobill_plugin_sdk::serve(ferrobill_plugin_sdk::router(plugin, secret)).await
}
