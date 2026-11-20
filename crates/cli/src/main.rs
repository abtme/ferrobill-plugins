// SPDX-License-Identifier: Apache-2.0
//! `ferrobill-plugin-cli`: call a plugin the way ferrobill does, to try a module against a real
//! server before ferrobill uses it.
//!
//! ```text
//! export FERROBILL_PLUGIN_SECRET=...      # the secret the plugin runs with
//! ferrobill-plugin-cli http://127.0.0.1:8080 manifest
//! ferrobill-plugin-cli http://127.0.0.1:8080 job job.json      # POST /provision
//! ferrobill-plugin-cli http://127.0.0.1:8080 action action.json   # POST /action
//! ```
//!
//! The files hold the request bodies (spec/protocol.md). The answer is printed as JSON; a
//! refusal exits 2, anything to retry exits 1.

use ferrobill_plugin_sdk::{ActionRequest, Error, Job, client::Client};

fn read<T: serde::de::DeserializeOwned>(path: &str) -> anyhow::Result<T> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{path}: {e}"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let secret = ferrobill_plugin_sdk::env("FERROBILL_PLUGIN_SECRET")?;
    let usage =
        "usage: ferrobill-plugin-cli <plugin url> manifest | job <file.json> | action <file.json>";
    let (Some(url), Some(what)) = (args.first(), args.get(1)) else {
        anyhow::bail!(usage);
    };
    let c = Client::new(url, secret)?;
    let answer = match (what.as_str(), args.get(2)) {
        ("manifest", None) => c
            .manifest()
            .await
            .map(|m| serde_json::to_value(m).unwrap_or_default()),
        ("job", Some(f)) => c
            .provision(&read::<Job>(f)?)
            .await
            .map(|d| serde_json::json!({"data": d})),
        ("action", Some(f)) => c
            .action(&read::<ActionRequest>(f)?)
            .await
            .map(|a| serde_json::to_value(a).unwrap_or_default()),
        _ => anyhow::bail!(usage),
    };
    match answer {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Err(Error::Refused(d)) => {
            eprintln!("refused: {d}");
            std::process::exit(2)
        }
        Err(Error::Retry(d)) => {
            eprintln!("not done (would be retried): {d}");
            std::process::exit(1)
        }
    }
}
