# Development and releases

## Local installation

```sh
cargo install --path .
```

## Validation

Run the repository checks before submitting changes:

```sh
cargo fmt --check
cargo check --locked --all-targets --all-features
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
```

## Release workflow

On pushes to `master`, the GitHub Actions release workflow compares the current Cargo package
version with the version in the previous `Cargo.toml`. When it changes, the workflow creates tag
`v{version}`, creates a GitHub release, uploads raw binaries, and publishes the crate to crates.io.

The workflow builds Linux musl binaries for `aarch64` and `x86_64`, plus a macOS arm64 binary.

Publishing requires a repository Actions secret named `CARGO_REGISTRY_TOKEN`.

Release assets are named:

```text
codex-switch-aarch64-unknown-linux-musl
codex-switch-x86_64-unknown-linux-musl
codex-switch-aarch64-apple-darwin
```
