# ferrobill-plugins

Provisioning plugins for [ferrobill](https://github.com/abtme/ferrobill), the API-first billing
server: they set up what customers buy (hosting accounts, servers, ...) when their service
starts, and suspend, restore, change and remove it as the service changes. Like the server
modules of WHMCS or ClientExec, but separate services that talk to ferrobill over a small,
documented, signed JSON protocol.

Everything here is under the **Apache License 2.0**. ferrobill itself is AGPL-3.0 with a plugin
exception: plugins that only use the documented interface may use any licence.

> **Status: early.** The plugins are tested against fakes of the panels' APIs, not yet against
> real servers.

## Plugins

| Plugin | Image | State |
|---|---|---|
| [DirectAdmin](modules/directadmin/README.md) | `ferrobill-directadmin` | written, tested against a fake DirectAdmin |
| apicp | | planned |
| [HestiaCP](modules/hestiacp/README.md) | `ferrobill-hestiacp` | written, tested against a fake HestiaCP |
| cPanel / WHM | | planned |
| Plesk | | planned |
| DigitalOcean (droplets) | | planned |
| Proxmox, Virtualizor | | planned |

## Layout

| Path | What it is |
|---|---|
| [spec/protocol.md](spec/protocol.md) | The protocol: manifest, jobs, actions, signatures |
| `spec/plugin.wit` | The same interface for WebAssembly plugins |
| `crates/sdk` | `ferrobill-plugin-sdk`: types, signature checks, the HTTP server, a client for tests, helpers (domain, username, password) |
| `crates/cli` | `ferrobill-plugin-cli`: call a plugin as ferrobill would, to try it against a real server |
| `modules/<name>` | One plugin each, built into its own image |

## Writing a plugin

In Rust, implement `ferrobill_plugin_sdk::Provisioner` (a manifest, `provision(job)`, and
optionally `action(request)`) and serve it:

```rust
ferrobill_plugin_sdk::serve(ferrobill_plugin_sdk::router(MyPlugin::new(), secret)).await
```

`Error::Refused` fails the job for good (bad settings, a name taken by someone else);
`Error::Retry` makes ferrobill try again later (the server is down). Every job may arrive twice,
so make each action idempotent. `modules/directadmin` is a complete example, with a test against
a fake panel.

Any other language works too: follow [spec/protocol.md](spec/protocol.md).

## Building

```sh
cargo test --workspace
docker build -f docker/Dockerfile --target directadmin -t ferrobill-directadmin .
```

Images are static (musl, rustls), built `FROM scratch`, run as an unprivileged user and listen
on 8080.

## Licence

Apache-2.0, see [LICENSE](LICENSE). Contributions are accepted under the same licence
(see [CONTRIBUTING.md](CONTRIBUTING.md)). ferrobill-plugins is not affiliated with WHMCS,
ClientExec, DirectAdmin, cPanel, Plesk, HestiaCP or DigitalOcean.
