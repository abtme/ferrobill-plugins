# Contributing

Contributions are welcome under the Apache License 2.0: by opening a pull request you agree that
your contribution is licensed under it (Apache-2.0, section 5).

- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
  and `cargo deny check` must pass.
- A new plugin goes in `modules/<name>`, with a README, a test against a fake of the panel's API,
  and a target in `docker/Dockerfile`.
- Dependencies must be under permissive licences (see `deny.toml`), and released at least two
  weeks before they are added (security fixes excepted).
- Pull requests from forks wait for a maintainer before CI runs.
