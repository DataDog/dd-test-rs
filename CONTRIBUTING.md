# Contributing

Community contributions to dd-test-rs are welcome. This page explains how to request features, report bugs, and submit changes.

## Requesting a feature

[Open a GitHub issue](https://github.com/DataDog/dd-test-rs/issues/new/choose) and describe:

- The goal of the feature.
- How you would use it, or how it should behave.
- Links to related resources, such as similar features in other Datadog libraries.

## Reporting a bug

For urgent matters, such as outages, or for issues with the Datadog service or UI, contact [Datadog support](https://docs.datadoghq.com/help/). You get a faster answer there.

To report a bug in dd-test-rs, [open a GitHub issue](https://github.com/DataDog/dd-test-rs/issues/new/choose) with:

- A description of the problem, and the behavior you expected.
- Steps to reproduce it. A small project that reproduces it is best.
- The `dd-test-wrapper` version or commit, and the output of `rustc --version`.
- How you run your tests, such as `cargo test` or `cargo nextest run`, and your OS and CI provider.
- The output of your test run with `DD_TRACE_DEBUG=true`. Remove secrets, such as your API key, first.

Do not report security vulnerabilities in public issues. See [SECURITY.md](SECURITY.md).

## Development

You need Rust 1.88 or later. To test the harness with other toolchains, such as 1.81 or nightly, install them with `rustup`.

Build and test the workspace:

```bash
cargo build --workspace
cargo test --workspace
```

The end-to-end tests build [`fixtures/basic`](fixtures/basic) through the wrapper and check the results a fake Datadog intake receives. `DD_TEST_E2E_TOOLCHAIN` picks the toolchain that builds the fixture and the harness:

```bash
DD_TEST_E2E_TOOLCHAIN=1.81.0 cargo test -p dd-test-wrapper --test e2e
```

The harness is not a workspace member, so `cargo fmt` does not format it. Format it separately:

```bash
cargo fmt --all
rustfmt --edition 2021 --config-path rustfmt.toml crates/dd-test-harness/src/lib.rs
```

## Submitting a change

1. Fork the repository and create a branch from `main`.
2. Make your change, and add tests that show the feature works or the bug is fixed.
3. Update the [README](README.md) when the change affects users, such as a new setting.
4. Run the checks below.
5. Open a pull request against `main`.

A maintainer reviews every pull request. To be merged, a pull request must:

- Explain its goal and describe the changes.
- Pass CI.
- Have a title that follows [Conventional Commits](https://www.conventionalcommits.org/), such as `fix(agent): retry failed uploads`.
- Contain only signed commits. See [signing commits](https://docs.github.com/en/authentication/managing-commit-signature-verification/signing-commits).

Keep pull requests focused on one change. Small pull requests are reviewed faster.

### Checks

Before opening a pull request, run:

```bash
cargo fmt --all --check
rustfmt --check --edition 2021 --config-path rustfmt.toml crates/dd-test-harness/src/lib.rs
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

### License headers

Every Rust file starts with this header:

```rust
// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0
```

### Dependencies

[`LICENSE-3rdparty.csv`](LICENSE-3rdparty.csv) lists every third-party crate the project uses. CI fails when it does not match `Cargo.lock`. When you add, remove, or update a dependency, regenerate it with [dd-rust-license-tool](https://github.com/DataDog/rust-license-tool):

```bash
cargo install dd-rust-license-tool --version 1.0.6 --locked
dd-rust-license-tool write
```

Dependencies must use a license allowed in [`deny.toml`](deny.toml). The release archives include the license texts of the crates, generated from [`about.hbs`](about.hbs). When you allow a new license, add it to [`about.toml`](about.toml) too.

## Releasing

Maintainers release from `main` with the [Release workflow](.github/workflows/release.yml). It requires Maintain or Admin access to the repository.

1. Open a pull request that sets the new version in the `[workspace.package]` section of [`Cargo.toml`](Cargo.toml) and in the `version` of the `dd-test-core` and `dd-test-agent` dependencies, then runs `cargo update --workspace` to update `Cargo.lock`. Merge it.
2. Run the Release workflow on `main`, with the version as input, such as `v0.2.0`. It must match the version in `Cargo.toml`.
3. The workflow builds and tests an archive for each platform, tags the commit, and creates a draft release with generated notes and the archives.
4. Review the draft release, then publish it.

Pull requests that change the release workflow, its scripts, or the license template also build and package the archives, without releasing.
