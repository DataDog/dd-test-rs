// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Instrumentation telemetry in the `civisibility` namespace, with the metric
//! names and tags other Test Optimization libraries use.

use crate::config::Config;
use dd_test_core::debug;
use libdd_capabilities_impl::NativeCapabilities;
use libdd_common::tag::Tag;
use libdd_telemetry::config::TelemetryEndpoint;
use libdd_telemetry::data::metrics::{MetricNamespace, MetricType};
use libdd_telemetry::metrics::ContextKey;
use libdd_telemetry::worker::{TelemetryWorkerBuilder, TelemetryWorkerFlavor, TelemetryWorkerHandle};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT_MS: u64 = 2000;

pub struct Telemetry {
    handle: TelemetryWorkerHandle<NativeCapabilities>,
    contexts: Mutex<HashMap<(&'static str, bool), ContextKey>>,
}

impl Telemetry {
    pub fn start(
        config: &Config,
        service: &str,
        env: &str,
        rustc_version: &str,
        library_version: &str,
    ) -> Option<Telemetry> {
        if !config.telemetry_enabled {
            return None;
        }
        let mut builder = TelemetryWorkerBuilder::new_fetch_host(
            service.to_string(),
            "rust".to_string(),
            rustc_version.to_string(),
            library_version.to_string(),
        );
        builder.application.env = Some(env.to_string());
        builder.flavor = TelemetryWorkerFlavor::Full;
        builder.config = match worker_config(config) {
            Ok(worker_config) => worker_config,
            Err(error) => {
                debug!("telemetry is disabled: {error}");
                return None;
            }
        };
        let handle = match builder.run::<NativeCapabilities>() {
            Ok(handle) => handle,
            Err(error) => {
                debug!("telemetry is disabled: {error}");
                return None;
            }
        };
        let _ = handle.send_start();
        Some(Telemetry {
            handle,
            contexts: Mutex::new(HashMap::new()),
        })
    }

    fn add(&self, name: &'static str, distribution: bool, value: f64, tags: &[&str]) {
        let key = {
            let mut contexts = self.contexts.lock().unwrap_or_else(|e| e.into_inner());
            *contexts.entry((name, distribution)).or_insert_with(|| {
                let kind = if distribution {
                    MetricType::Distribution
                } else {
                    MetricType::Count
                };
                self.handle.register_metric_context(
                    name.to_string(),
                    Vec::new(),
                    kind,
                    true,
                    MetricNamespace::Civisibility,
                )
            })
        };
        let tags = tags
            .iter()
            .filter_map(|tag| tag.split_once(':'))
            .filter_map(|(key, value)| Tag::new(key, value).ok())
            .collect();
        let _ = self.handle.add_point(value, &key, tags);
    }

    pub fn count(&self, name: &'static str, tags: &[&str]) {
        self.add(name, false, 1.0, tags);
    }

    pub fn distribution(&self, name: &'static str, value: f64, tags: &[&str]) {
        self.add(name, true, value, tags);
    }

    /// Sends what is buffered. Waits at most one second, since every test
    /// binary pays this wait when the telemetry endpoint is unreachable.
    pub fn stop(&self) {
        let _ = self.handle.send_stop();
        self.handle
            .wait_for_shutdown_deadline(Instant::now() + SHUTDOWN_TIMEOUT);
    }
}

fn worker_config(config: &Config) -> Result<libdd_telemetry::config::Config, String> {
    let mut worker_config = libdd_telemetry::config::Config::default();
    worker_config.direct_submission_enabled = true;
    let url = config
        .telemetry_url
        .clone()
        .or_else(|| config.intake_url.clone())
        .unwrap_or_else(|| format!("https://instrumentation-telemetry-intake.{}", config.site));
    let endpoint = TelemetryEndpoint {
        url: Some(url),
        api_key: Some(config.api_key.clone()),
        timeout_ms: REQUEST_TIMEOUT_MS,
        ..TelemetryEndpoint::default()
    };
    worker_config.set_endpoint(endpoint).map_err(|e| e.to_string())?;
    Ok(worker_config)
}

/// Metric tags for a request error, by HTTP status or as a network error.
pub fn error_tags(status: Option<u16>) -> Vec<String> {
    match status {
        Some(status) if (400..500).contains(&status) => {
            vec![
                "error_type:status_code_4xx_response".into(),
                format!("status_code:{status}"),
            ]
        }
        Some(status) if status >= 500 => {
            vec![
                "error_type:status_code_5xx_response".into(),
                format!("status_code:{status}"),
            ]
        }
        Some(_) => vec!["error_type:status_code".into()],
        None => vec!["error_type:network".into()],
    }
}

/// Times a backend request and reports `<prefix>.request`, `_ms` and `_errors`.
pub fn request<T>(
    telemetry: Option<&Telemetry>,
    request: &'static str,
    request_ms: &'static str,
    errors: &'static str,
    call: impl FnOnce() -> Result<T, crate::transport::RequestError>,
) -> Result<T, crate::transport::RequestError> {
    let started = Instant::now();
    let result = call();
    if let Some(t) = telemetry {
        t.count(request, &[]);
        t.distribution(request_ms, started.elapsed().as_secs_f64() * 1000.0, &[]);
        if let Err(error) = &result {
            let tags = error_tags(error.status);
            t.count(errors, &tags.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(env: &[(&str, &str)]) -> String {
        let env: HashMap<&str, &str> = env.iter().copied().collect();
        let config = Config::from_lookup(|key| env.get(key).map(|v| v.to_string()));
        worker_config(&config).unwrap().endpoint().unwrap().url.to_string()
    }

    #[test]
    fn telemetry_goes_to_the_intake() {
        assert_eq!(
            url(&[("DD_API_KEY", "k"), ("DD_SITE", "datadoghq.eu")]),
            "https://instrumentation-telemetry-intake.datadoghq.eu/api/v2/apmtelemetry"
        );
    }
}
