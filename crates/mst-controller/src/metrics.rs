//! Prometheus metrics, served at `/metrics` on `metrics_listen`.
//!
//! Two kinds:
//!
//! - What this replica did (requests, tasks, agent commands, network
//!   passes), counted as it happens with the `metrics` crate. Sum these over
//!   replicas.
//! - The state of the platform (VMs, hosts, open tasks), read from Postgres
//!   at scrape time. Every replica reports the same values; take one (`max`).
//!   Hosts and VMs that are gone simply drop out.

use std::fmt::Write as _;
use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::state::AppState;

/// Installs the process-wide recorder.
pub fn install() -> anyhow::Result<PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Suffix("_seconds".into()),
            &[
                0.005, 0.025, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 180.0, 600.0,
            ],
        )?
        .install_recorder()?;
    metrics::gauge!("mst_build_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);
    metrics::gauge!("mst_leader").set(0.0);
    Ok(handle)
}

/// Counts API requests by route (not by path, which would name every VM).
pub async fn track_http(req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        // The web UI's files and client-side routes.
        .unwrap_or_else(|| "other".into());
    let method = req.method().as_str().to_owned();
    let start = Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16().to_string();
    metrics::histogram!("mst_http_request_duration_seconds", "method" => method.clone(), "route" => route.clone())
        .record(start.elapsed().as_secs_f64());
    metrics::counter!("mst_http_requests_total", "method" => method, "route" => route, "status" => status)
        .increment(1);
    resp
}

/// Records how a task ended.
pub fn task_finished(kind: &str, ok: bool, started: Instant) {
    let outcome = if ok { "succeeded" } else { "failed" };
    metrics::counter!("mst_tasks_finished_total", "kind" => kind.to_owned(), "outcome" => outcome)
        .increment(1);
    metrics::histogram!("mst_task_duration_seconds", "kind" => kind.to_owned())
        .record(started.elapsed().as_secs_f64());
}

/// Records an agent command sent from this replica (forwarded or not).
pub fn agent_command(command: &'static str, outcome: &'static str, started: Instant) {
    metrics::counter!("mst_agent_commands_total", "command" => command, "outcome" => outcome)
        .increment(1);
    metrics::histogram!("mst_agent_command_duration_seconds", "command" => command)
        .record(started.elapsed().as_secs_f64());
}

/// `/metrics`: this replica's counters, then the platform's state.
pub async fn render(State((state, handle)): State<(AppState, PrometheusHandle)>) -> Response {
    let mut out = handle.render();
    if let Err(e) = platform(&state, &mut out).await {
        tracing::warn!(error = %e, "collecting metrics");
        return (StatusCode::SERVICE_UNAVAILABLE, "database unavailable\n").into_response();
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response()
}

async fn platform(state: &AppState, out: &mut String) -> anyhow::Result<()> {
    let db = &state.db;
    let local = state.agents.session_count();
    family(
        out,
        "mst_agent_sessions",
        "gauge",
        "Agent sessions held by this replica.",
    );
    let _ = writeln!(out, "mst_agent_sessions {local}");

    family(
        out,
        "mst_vms",
        "gauge",
        "VMs by status and reported power state.",
    );
    for c in mst_db::stats::vms(db).await? {
        sample(
            out,
            "mst_vms",
            &[("status", &c.a), ("power", &c.b)],
            c.n as f64,
        );
    }
    family(
        out,
        "mst_tasks_open",
        "gauge",
        "Tasks waiting or running, by kind.",
    );
    for c in mst_db::stats::open_tasks(db).await? {
        sample(
            out,
            "mst_tasks_open",
            &[("kind", &c.a), ("state", &c.b)],
            c.n as f64,
        );
    }
    family(
        out,
        "mst_migrations_running",
        "gauge",
        "Live migrations in progress.",
    );
    let _ = writeln!(
        out,
        "mst_migrations_running {}",
        mst_db::stats::migrations(db).await?
    );
    family(out, "mst_replicas", "gauge", "Controller replicas alive.");
    let _ = writeln!(
        out,
        "mst_replicas {}",
        mst_db::stats::live_replicas(db).await?
    );

    let hosts = mst_db::hosts::list(db).await?;
    family(
        out,
        "mst_host_up",
        "gauge",
        "Whether the host's agent holds a session and heartbeats.",
    );
    for h in &hosts {
        let up = if h.is_live() { 1.0 } else { 0.0 };
        sample(
            out,
            "mst_host_up",
            &[("host", &h.name), ("state", &h.state)],
            up,
        );
    }
    family(
        out,
        "mst_host_cert_expiry_timestamp_seconds",
        "gauge",
        "When the host's agent certificate expires (renewed automatically).",
    );
    for h in &hosts {
        sample(
            out,
            "mst_host_cert_expiry_timestamp_seconds",
            &[("host", &h.name)],
            h.cert_expires_at.timestamp() as f64,
        );
    }
    family(
        out,
        "mst_host_last_seen_timestamp_seconds",
        "gauge",
        "When the host's agent was last heard from.",
    );
    for h in &hosts {
        if let Some(t) = h.last_seen_at {
            sample(
                out,
                "mst_host_last_seen_timestamp_seconds",
                &[("host", &h.name)],
                t.timestamp() as f64,
            );
        }
    }
    Ok(())
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

fn sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    let labels: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect();
    let _ = writeln!(out, "{name}{{{}}} {value}", labels.join(","));
}

fn escape(v: &str) -> String {
    v.replace('\\', r"\\")
        .replace('"', r#"\""#)
        .replace('\n', r"\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn label_values_are_escaped() {
        let mut out = String::new();
        super::sample(&mut out, "m", &[("host", "a\"b\\c\nd")], 1.0);
        assert_eq!(out, "m{host=\"a\\\"b\\\\c\\nd\"} 1\n");
    }
}
