//! pingcheck - ICMP liveness probe that pushes status to an Uptime Kuma
//! push monitor.
//!
//! Configuration comes exclusively from environment variables:
//!
//!   PUSH_URL    base URL of the push endpoint (required)
//!   PUSH_TOKEN  monitor token, appended to the path (required)
//!   TARGET      hostname or IP to ping (required)
//!   INTERVAL    seconds between checks, 10..=120 (default 60)
//!   DRY_RUN     "1" to log the payload instead of sending it
//!   LOG         off|error|warn|info|debug (default info)
//!
//! The process never exits because of a network error: a failed push is
//! logged and the loop continues. Exiting here would turn a transient
//! outage into a systemd restart loop.

use std::env;
use std::net::IpAddr;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use surge_ping::{Client, Config as PingConfig, ICMP, PingIdentifier, PingSequence};

const MIN_INTERVAL: u64 = 10;
const MAX_INTERVAL: u64 = 120;
const DEFAULT_INTERVAL: u64 = 60;

const PING_TIMEOUT: Duration = Duration::from_secs(1);
const PUSH_TIMEOUT: Duration = Duration::from_secs(4);
const PUSH_ATTEMPTS: u32 = 3;
const PUSH_RETRY_DELAY: Duration = Duration::from_secs(2);

const IP_REFRESH: Duration = Duration::from_secs(3600);
const IP_SOURCES: [&str; 2] = ["https://icanhazip.com", "https://api.ipify.org"];

/// Number of consecutive failed probes required before reporting "down".
const DOWN_AFTER: u32 = 2;

struct Settings {
    push_url: String,
    push_token: String,
    target: String,
    interval: Duration,
    dry_run: bool,
}

impl Settings {
    fn from_env() -> Result<Self, String> {
        let push_url = required("PUSH_URL")?;
        let push_url = push_url.trim().trim_end_matches('/').to_string();
        if push_url.is_empty() {
            return Err("PUSH_URL is empty".into());
        }

        let push_token = required("PUSH_TOKEN")?;
        if push_token.trim().is_empty() {
            return Err("PUSH_TOKEN is empty".into());
        }

        let target = required("TARGET")?;
        if target.trim().is_empty() {
            return Err("TARGET is empty".into());
        }

        let interval_s = match env::var("INTERVAL") {
            Ok(v) => v
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("INTERVAL is not a number: {v:?}"))?,
            Err(_) => DEFAULT_INTERVAL,
        };
        if !(MIN_INTERVAL..=MAX_INTERVAL).contains(&interval_s) {
            return Err(format!(
                "INTERVAL out of range: {interval_s} (expected {MIN_INTERVAL}..={MAX_INTERVAL})"
            ));
        }

        let dry_run = match env::var("DRY_RUN") {
            Ok(v) => matches!(v.trim(), "1" | "true" | "yes"),
            Err(_) => false,
        };

        Ok(Self {
            push_url,
            push_token,
            target,
            interval: Duration::from_secs(interval_s),
            dry_run,
        })
    }
}

fn required(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| format!("{name} is not set"))
}

/// Pure state machine, kept separate so it can be unit tested.
fn status_for(fails: u32) -> &'static str {
    if fails >= DOWN_AFTER {
        "down"
    } else {
        "up"
    }
}

fn masked_url(base: &str, token: &str) -> String {
    let shown = if token.len() > 6 {
        format!("{}…", &token[..6])
    } else {
        "…".to_string()
    };
    format!("{base}/{shown}/")
}

/// One ICMP client per address family. Creating a client spawns a receiver
/// task, so it must happen once and be reused: building one per probe would
/// leak a task and a socket on every iteration.
struct Prober {
    v4: Option<Client>,
    v6: Option<Client>,
}

impl Prober {
    fn new() -> Self {
        let v4 = Client::new(&PingConfig::default()).ok();
        let v6 = Client::new(&PingConfig::builder().kind(ICMP::V6).build()).ok();

        if v4.is_none() {
            error!("cannot open the IPv4 ICMP socket (check net.ipv4.ping_group_range)");
        }

        Self { v4, v6 }
    }

    async fn ping(&self, addr: IpAddr) -> Option<Duration> {
        let client = if addr.is_ipv4() { &self.v4 } else { &self.v6 };
        let Some(client) = client.as_ref() else {
            debug!("no ICMP client available for {addr}");
            return None;
        };

        let mut pinger = client
            .pinger(addr, PingIdentifier(std::process::id() as u16))
            .await;
        pinger.timeout(PING_TIMEOUT);

        match pinger.ping(PingSequence(0), &[]).await {
            Ok((_, rtt)) => Some(rtt),
            Err(e) => {
                debug!("ping {addr} failed: {e}");
                None
            }
        }
    }
}

/// Resolves the target on every call, so a DNS change is picked up without
/// any cache, and tries every address the name resolves to.
async fn ping_target(prober: &Prober, target: &str) -> Option<Duration> {
    let addrs: Vec<IpAddr> = match tokio::net::lookup_host((target, 0)).await {
        Ok(iter) => iter.map(|s| s.ip()).collect(),
        Err(e) => {
            warn!("DNS lookup for {target} failed: {e}");
            return None;
        }
    };

    if addrs.is_empty() {
        warn!("DNS lookup for {target} returned no address");
        return None;
    }

    for addr in addrs {
        if let Some(rtt) = prober.ping(addr).await {
            return Some(rtt);
        }
    }

    None
}

fn http_get_text(url: &str) -> Option<String> {
    match ureq::get(url).timeout(PUSH_TIMEOUT).call() {
        Ok(resp) => resp.into_string().ok(),
        Err(e) => {
            debug!("GET {url} failed: {e}");
            None
        }
    }
}

