# Datadog Test Optimization for Rust

Report your Rust tests to [Datadog Test Optimization](https://docs.datadoghq.com/tests/), and keep flaky tests from breaking your builds.

It works with your existing `cargo test` and `cargo nextest` commands. You do not change your tests or your `Cargo.toml`.

## Features

- [Test Optimization](https://docs.datadoghq.com/tests/): collect results, durations, and errors for every test, with CI and git metadata.
- [Flaky Test Management](https://docs.datadoghq.com/tests/flaky_management/): quarantine or disable flaky tests from Datadog, so known flakes do not fail your builds.
- [Auto Test Retries](https://docs.datadoghq.com/tests/flaky_tests/auto_test_retries/): retry failed tests, so a flaky failure does not fail your build.
- [Early Flake Detection](https://docs.datadoghq.com/tests/flaky_tests/early_flake_detection/): run new tests several times to catch flakiness before it reaches your default branch.

You turn features on and off in Datadog, in the Test Optimization settings of your repository.

## Requirements

- Projects built with Cargo, using the standard test harness.
- Rust 1.81 or later to build and run your tests.
- Rust 1.88 or later to install the wrapper with Cargo or build it from source. You can use a different toolchain than the one your project uses. Prebuilt binaries have no toolchain requirement.
- Linux or macOS.
- A Datadog [API key](https://docs.datadoghq.com/account_management/api-app-keys/). Results go directly to Datadog, so you do not need the Datadog Agent.

## Installation

### Download a prebuilt binary

Prebuilt binaries for Linux and macOS will be attached to each [GitHub release](https://github.com/DataDog/dd-test-rs/releases), starting with the first release.

In GitHub Actions, download one with the `gh` command line tool:

```yaml
- name: Download dd-test-wrapper
  run: |
    gh release download --repo DataDog/dd-test-rs --pattern "dd-test-wrapper-linux-amd64" --dir bin
    mv bin/dd-test-wrapper-linux-amd64 bin/dd-test-wrapper
    chmod +x bin/dd-test-wrapper
    echo "$PWD/bin" >> "$GITHUB_PATH"
  env:
    GH_TOKEN: ${{ github.token }}
```

Or with `curl`:

```bash
mkdir -p bin
curl -fsSL https://github.com/DataDog/dd-test-rs/releases/latest/download/dd-test-wrapper-linux-amd64 -o bin/dd-test-wrapper
chmod +x bin/dd-test-wrapper
```

### Install with Cargo

```bash
cargo install --locked --git https://github.com/DataDog/dd-test-rs dd-test-wrapper
```

Once the crate is published on crates.io, you will also be able to run:

```bash
cargo install --locked dd-test-wrapper
```

### Build from source

```bash
git clone https://github.com/DataDog/dd-test-rs.git
cd dd-test-rs && cargo build --release -p dd-test-wrapper
```

This creates `target/release/dd-test-wrapper`. It requires Rust 1.88 or later.

## Setup

Set three environment variables, then run your tests as usual:

```bash
export RUSTC_WORKSPACE_WRAPPER=dd-test-wrapper
export DD_API_KEY=<your API key>
export DD_SITE=datadoghq.com  # your Datadog site

cargo test
```

`cargo nextest run` works too, but reports each test as its own test session. See [cargo nextest](#cargo-nextest).

`RUSTC_WORKSPACE_WRAPPER` accepts the name of a binary on your `PATH` or a full path. Use the [Datadog site](https://docs.datadoghq.com/getting_started/site/) of your organization for `DD_SITE`, such as `datadoghq.eu` or `us5.datadoghq.com`.

We also recommend setting `DD_SERVICE` and `DD_ENV`, so you can find your tests in Datadog:

```bash
export DD_SERVICE=my-crate
export DD_ENV=ci
```

### GitHub Actions

```yaml
jobs:
  test:
    runs-on: ubuntu-latest
    env:
      RUSTC_WORKSPACE_WRAPPER: dd-test-wrapper
      DD_API_KEY: ${{ secrets.DD_API_KEY }}
      DD_SITE: datadoghq.com
      DD_SERVICE: my-crate
      DD_ENV: ci
    steps:
      - uses: actions/checkout@v4
      - run: cargo install --locked --git https://github.com/DataDog/dd-test-rs dd-test-wrapper
      - run: cargo test
```

The same variables work in any CI provider. CI and git metadata are detected automatically for AppVeyor, AWS CodePipeline, Azure Pipelines, Bitbucket Pipelines, Bitrise, Buddy, Buildkite, CircleCI, Codefresh, Drone, GitHub Actions, GitLab CI, Jenkins, TeamCity, and Travis CI.

## What changes when you run your tests

Your test output, filters, and exit codes stay the same. Datadog only changes the outcome of a test when a feature asks for it:

- A test that fails and then passes on retry is reported to `cargo test` as passed.
- A quarantined test still runs, but its failure does not fail `cargo test`.
- A disabled test is skipped.
- An attempt-to-fix test runs several times, and fails if any execution fails.

When this happens, the test binary prints one summary line at the end, for example:

```text
Datadog Test Optimization: 2 retried executions, 1 quarantined failures ignored
```

If Datadog cannot be reached, or `DD_API_KEY` is not set, your tests run normally and a warning is printed.

The first build with the wrapper recompiles your workspace crates, because Cargo includes the wrapper in its build fingerprint. Your dependencies stay cached.

## How tests appear in Datadog

| Datadog | Rust | Example |
| --- | --- | --- |
| Test session | one test binary | |
| Test module | Cargo package | `bytes` |
| Test suite | crate name and module path | `bytes::bytes_mut::tests` |
| Test | function name | `test_original_capacity_to_repr` |

Tests marked `#[ignore]` are reported as skipped, with their reason when you give one.

When you filter tests, such as with `cargo test my_test`, test binaries without a matching test are not reported.

## Configuration

All settings are environment variables. Boolean values accept `true`, `false`, `1`, and `0`.

### Main settings

| Variable | Default | Description |
| --- | --- | --- |
| `DD_API_KEY` | none | Your Datadog API key. Required. |
| `DD_SITE` | `datadoghq.com` | Your Datadog site. |
| `DD_SERVICE` | repository name | The service your tests belong to. |
| `DD_ENV` | `none` | The environment, such as `ci` or `local`. |
| `DD_TEST_SESSION_NAME` | CI job name and test command | A name that groups test sessions in Datadog. |
| `DD_TAGS` | none | Custom tags for every test event, as `key:value` pairs separated by commas or spaces. Tags named `test.configuration.<name>` also become part of the test configuration, like the OS and Rust version. |
| `DD_CIVISIBILITY_ENABLED` | `true` | Set to `false` to run tests without Datadog. |
| `DD_TRACE_DEBUG` | `false` | Print debug logs. |

### Features

These settings can only turn off a feature that is enabled in Datadog.

| Variable | Default | Description |
| --- | --- | --- |
| `DD_CIVISIBILITY_FLAKY_RETRY_ENABLED` | `true` | Auto Test Retries. |
| `DD_CIVISIBILITY_FLAKY_RETRY_COUNT` | `5` | Retries for each failed test. |
| `DD_CIVISIBILITY_TOTAL_FLAKY_RETRY_COUNT` | `1000` | Maximum number of retried tests in one test binary. |
| `DD_CIVISIBILITY_EARLY_FLAKE_DETECTION_ENABLED` | `true` | Early Flake Detection. |
| `DD_TEST_MANAGEMENT_ENABLED` | `true` | Flaky Test Management. |
| `DD_TEST_MANAGEMENT_ATTEMPT_TO_FIX_RETRIES` | Datadog setting | Executions of an attempt-to-fix test. |
| `DD_INSTRUMENTATION_TELEMETRY_ENABLED` | `true` | Send telemetry about the library itself. |

### Git metadata

Git metadata comes from your CI provider and from the local checkout. Use these variables when neither is available, such as in a container without the `.git` directory.

| Variable | Description |
| --- | --- |
| `DD_GIT_REPOSITORY_URL` | Repository URL. |
| `DD_GIT_COMMIT_SHA` | Full commit SHA. |
| `DD_GIT_BRANCH` | Branch name. |
| `DD_GIT_TAG` | Tag name, for tag builds. |
| `DD_GIT_COMMIT_MESSAGE` | Commit message. |
| `DD_GIT_COMMIT_AUTHOR_NAME`, `DD_GIT_COMMIT_AUTHOR_EMAIL`, `DD_GIT_COMMIT_AUTHOR_DATE` | Commit author. |
| `DD_GIT_COMMIT_COMMITTER_NAME`, `DD_GIT_COMMIT_COMMITTER_EMAIL`, `DD_GIT_COMMIT_COMMITTER_DATE` | Commit committer. |
| `DD_GIT_PULL_REQUEST_BASE_BRANCH`, `DD_GIT_PULL_REQUEST_BASE_BRANCH_SHA` | Pull request base branch and its SHA. |

### Advanced

| Variable | Default | Description |
| --- | --- | --- |
| `DD_CIVISIBILITY_AGENTLESS_URL` | derived from `DD_SITE` | Send every request to this URL instead, such as a proxy. |
| `DD_INSTRUMENTATION_TELEMETRY_URL` | derived from `DD_SITE` | Send telemetry to this URL instead. |
| `DD_TEST_AGENT` | the installed wrapper | Path of the `dd-test-wrapper` binary that test binaries start. Set it when you run test binaries on another machine, or after moving the wrapper. |
| `DD_TEST_READ_CACHE_DIR` | your user cache directory | Where test binaries of the same run share Datadog responses and git metadata. |

## Compatibility

- The wrapper works together with `RUSTC_WRAPPER` tools such as sccache. sccache still caches your dependencies, but not your workspace crates.
- Each test binary is reported as its own test session.
- Retries run in the same process, right after the failed execution. A retried test shares process state, such as statics, with its earlier executions.

### cargo nextest

nextest runs each test in its own process, and each process is reported like a test binary. As a result:

- Each test is reported as its own test session, with its own module and suite.
- `DD_CIVISIBILITY_TOTAL_FLAKY_RETRY_COUNT` limits the retries of each test, not of the whole run.
- Early Flake Detection does not detect runs with too many new tests, because each process sees a single test.

Use `cargo test` to report one test session per test binary.

## Limitations

- Test Impact Analysis and code coverage are not supported yet.
- Only Cargo is supported. Bazel is not.
- These tests run normally, but are not reported to Datadog:
  - doctests;
  - tests that use a custom harness (`harness = false` or `#![test_runner]`);
  - test builds with `panic = "abort"`.
- Results can be lost if the test process is killed with `SIGKILL`, or if Ctrl-C reaches the whole process group.
- Windows is not supported yet.

## Troubleshooting

If your tests do not appear in Datadog:

1. Check that `RUSTC_WORKSPACE_WRAPPER` points to `dd-test-wrapper`, and that `DD_API_KEY` and `DD_SITE` are set where the tests run.
2. Run with `DD_TRACE_DEBUG=true` to see what the library does, including requests to Datadog, how long they took, and their errors. Each debug line starts with the time and the process ID.
3. Warnings starting with `[dd-test warn]` explain why results were not sent, for example `tests are not reported to Datadog: DD_API_KEY is not set`.

## Development

### Prerequisites

- Rust 1.88 or later.
- Optionally, other toolchains to test the harness with them, such as 1.81 or nightly.

### Building

```bash
cargo build --workspace
```

### Testing

```bash
cargo test --workspace
```

The end-to-end tests build [`fixtures/basic`](fixtures/basic) through the wrapper and check the results a fake Datadog intake receives. `DD_TEST_E2E_TOOLCHAIN` picks the toolchain that builds the fixture and the harness:

```bash
DD_TEST_E2E_TOOLCHAIN=1.81.0 cargo test -p dd-test-wrapper --test e2e
```

### Formatting and linting

```bash
cargo fmt --all
rustfmt --edition 2021 --config-path rustfmt.toml crates/dd-test-harness/src/lib.rs
cargo clippy --workspace --all-targets -- -D warnings
```

The harness is not a workspace member, so `cargo fmt` does not format it.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). To report a security vulnerability, see [SECURITY.md](SECURITY.md).

## License

This project is licensed under the [Apache License 2.0](LICENSE). Third-party dependencies and their licenses are listed in [LICENSE-3rdparty.csv](LICENSE-3rdparty.csv).
