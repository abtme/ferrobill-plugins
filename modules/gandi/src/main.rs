// SPDX-License-Identifier: Apache-2.0
//! The Gandi registrar plugin's server: see README.md for its settings.

use ferrobill_gandi::{Config, Gandi};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let secret = ferrobill_plugin_sdk::env("FERROBILL_PLUGIN_SECRET")?;
    let plugin = Gandi::new(Config::from_env()?)?;
    ferrobill_plugin_sdk::serve(ferrobill_plugin_sdk::registrar::router(plugin, secret)).await
}
