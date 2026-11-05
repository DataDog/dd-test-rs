# Datadog Test Optimization for Rust

> [!NOTE]
> This library is in Preview. Features, settings, and behavior may change before general availability. To report a problem or give feedback, [open an issue](https://github.com/DataDog/dd-test-rs/issues/new/choose).

Datadog's Rust library for instrumenting your tests. It reports tests that run with `cargo test` or `cargo nextest` to [Datadog Test Optimization](https://docs.datadoghq.com/tests/), without changes to your tests or your `Cargo.toml`.

The library ships as `dd-test-wrapper`, a `RUSTC_WORKSPACE_WRAPPER`. When Cargo compiles a test binary, the wrapper builds it with a test harness that reports every test to Datadog and applies the features enabled for your repository.

## Features

- [Test Optimization](https://docs.datadoghq.com/tests/): results, durations, and errors of every test, with CI and git metadata.
- [Flaky Test Management](https://docs.datadoghq.com/tests/flaky_management/): quarantine, disable, or attempt to fix flaky tests from Datadog.
- [Auto Test Retries](https://docs.datadoghq.com/tests/flaky_tests/auto_test_retries/): retry failed tests, so a test that passes on retry does not fail your build.
- [Early Flake Detection](https://docs.datadoghq.com/tests/flaky_tests/early_flake_detection/): run new tests several times to detect flakiness before they reach your default branch.

Features are turned on and off in Datadog, in the Test Optimization settings of your repository.

## Requirements

- Projects built with Cargo, using the standard test harness.
- Rust 1.81 or later to build and run your tests.
- Linux or macOS.
- A Datadog [API key](https://docs.datadoghq.com/account_management/api-app-keys/). Results go directly to Datadog, so the Datadog Agent is not needed.

## Installation

Each [GitHub release](https://github.com/DataDog/dd-test-rs/releases) has an archive for each platform:

| Platform | Archive |
| --- | --- |
| Linux x86-64 | `dd-test-wrapper-linux-amd64.tar.gz` |
| Linux ARM64 | `dd-test-wrapper-linux-arm64.tar.gz` |
| macOS x86-64 | `dd-test-wrapper-darwin-amd64.tar.gz` |
| macOS ARM64 | `dd-test-wrapper-darwin-arm64.tar.gz` |

Each archive contains the `dd-test-wrapper` binary and its licenses. The Linux binaries are statically linked, so they run on any distribution. `SHA256SUMS` lists the checksums of the archives.

For example, with `curl` on Linux x86-64:

```bash
mkdir -p bin
curl -fsSL https://github.com/DataDog/dd-test-rs/releases/latest/download/dd-test-wrapper-linux-amd64.tar.gz | tar -xz -C bin dd-test-wrapper
```

In GitHub Actions, you can download it with the `gh` command line tool instead. See [GitHub Actions](#github-actions).

## Setup

Set these environment variables, then run your tests as usual:

```bash
export RUSTC_WORKSPACE_WRAPPER=dd-test-wrapper
export DD_API_KEY=<your API key>
export DD_SITE=datadoghq.com  # your Datadog site
export DD_SERVICE=my-crate    # defaults to the repository name
export DD_ENV=ci

cargo test
```

`cargo nextest run` works too, but reports each test as its own test session. See [cargo nextest](#cargo-nextest).

`RUSTC_WORKSPACE_WRAPPER` accepts the name of a binary on your `PATH` or a full path. Use the [Datadog site](https://docs.datadoghq.com/getting_started/site/) of your organization for `DD_SITE`, such as `datadoghq.eu` or `us5.datadoghq.com`.

Test output, filters, and exit codes stay the same. When a feature changes the outcome of a test, such as a failed test that passes on retry, the test binary prints a summary line at the end:

```text
Datadog Test Optimization: 2 retried executions, 1 quarantined failures ignored
```

If Datadog cannot be reached, or `DD_API_KEY` is not set, tests run normally and a warning is printed.

The first build with the wrapper recompiles your workspace crates, because Cargo includes the wrapper in its build fingerprint. Dependencies stay cached.

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
      - name: Download dd-test-wrapper
        run: |
          mkdir -p bin
          gh release download --repo DataDog/dd-test-rs --pattern dd-test-wrapper-linux-amd64.tar.gz --output - | tar -xz -C bin dd-test-wrapper
          echo "$PWD/bin" >> "$GITHUB_PATH"
        env:
          GH_TOKEN: ${{ github.token }}
      - run: cargo test
```

The same variables work in any CI provider. CI and git metadata are detected automatically for AppVeyor, AWS CodePipeline, Azure Pipelines, Bitbucket Pipelines, Bitrise, Buddy, Buildkite, CircleCI, Codefresh, Drone, GitHub Actions, GitLab CI, Jenkins, TeamCity, and Travis CI.

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

## What is supported

Each test binary is reported as a test session:

| Datadog | Rust | Example |
| --- | --- | --- |
| Test session | one test binary | |
| Test module | Cargo package | `bytes` |
| Test suite | crate name and module path | `bytes::bytes_mut::tests` |
| Test | function name | `test_original_capacity_to_repr` |

- Tests marked `#[ignore]` are reported as skipped, with their reason when you give one.
- When you filter tests, such as with `cargo test my_test`, test binaries without a matching test are not reported.
- The wrapper works together with `RUSTC_WRAPPER` tools such as sccache. sccache still caches your dependencies, but not your workspace crates.
- Retries run in the same process, right after the failed execution. A retried test shares process state, such as statics, with its earlier executions.

### cargo nextest

nextest runs each test in its own process, and each process is reported like a test binary. As a result:

- Each test is reported as its own test session, with its own module and suite.
- `DD_CIVISIBILITY_TOTAL_FLAKY_RETRY_COUNT` limits the retries of each test, not of the whole run.
- Early Flake Detection does not detect runs with too many new tests, because each process sees a single test.

Use `cargo test` to report one test session per test binary.

### Limitations

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

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) to build and test the project, and to submit changes. To report a security vulnerability, see [SECURITY.md](SECURITY.md).

## License

This project is licensed under the [Apache License 2.0](LICENSE). Third-party dependencies and their licenses are listed in [LICENSE-3rdparty.csv](LICENSE-3rdparty.csv).
