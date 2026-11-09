//! `ephpm cron` — inspect and trigger the distributed scheduler.
//!
//! `list` is standalone (it reads the config and computes each job's next fire
//! from its schedule and timezone). `status` and `run-now` talk to a running
//! server's internal endpoints (`GET /_ephpm/cron`, `POST /_ephpm/cron/run/…`)
//! over the local HTTP diagnostics listener — the same `[server.diagnostics]
//! token` the other control endpoints use.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use chrono::Utc;
use clap::Subcommand;
use ephpm_config::Config;

/// `ephpm cron <COMMAND>`.
#[derive(Subcommand, Debug)]
pub enum CronCommand {
    /// List configured jobs with their schedule, timezone and next fire.
    List {
        /// The ephpm config file.
        #[arg(long, short = 'c')]
        config: PathBuf,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show live scheduler status from a running server (`GET /_ephpm/cron`).
    Status {
        /// The ephpm config file.
        #[arg(long, short = 'c')]
        config: PathBuf,
    },
    /// Trigger a job now on a running server (honours the per-tick lease).
    #[command(name = "run-now")]
    RunNow {
        /// The job name.
        name: String,
        /// The ephpm config file.
        #[arg(long, short = 'c')]
        config: PathBuf,
        /// Bypass the per-tick lease (a true out-of-band ad-hoc run).
        #[arg(long)]
        force: bool,
    },
}

/// Dispatch an `ephpm cron` subcommand.
///
/// # Errors
///
/// Propagates config-load and HTTP errors.
pub fn run(command: CronCommand) -> anyhow::Result<ExitCode> {
    match command {
        CronCommand::List { config, json } => list(&config, json),
        CronCommand::Status { config } => status(&config),
        CronCommand::RunNow { name, config, force } => run_now(&config, &name, force),
    }
}

fn list(config_path: &PathBuf, json: bool) -> anyhow::Result<ExitCode> {
    let config = Config::load(config_path).context("failed to load configuration")?;
    let now = Utc::now();
    let default_tz = &config.cron.timezone;

    if json {
        let mut jobs = Vec::new();
        for job in &config.cron.jobs {
            let schedule = job.schedule_parsed()?;
            let tz = job.timezone_resolved(default_tz)?;
            let next = schedule.next_after(now, tz);
            jobs.push(serde_json::json!({
                "name": job.name,
                "site": job.site,
                "schedule": job.schedule,
                "timezone": tz.name(),
                "suspended": job.suspend,
                "next_fire": next.map(|t| t.to_rfc3339()),
                "command": job.command,
            }));
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "enabled": config.cron.enabled,
                "jobs": jobs,
            }))?
        );
        return Ok(ExitCode::SUCCESS);
    }

    if !config.cron.enabled {
        println!("[cron] enabled = false — the scheduler does not run.");
    }
    if config.cron.jobs.is_empty() {
        println!("No [[cron.jobs]] configured.");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{:<24} {:<16} {:<14} {:<20} SITE", "NAME", "SCHEDULE", "TZ", "NEXT FIRE (UTC)");
    for job in &config.cron.jobs {
        let schedule = job.schedule_parsed()?;
        let tz = job.timezone_resolved(default_tz)?;
        let next = schedule
            .next_after(now, tz)
            .map_or_else(|| "-".to_string(), |t| t.format("%Y-%m-%d %H:%M").to_string());
        let suspended = if job.suspend { " (suspended)" } else { "" };
        println!(
            "{:<24} {:<16} {:<14} {:<20} {}{}",
            job.name,
            job.schedule,
            tz.name(),
            next,
            job.site.as_deref().unwrap_or("-"),
            suspended,
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn status(config_path: &PathBuf) -> anyhow::Result<ExitCode> {
    let config = Config::load(config_path).context("failed to load configuration")?;
    let url = format!("{}/_ephpm/cron", base_url(&config));
    let body = get(&config, &url)?;
    // Pretty-print the JSON if it parses, else print as-is.
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
        Err(_) => println!("{body}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn run_now(config_path: &PathBuf, name: &str, force: bool) -> anyhow::Result<ExitCode> {
    let config = Config::load(config_path).context("failed to load configuration")?;
    let mut url = format!("{}/_ephpm/cron/run/{name}", base_url(&config));
    if force {
        url.push_str("?force=true");
    }
    let client = reqwest::blocking::Client::new();
    let mut req = client.post(&url);
    if let Some(token) = &config.server.diagnostics.token {
        req = req.bearer_auth(token);
    }
    let resp =
        req.send().with_context(|| format!("failed to reach {url} — is the server running?"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    println!("{body}");
    if status.is_success() { Ok(ExitCode::SUCCESS) } else { Ok(ExitCode::FAILURE) }
}

/// A GET against the diagnostics endpoint, with the bearer token if configured.
fn get(config: &Config, url: &str) -> anyhow::Result<String> {
    let client = reqwest::blocking::Client::new();
    let mut req = client.get(url);
    if let Some(token) = &config.server.diagnostics.token {
        req = req.bearer_auth(token);
    }
    let resp =
        req.send().with_context(|| format!("failed to reach {url} — is the server running?"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{url} returned {status}: {body}");
    }
    Ok(body)
}

/// The base URL of the local diagnostics listener: the `[server.metrics]
/// listen` address when set (where the status endpoint moves), else
/// `[server] listen`, as plain HTTP on the loopback interface.
fn base_url(config: &Config) -> String {
    let addr = config.server.metrics.listen.clone().unwrap_or_else(|| config.server.listen.clone());
    let port = addr.rsplit(':').next().unwrap_or("8080");
    format!("http://127.0.0.1:{port}")
}
