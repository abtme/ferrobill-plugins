// SPDX-License-Identifier: Apache-2.0
//! The Domain Name API registrar plugin's server: see README.md for its settings.

use ferrobill_domainnameapi::{Config, DomainNameApi};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let secret = ferrobill_plugin_sdk::env("FERROBILL_PLUGIN_SECRET")?;
    let plugin = DomainNameApi::new(Config::from_env()?)?;
    ferrobill_plugin_sdk::serve(ferrobill_plugin_sdk::registrar::router(plugin, secret)).await
}
