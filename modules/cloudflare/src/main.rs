// SPDX-License-Identifier: Apache-2.0
//! The Cloudflare Registrar plugin's server: see README.md for its settings.

use ferrobill_cloudflare::{Cloudflare, Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let secret = ferrobill_plugin_sdk::env("FERROBILL_PLUGIN_SECRET")?;
    let plugin = Cloudflare::new(Config::from_env()?)?;
    ferrobill_plugin_sdk::serve(ferrobill_plugin_sdk::registrar::router(plugin, secret)).await
}