/// Egress IP, validated as an actual address instead of a regex guess.
fn fetch_ip() -> Option<IpAddr> {
    for url in IP_SOURCES {
        let Some(body) = http_get_text(url) else {
            continue;
        };
        let text = body.trim();
        match IpAddr::from_str(text) {
            Ok(ip) => return Some(ip),
            Err(_) => debug!("{url} returned a non-address: {text:?}"),
        }
    }
    None
}

fn rtt_ms(rtt: Option<Duration>) -> String {
    match rtt {
        Some(d) => format!("{:.1}", d.as_secs_f64() * 1000.0),
        None => "0".to_string(),
    }
}

async fn push(settings: &Settings, status: &str, ip: Option<IpAddr>, rtt: Option<Duration>) {
    let url = format!("{}/{}/", settings.push_url, settings.push_token);
    let msg = ip.map(|i| i.to_string()).unwrap_or_else(|| "OK".to_string());
    let ping = rtt_ms(rtt);

    if settings.dry_run {
        info!(
            "[dry-run] would push to {}: status={status} msg={msg} ping={ping}",
            masked_url(&settings.push_url, &settings.push_token)
        );
        return;
    }

    for attempt in 1..=PUSH_ATTEMPTS {
        let result = ureq::get(&url)
            .query("status", status)
            .query("msg", &msg)
            .query("ping", &ping)
            .timeout(PUSH_TIMEOUT)
            .call();

        match result {
            Ok(_) => {
                debug!("push delivered (status={status})");
                return;
            }
            Err(e) => {
                if attempt == PUSH_ATTEMPTS {
                    error!("push failed after {PUSH_ATTEMPTS} attempts: {e}");
                    return;
                }
                warn!("push failed (attempt {attempt}/{PUSH_ATTEMPTS}): {e}");
                tokio::time::sleep(PUSH_RETRY_DELAY).await;
            }
        }
    }
}

fn init_logging() {
    use log::LevelFilter;

    let level = env::var("LOG").unwrap_or_else(|_| "info".to_string());
    let filter = LevelFilter::from_str(level.trim().to_lowercase().as_str())
        .unwrap_or(LevelFilter::Info);

    env_logger::Builder::new()
        .filter_level(filter)
        // With LOG=debug we want our own messages, not a TLS handshake trace.
        .filter_module("ureq", LevelFilter::Warn)
        .filter_module("rustls", LevelFilter::Warn)
        .target(env_logger::Target::Stderr)
        .format_timestamp_secs()
        .init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    init_logging();

    let settings = match Settings::from_env() {
        Ok(s) => s,
        Err(e) => {
            error!("invalid configuration: {e}");
            return ExitCode::from(2);
        }
    };

    info!(
        "starting: target={} interval={}s dry_run={} push={}",
        settings.target,
        settings.interval.as_secs(),
        settings.dry_run,
        masked_url(&settings.push_url, &settings.push_token)
    );

    let mut ip = fetch_ip();
    match ip {
        Some(i) => info!("egress IP: {i}"),
        None => warn!("egress IP unavailable, will fall back to 'OK'"),
    }
    let mut ip_fetched_at = Instant::now();

    let prober = Prober::new();

    let mut fails: u32 = 0;
    let mut last_status: Option<&'static str> = None;

    loop {
        let started = Instant::now();

        let rtt = ping_target(&prober, &settings.target).await;
        if rtt.is_some() {
            fails = 0;
        } else {
            fails = fails.saturating_add(1);
        }

        let status = status_for(fails);

        if ip_fetched_at.elapsed() >= IP_REFRESH {
            ip = fetch_ip().or(ip);
            ip_fetched_at = Instant::now();
            if let Some(i) = ip {
                info!("egress IP refreshed: {i}");
            }
        }

        let rtt_display = match rtt {
            Some(d) => format!("{:.1}ms", d.as_secs_f64() * 1000.0),
            None => "-".to_string(),
        };
        let ip_display = ip
            .map(|i| i.to_string())
            .unwrap_or_else(|| "-".to_string());

        info!("status={status} rtt={rtt_display} fails={fails} ip={ip_display}");

        if last_status != Some(status) {
            if let Some(previous) = last_status {
                info!("state transition: {previous} -> {status}");
            }
            last_status = Some(status);
        }

        push(&settings, status, ip, rtt).await;

        let elapsed = started.elapsed();
        if elapsed < settings.interval {
            tokio::time::sleep(settings.interval - elapsed).await;
        } else {
            warn!(
                "iteration took {elapsed:?}, longer than the {:?} interval",
                settings.interval
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_needs_two_consecutive_failures() {
        assert_eq!(status_for(0), "up");
        assert_eq!(status_for(1), "up");
        assert_eq!(status_for(2), "down");
        assert_eq!(status_for(9), "down");
    }

    #[test]
    fn recovery_resets_after_success() {
        // A success sets fails back to 0, so one more failure is not enough.
        let mut fails = 0;
        fails += 1;
        assert_eq!(status_for(fails), "up");
        fails = 0;
        fails += 1;
        assert_eq!(status_for(fails), "up");
    }

    #[test]
    fn token_is_masked() {
        assert_eq!(
            masked_url("https://example.tld/api/push", "abcdef123456"),
            "https://example.tld/api/push/abcdef…/"
        );
        assert_eq!(
            masked_url("https://example.tld/api/push", "abc"),
            "https://example.tld/api/push/…/"
        );
    }

    #[test]
    fn rtt_is_formatted_in_milliseconds() {
        assert_eq!(rtt_ms(Some(Duration::from_millis(12))), "12.0");
        assert_eq!(rtt_ms(Some(Duration::from_micros(12_340))), "12.3");
        assert_eq!(rtt_ms(None), "0");
    }
}
