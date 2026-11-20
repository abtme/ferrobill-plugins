// SPDX-License-Identifier: Apache-2.0
//! `ferrobill-hestiacp`: the HestiaCP plugin as a service.
//!
//! | Variable | |
//! |---|---|
//! | `FERROBILL_PLUGIN_SECRET` | the secret ferrobill gave when the plugin was added |
//! | `SERVERS_FILE` | JSON: `{"<server name>": {"url", "access_key", "secret_key", "ip"}}` (default `/etc/ferrobill-hestiacp/servers.json`) |
//! | `LISTEN` | default `0.0.0.0:8080` |

use ferrobill_hestiacp::{Hestia, Servers};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let secret = ferrobill_plugin_sdk::env("FERROBILL_PLUGIN_SECRET")?;
    let path = std::env::var("SERVERS_FILE")
        .unwrap_or_else(|_| "/etc/ferrobill-hestiacp/servers.json".into());
    let text =
        std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
    let servers: Servers =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let plugin = Hestia::new(servers)?;
    ferrobill_plugin_sdk::serve(ferrobill_plugin_sdk::router(plugin, secret)).await
}
