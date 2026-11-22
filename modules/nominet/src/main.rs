// SPDX-License-Identifier: Apache-2.0
//! The Nominet registrar plugin's server: see README.md for its settings.

use ferrobill_nominet::{Config, Nominet};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let secret = ferrobill_plugin_sdk::env("FERROBILL_PLUGIN_SECRET")?;
    let plugin = Nominet::new(Config::from_env()?)?;
    ferrobill_plugin_sdk::serve(ferrobill_plugin_sdk::registrar::router(plugin, secret)).await
}
