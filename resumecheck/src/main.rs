//! resumecheck: post-resume health checks for a laptop, run as a systemd user service.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_lite::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::{Instant, sleep, timeout};

const RULES_DIR: &str = "/etc/opensnitchd/rules";
const MAX_KERNEL_LINES: usize = 5;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
struct Config {
    delay_secs: u64,
    notify_ok: bool,
    dns_hosts: Vec<String>,
    dns_window_secs: u64,
    check_opensnitch: bool,
    check_dns: bool,
    check_fingerprint: bool,
    check_kernel: bool,
    check_failed_units: bool,
    check_bluetooth: bool,
    auto_reset_failed_units: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            delay_secs: 15,
            notify_ok: false,
            dns_hosts: vec!["fedoraproject.org".into(), "cloudflare.com".into()],
            dns_window_secs: 60,
            check_opensnitch: true,
            check_dns: true,
            check_fingerprint: true,
            check_kernel: true,
            check_failed_units: true,
            check_bluetooth: true,
            auto_reset_failed_units: vec!["open-fprintd-resume.service".into()],
        }
    }
}

#[derive(Debug, PartialEq)]
struct Issue {
    title: String,
    fix: String,
}

fn issue(title: impl Into<String>, fix: impl Into<String>) -> Issue {
    Issue {
        title: title.into(),
        fix: fix.into(),
    }
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".local/state"))
        .join("resumecheck")
}

fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
        .join("resumecheck/config.toml")
}

fn parse_config(s: &str) -> Result<Config, String> {
    toml::from_str(s).map_err(|e| e.to_string())
}

fn load_config() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => parse_config(&s).unwrap_or_else(|e| {
            eprintln!("config {}: {e}; using defaults", path.display());
            Config::default()
        }),
        Err(_) => {
            let cfg = Config::default();
            if let (Some(dir), Ok(text)) = (path.parent(), toml::to_string_pretty(&cfg)) {
                let _ = std::fs::create_dir_all(dir);
                let _ = std::fs::write(&path, text);
            }
            cfg
        }
    }
}

/// Run a command with a hard timeout (child is killed on drop). Returns (success, stdout+stderr).
async fn run(cmd: &str, args: &[&str], secs: u64) -> Result<(bool, String), String> {
    let child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match timeout(Duration::from_secs(secs), child).await {
        Err(_) => Err(format!("{cmd} timed out after {secs}s")),
        Ok(Err(e)) => Err(format!("{cmd}: {e}")),
        Ok(Ok(o)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            Ok((o.status.success(), s))
        }
    }
}

fn not_found(e: &str) -> bool {
    e.contains("No such file")
}

// ---- a. OpenSnitch deny rules ----

/// "deny-always-simple-usr-bin-rclone.json" -> "/usr/bin/rclone" (lossy: '-' in names is ambiguous).
fn decode_rule_name(name: &str) -> String {
    let mut s = name.strip_suffix(".json").unwrap_or(name);
    s = s.strip_prefix("deny-").unwrap_or(s);
    for p in [
        "always-",
        "once-",
        "until-restart-",
        "1h-",
        "30s-",
        "5m-",
        "15m-",
    ] {
        if let Some(r) = s.strip_prefix(p) {
            s = r;
            break;
        }
    }
    for p in ["simple-", "list-", "regexp-", "network-"] {
        if let Some(r) = s.strip_prefix(p) {
            s = r;
            break;
        }
    }
    format!("/{}", s.replace('-', "/"))
}

fn known_path() -> PathBuf {
    state_dir().join("known_deny_rules")
}

/// Rules to report: ones not in the known set (none on the very first run, which just
/// baselines) plus any modified after the suspend started.
fn rules_to_report(
    current: &BTreeSet<String>,
    known: Option<&BTreeSet<String>>,
    recent: BTreeSet<String>,
) -> BTreeSet<String> {
    let mut report: BTreeSet<String> = match known {
        Some(k) => current.difference(k).cloned().collect(),
        None => BTreeSet::new(),
    };
    report.extend(recent);
    report
}

fn write_atomic(p: &Path, text: &str) -> std::io::Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, p)
}

fn check_opensnitch(suspend: Option<u64>, persist: bool) -> Vec<Issue> {
    check_opensnitch_in(Path::new(RULES_DIR), &known_path(), suspend, persist)
}

/// Deny-rule check for `dir`, tracking already-seen rules in file `kp`.
fn check_opensnitch_in(dir: &Path, kp: &Path, suspend: Option<u64>, persist: bool) -> Vec<Issue> {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return vec![], // not installed / not listable
    };
    let mut current: BTreeSet<String> = BTreeSet::new();
    let mut recent: BTreeSet<String> = BTreeSet::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("deny-") || !name.ends_with(".json") {
            continue;
        }
        if let (Some(t), Ok(m)) = (suspend, e.metadata().and_then(|m| m.modified()))
            && m.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) > t
        {
            recent.insert(name.clone());
        }
        current.insert(name);
    }
    let known: Option<BTreeSet<String>> = std::fs::read_to_string(kp).ok().map(|s| {
        s.lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    });
    let report = rules_to_report(&current, known.as_ref(), recent);
    if persist {
        let text: Vec<&str> = current.iter().map(String::as_str).collect();
        if let Err(e) = write_atomic(kp, &text.join("\n")) {
            eprintln!("cannot save {}: {e}", kp.display());
        }
    }
    report
        .into_iter()
        .map(|n| {
            issue(
                format!(
                    "New OpenSnitch deny rule for {} (approx. path)",
                    decode_rule_name(&n)
                ),
                format!(
                    "Open OpenSnitch and delete rule {}",
                    n.trim_end_matches(".json")
                ),
            )
        })
        .collect()
}

// ---- b. DNS / network ----

async fn check_dns(cfg: &Config) -> Vec<Issue> {
    let mut out = vec![];
    let deadline = Instant::now() + Duration::from_secs(cfg.dns_window_secs);
    let mut ok = cfg.dns_hosts.is_empty();
    let mut failing: Vec<String> = vec![];
    loop {
        failing.clear();
        for h in &cfg.dns_hosts {
            let r = timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host((h.as_str(), 443)),
            )
            .await;
            let resolved = match r {
                Ok(Ok(mut it)) => it.next().is_some(),
                _ => false,
            };
            if resolved {
                ok = true;
            } else {
                failing.push(h.clone());
            }
        }
        if ok || Instant::now() >= deadline {
            break;
        }
        sleep(Duration::from_secs(5)).await;
    }
    if !ok {
        out.push(issue(
            format!("DNS still failing after {}s ({})", cfg.dns_window_secs, failing.join(", ")),
            "Check Wi-Fi; try `resolvectl flush-caches` or `nmcli networking off && nmcli networking on`",
        ));
    }
    match run("nmcli", &["-t", "-f", "CONNECTIVITY", "general"], 10).await {
        Ok((true, s)) => {
            out.extend(connectivity_issue(&s));
        }
        Ok((false, s)) => out.push(issue(
            format!("nmcli failed: {}", s.trim()),
            "Check NetworkManager.service",
        )),
        Err(e) if not_found(&e) => {}
        Err(e) => out.push(issue(e, "Check NetworkManager.service")),
    }
    out
}

fn connectivity_issue(s: &str) -> Option<Issue> {
    let c = s.trim();
    (c == "none" || c == "limited").then(|| {
        issue(
            format!("NetworkManager connectivity: {c}"),
            "Reconnect or toggle Wi-Fi (`nmcli radio wifi off && nmcli radio wifi on`)",
        )
    })
}

// ---- c. fingerprint ----

const FP_FIX: &str = "systemctl restart python3-validity open-fprintd (needs sudo)";

fn fingerprint_issue(ok: bool, out: &str) -> Option<Issue> {
    let first = out
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("fprintd-list failed");
    let bad = !ok || out.contains("No such device") || out.contains("No devices available");
    bad.then(|| issue(format!("Fingerprint reader: {first}"), FP_FIX))
}

async fn current_user() -> String {
    for v in ["USER", "LOGNAME"] {
        if let Ok(u) = std::env::var(v)
            && !u.is_empty()
        {
            return u;
        }
    }
    match run("id", &["-un"], 5).await {
        Ok((true, s)) => s.trim().to_owned(),
        _ => String::new(),
    }
}

async fn check_fingerprint() -> Vec<Issue> {
    let user = current_user().await;
    match run("fprintd-list", &[&user], 10).await {
        Ok((ok, s)) => fingerprint_issue(ok, &s).into_iter().collect(),
        Err(e) if not_found(&e) => vec![],
        Err(e) => vec![issue(format!("Fingerprint reader: {e}"), FP_FIX)],
    }
}

// ---- d. kernel log ----

const KERNEL_PATTERNS: [&str; 5] = [
    "HC died",
    "not responding, assume dead",
    "Unable to change power state",
    "Call Trace",
    "BUG:",
];

async fn check_kernel(since: &str) -> Vec<Issue> {
    let arg = format!("--since={since}");
    let text = match run("journalctl", &["-k", &arg, "-o", "cat", "--no-pager"], 20).await {
        Ok((true, s)) => s,
        Ok((false, s)) => {
            return vec![issue(
                format!("journalctl -k failed: {}", s.trim()),
                "Check journal access",
            )];
        }
        Err(e) if not_found(&e) => return vec![],
        Err(e) => return vec![issue(e, "Check journal access")],
    };
    analyze_kernel(&text).into_iter().collect()
}

fn is_kernel_error(l: &str) -> bool {
    // Fingerprint reader disconnects are expected across suspend.
    !l.contains("USB disconnect") && KERNEL_PATTERNS.iter().any(|p| l.contains(p))
}

fn analyze_kernel(text: &str) -> Option<Issue> {
    let mut seen = BTreeSet::new();
    let mut lines = vec![];
    let mut xhci = false;
    let mut total = 0;
    for l in text.lines().map(str::trim).filter(|l| is_kernel_error(l)) {
        xhci |= l.contains("HC died") || l.contains("assume dead") || l.contains("D3hot to D0");
        total += 1;
        if seen.insert(l.to_owned()) && lines.len() < MAX_KERNEL_LINES {
            lines.push(l.to_owned());
        }
    }
    if total == 0 {
        return None;
    }
    let fix = if xhci {
        "Thunderbolt controller died; replug or `echo 1 | sudo tee /sys/bus/pci/rescan`"
    } else {
        "Inspect `journalctl -k`; consider rebooting"
    };
    Some(issue(
        format!(
            "Kernel errors ({total} lines, {} unique): {}",
            seen.len(),
            lines.join(" | ")
        ),
        fix,
    ))
}

// ---- e. failed units ----

/// Unit names from `systemctl --failed --plain --no-legend` (tolerates a leading bullet).
fn parse_failed_units(s: &str) -> Vec<&str> {
    s.lines()
        .filter_map(|l| l.split_whitespace().find(|w| *w != "●" && *w != "*"))
        .filter(|u| u.contains('.'))
        .collect()
}

async fn check_failed_units(auto_reset: &[String]) -> Vec<Issue> {
    let mut out = vec![];
    for (scope, args) in [
        (
            "system",
            ["--failed", "--plain", "--no-legend", "--no-pager"].as_slice(),
        ),
        (
            "user",
            ["--user", "--failed", "--plain", "--no-legend", "--no-pager"].as_slice(),
        ),
    ] {
        match run("systemctl", args, 10).await {
            Ok((_, s)) => {
                let units = parse_failed_units(&s);
                if !units.is_empty() {
                    let mut reset_attempted = Vec::new();
                    for unit in &units {
                        if auto_reset.iter().any(|target| target == unit) {
                            let reset_args = if scope == "user" {
                                vec!["--user", "reset-failed", *unit]
                            } else {
                                vec!["reset-failed", *unit]
                            };
                            let _ = run("systemctl", &reset_args, 5).await;
                            reset_attempted.push(*unit);
                        }
                    }
                    let sc = if scope == "user" { "--user " } else { "" };
                    let mut title = format!("Failed {scope} units: {}", units.join(", "));
                    if !reset_attempted.is_empty() {
                        title.push_str(&format!(
                            " (auto-reset attempted: {})",
                            reset_attempted.join(", ")
                        ));
                    }
                    out.push(issue(
                        title,
                        format!("systemctl {sc}status <unit>; systemctl {sc}reset-failed"),
                    ));
                }
            }
            Err(e) if not_found(&e) => {}
            Err(e) => out.push(issue(e, "Check systemd")),
        }
    }
    out
}

async fn check_bluetooth() -> Vec<Issue> {
    match run("rfkill", &["list", "bluetooth"], 5).await {
        Ok((true, s)) => {
            if s.contains("Soft blocked: yes") {
                vec![issue(
                    "Bluetooth adapter is soft-blocked",
                    "Unblock bluetooth: `rfkill unblock bluetooth` or toggle in GNOME Settings",
                )]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

// ---- orchestration ----

async fn run_checks(cfg: &Config, suspend: Option<u64>, since: &str, persist: bool) -> Vec<Issue> {
    let (a, b, c, d, e, f) = tokio::join!(
        async {
            if cfg.check_opensnitch {
                check_opensnitch(suspend, persist)
            } else {
                vec![]
            }
        },
        async {
            if cfg.check_dns {
                check_dns(cfg).await
            } else {
                vec![]
            }
        },
        async {
            if cfg.check_fingerprint {
                check_fingerprint().await
            } else {
                vec![]
            }
        },
        async {
            if cfg.check_kernel {
                check_kernel(since).await
            } else {
                vec![]
            }
        },
        async {
            if cfg.check_failed_units {
                check_failed_units(&cfg.auto_reset_failed_units).await
            } else {
                vec![]
            }
        },
        async {
            if cfg.check_bluetooth {
                check_bluetooth().await
            } else {
                vec![]
            }
        },
    );
    [a, b, c, d, e, f].into_iter().flatten().collect()
}

fn fmt_utc(t: u64) -> String {
    let (days, rem) = ((t / 86400) as i64, t % 86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Notification bodies are Pango-ish markup; escape what kernel lines may contain.
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Journal window start: the suspend time, or shortly before resume if unknown.
fn since_for(suspend: Option<u64>, resumed: u64) -> String {
    format!("@{}", suspend.unwrap_or(resumed.saturating_sub(10)))
}

fn body_of(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(|i| format!("- {}\n  Fix: {}", i.title, i.fix))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn notify(summary: &str, body: &str, urgent: bool) -> zbus::Result<()> {
    let conn = zbus::Connection::session().await?;
    let hints: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
    let icon = if urgent {
        "dialog-warning"
    } else {
        "dialog-information"
    };
    let args = (
        "resumecheck",
        0u32,
        icon,
        summary,
        body,
        Vec::<&str>::new(),
        hints,
        if urgent { 0i32 } else { 10_000 },
    );
    let call = conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &args,
    );
    timeout(Duration::from_secs(10), call)
        .await
        .map_err(|_| zbus::Error::Failure("notify timed out".into()))??;
    Ok(())
}

async fn report(cfg: &Config, issues: &[Issue], tag: &str) {
    let line = if issues.is_empty() {
        format!("{} {tag} ok", fmt_utc(now_epoch()))
    } else {
        let t: Vec<&str> = issues.iter().map(|i| i.title.as_str()).collect();
        format!(
            "{} {tag} issues={}: {}",
            fmt_utc(now_epoch()),
            issues.len(),
            t.join("; ").replace('\n', " ")
        )
    };
    eprintln!("{line}");
    let _ = std::fs::create_dir_all(state_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir().join("history.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
    let res = if !issues.is_empty() {
        notify(
            &format!("resumecheck: {} issue(s) after resume", issues.len()),
            &escape_markup(&body_of(issues)),
            true,
        )
        .await
    } else if cfg.notify_ok {
        notify(
            "resumecheck: all OK",
            "No problems detected after resume.",
            false,
        )
        .await
    } else {
        Ok(())
    };
    if let Err(e) = res {
        eprintln!("notification failed: {e}");
    }
}

async fn take_sleep_inhibitor(proxy: &zbus::Proxy<'_>) -> Option<zbus::zvariant::OwnedFd> {
    match proxy
        .call_method(
            "Inhibit",
            &(
                "sleep",
                "resumecheck",
                "Pre-suspend and post-resume state sync",
                "delay",
            ),
        )
        .await
    {
        Ok(reply) => reply.body().deserialize::<zbus::zvariant::OwnedFd>().ok(),
        Err(e) => {
            eprintln!("resumecheck: sleep delay inhibitor not available: {e}");
            None
        }
    }
}

async fn daemon(cfg: Config) -> zbus::Result<()> {
    // Startup: report any deny rules not previously seen.
    let startup = check_opensnitch(None, true);
    if !startup.is_empty() {
        report(&cfg, &startup, "startup").await;
    }
    let conn = zbus::Connection::system().await?;
    let proxy = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let mut stream = proxy.receive_signal("PrepareForSleep").await?;
    let mut suspend: Option<u64> = None;
    let mut pending: Option<tokio::task::JoinHandle<()>> = None;
    let mut inhibitor = take_sleep_inhibitor(&proxy).await;
    eprintln!(
        "resumecheck: listening for PrepareForSleep (sleep delay lock: {})",
        if inhibitor.is_some() {
            "active"
        } else {
            "none"
        }
    );
    while let Some(msg) = stream.next().await {
        let Ok(going_to_sleep) = msg.body().deserialize::<bool>() else {
            continue;
        };
        if going_to_sleep {
            // Drop delay inhibitor lock so systemd logind proceeds with sleep.
            drop(inhibitor.take());
            // Suspended again before the previous post-resume check ran: cancel it but keep
            // its (earlier) window start so no kernel errors fall into a gap.
            let unfinished = pending.take().is_some_and(|h| {
                let u = !h.is_finished();
                h.abort();
                u
            });
            if unfinished && suspend.is_some() {
                eprintln!("suspended again before the check ran; merging windows");
            } else {
                suspend = Some(now_epoch());
            }
            continue;
        }
        // Woke up: immediately re-acquire inhibitor lock for the next sleep cycle.
        inhibitor = take_sleep_inhibitor(&proxy).await;
        let resumed = now_epoch();
        let since = since_for(suspend, resumed);
        let cfg = cfg.clone();
        let susp = suspend; // keep: a re-suspend before the check ran reuses it
        pending = Some(tokio::spawn(async move {
            sleep(Duration::from_secs(cfg.delay_secs)).await;
            let issues = run_checks(&cfg, susp, &since, true).await;
            report(&cfg, &issues, "resume").await;
        }));
    }
    // Stream ended: the system bus went away. Fail so systemd restarts us.
    Err(zbus::Error::Failure("PrepareForSleep stream ended".into()))
}

fn history_path() -> PathBuf {
    state_dir().join("history.log")
}

#[derive(Debug, PartialEq, Default)]
struct HistoryStats {
    total: usize,
    ok: usize,
    issues: usize,
    last: Option<String>,
}

fn parse_history_stats(content: &str) -> HistoryStats {
    let mut total = 0;
    let mut ok = 0;
    let mut issues = 0;
    let mut last = None;
    for line in content.lines().filter(|l| !l.trim().is_empty()) {
        total += 1;
        last = Some(line.to_string());
        if line.contains(" ok") {
            ok += 1;
        } else if line.contains(" issues=") {
            issues += 1;
        }
    }
    HistoryStats {
        total,
        ok,
        issues,
        last,
    }
}

#[derive(Serialize)]
struct ResumecheckStatusJson<'a> {
    config_file: String,
    delay_secs: u64,
    dns_window_secs: u64,
    notify_ok: bool,
    active_checks: Vec<&'a str>,
    auto_reset_units: Vec<String>,
    history_total: usize,
    history_ok: usize,
    history_issues: usize,
    clean_pct: usize,
    last_event: Option<String>,
}

fn print_status(cfg: &Config, json: bool) {
    let p = config_path();
    let mut checks = Vec::new();
    if cfg.check_opensnitch {
        checks.push("opensnitch");
    }
    if cfg.check_dns {
        checks.push("dns");
    }
    if cfg.check_fingerprint {
        checks.push("fingerprint");
    }
    if cfg.check_kernel {
        checks.push("kernel");
    }
    if cfg.check_failed_units {
        checks.push("failed_units");
    }
    if cfg.check_bluetooth {
        checks.push("bluetooth");
    }

    let hp = history_path();
    let stats = if let Ok(content) = std::fs::read_to_string(&hp) {
        parse_history_stats(&content)
    } else {
        HistoryStats {
            total: 0,
            ok: 0,
            issues: 0,
            last: None,
        }
    };
    let clean_pct = stats
        .ok
        .checked_mul(100)
        .and_then(|v| v.checked_div(stats.total))
        .unwrap_or(100);

    if json {
        let status_json = ResumecheckStatusJson {
            config_file: if p.exists() {
                p.display().to_string()
            } else {
                "defaults".to_string()
            },
            delay_secs: cfg.delay_secs,
            dns_window_secs: cfg.dns_window_secs,
            notify_ok: cfg.notify_ok,
            active_checks: checks,
            auto_reset_units: cfg.auto_reset_failed_units.clone(),
            history_total: stats.total,
            history_ok: stats.ok,
            history_issues: stats.issues,
            clean_pct,
            last_event: stats.last,
        };
        if let Ok(j) = serde_json::to_string_pretty(&status_json) {
            println!("{j}");
        }
        return;
    }

    println!(
        "resumecheck configuration ({}):",
        if p.exists() {
            p.display().to_string()
        } else {
            "defaults".to_string()
        }
    );
    println!(
        "  post-resume delay: {}s | DNS retry window: {}s | notify_ok: {}",
        cfg.delay_secs, cfg.dns_window_secs, cfg.notify_ok
    );
    println!("  active checks: {}", checks.join(", "));
    if !cfg.auto_reset_failed_units.is_empty() {
        println!(
            "  auto-reset units: {}",
            cfg.auto_reset_failed_units.join(", ")
        );
    }
    if hp.exists() {
        println!("\nResume history ({}):", hp.display());
        if stats.total == 0 {
            println!("  no resume events recorded yet");
        } else {
            println!(
                "  total resumes recorded: {} ({} ok, {} with issues — {}% clean)",
                stats.total, stats.ok, stats.issues, clean_pct
            );
            if let Some(last) = stats.last {
                println!("  last event: {last}");
            }
        }
    } else {
        println!("\nResume history: no history log found at {}", hp.display());
    }
}

fn print_history(limit: usize) {
    let hp = history_path();
    let content = match std::fs::read_to_string(&hp) {
        Ok(c) => c,
        Err(_) => {
            println!("No history log found at {}", hp.display());
            return;
        }
    };
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        println!("History log is empty.");
        return;
    }
    let take = limit.min(lines.len());
    let start = lines.len().saturating_sub(take);
    println!(
        "Recent resume history (showing last {} of {} events):",
        take,
        lines.len()
    );
    for line in &lines[start..] {
        println!("  {line}");
    }
}

fn check_config_file() -> Result<bool, String> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse_config(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(true)
}

#[derive(Debug, PartialEq, Eq)]
enum CommandMode {
    Daemon,
    Now { since: Option<String> },
    Status { json: bool },
    History { limit: usize },
    CheckConfig,
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    mode: CommandMode,
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<Args, String> {
    let mut mode = None;
    let mut since = None;
    let mut history_limit = 15;
    let mut status_json = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--now" => {
                if mode.is_some() && !matches!(mode, Some(CommandMode::Now { .. })) {
                    return Err("--now cannot be combined with status/history/check-config".into());
                }
                mode = Some(CommandMode::Now { since: None });
            }
            "--since" => {
                since = Some(args.next().ok_or("--since needs a value")?);
            }
            s if s.starts_with("--since=") => {
                since = Some(s["--since=".len()..].to_owned());
            }
            "status" | "--status" => {
                if mode.is_some() && !matches!(mode, Some(CommandMode::Status { .. })) {
                    return Err("conflicting commands specified".into());
                }
                mode = Some(CommandMode::Status { json: false });
            }
            "--json" => {
                status_json = true;
            }
            "history" | "--history" => {
                if mode.is_some() {
                    return Err("conflicting commands specified".into());
                }
                mode = Some(CommandMode::History {
                    limit: history_limit,
                });
            }
            "--limit" => {
                let val = args.next().ok_or("--limit needs a value")?;
                history_limit = val.parse().map_err(|_| format!("invalid limit: {val}"))?;
                if let Some(CommandMode::History { ref mut limit }) = mode {
                    *limit = history_limit;
                }
            }
            s if s.starts_with("--limit=") => {
                let val = &s["--limit=".len()..];
                history_limit = val.parse().map_err(|_| format!("invalid limit: {val}"))?;
                if let Some(CommandMode::History { ref mut limit }) = mode {
                    *limit = history_limit;
                }
            }
            "check-config" | "--check-config" => {
                if mode.is_some() {
                    return Err("conflicting commands specified".into());
                }
                mode = Some(CommandMode::CheckConfig);
            }
            "-h" | "--help" => {
                println!(
                    "usage: resumecheck [--now [--since <time spec>]] | resumecheck status [--json] | resumecheck history [--limit N] | resumecheck check-config"
                );
                std::process::exit(0);
            }
            o => return Err(format!("unknown argument: {o}")),
        }
    }

    let final_mode = match mode {
        Some(CommandMode::Now { .. }) => CommandMode::Now { since },
        Some(CommandMode::Status { .. }) => CommandMode::Status { json: status_json },
        Some(CommandMode::History { .. }) => CommandMode::History {
            limit: history_limit,
        },
        Some(m) => {
            if since.is_some() {
                return Err("--since only applies to --now".into());
            }
            m
        }
        None => {
            if since.is_some() {
                CommandMode::Now { since }
            } else if status_json {
                CommandMode::Status { json: true }
            } else {
                CommandMode::Daemon
            }
        }
    };
    Ok(Args { mode: final_mode })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = parse_args(std::env::args().skip(1)).unwrap_or_else(|e| {
        eprintln!("{e}\nusage: resumecheck [--now [--since <journalctl time spec>]] | resumecheck status [--json] | resumecheck history [--limit N] | resumecheck check-config");
        std::process::exit(2);
    });
    let cfg = load_config();
    match args.mode {
        CommandMode::Status { json } => {
            print_status(&cfg, json);
        }
        CommandMode::History { limit } => {
            print_history(limit);
        }
        CommandMode::CheckConfig => match check_config_file() {
            Ok(true) => println!("{}: configuration is valid", config_path().display()),
            Ok(false) => println!("no config found; defaults are valid"),
            Err(e) => {
                eprintln!("resumecheck check-config: {e}");
                std::process::exit(1);
            }
        },
        CommandMode::Now { since } => {
            let since = since.unwrap_or_else(|| format!("@{}", now_epoch().saturating_sub(3600)));
            let issues = run_checks(&cfg, None, &since, false).await;
            if issues.is_empty() {
                println!("All checks OK (kernel log since {since}).");
            } else {
                println!(
                    "{} issue(s) found (kernel log since {since}):\n{}",
                    issues.len(),
                    body_of(&issues)
                );
            }
            report(&cfg, &issues, "manual").await;
        }
        CommandMode::Daemon => {
            if let Err(e) = daemon(cfg).await {
                eprintln!("resumecheck: {e}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("resumecheck-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rule_name_decoding() {
        assert_eq!(
            decode_rule_name("deny-always-simple-usr-bin-rclone.json"),
            "/usr/bin/rclone"
        );
        assert_eq!(
            decode_rule_name("deny-once-simple-usr-bin-curl.json"),
            "/usr/bin/curl"
        );
        assert_eq!(
            decode_rule_name("deny-until-restart-simple-opt-x-y.json"),
            "/opt/x/y"
        );
        assert_eq!(decode_rule_name("deny-30s-list-foo.json"), "/foo");
        assert_eq!(decode_rule_name("deny-always-regexp-foo.json"), "/foo");
        // Unrecognised shape: still produces something sane.
        assert_eq!(decode_rule_name("deny-weird.json"), "/weird");
        assert_eq!(decode_rule_name("deny-always-simple-.json"), "/");
        // Only the first duration/type prefix is stripped.
        assert_eq!(
            decode_rule_name("deny-always-simple-always-bin.json"),
            "/always/bin"
        );
    }

    #[test]
    fn report_logic() {
        let cur = set(&["deny-a.json", "deny-b.json"]);
        // First run baselines silently.
        assert!(rules_to_report(&cur, None, set(&[])).is_empty());
        // First run still reports rules created after suspend.
        assert_eq!(
            rules_to_report(&cur, None, set(&["deny-b.json"])),
            set(&["deny-b.json"])
        );
        let known = set(&["deny-a.json"]);
        assert_eq!(
            rules_to_report(&cur, Some(&known), set(&[])),
            set(&["deny-b.json"])
        );
        // Removed rules are not reported; union with recent is deduplicated.
        let known = set(&["deny-a.json", "deny-b.json", "deny-gone.json"]);
        assert!(rules_to_report(&cur, Some(&known), set(&[])).is_empty());
        assert_eq!(
            rules_to_report(&cur, Some(&known), set(&["deny-a.json"])).len(),
            1
        );
    }

    #[test]
    fn opensnitch_dir_scan_end_to_end() {
        let d = tmpdir("os");
        let kp = d.join("state/known");
        let rules = d.join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("allow-always-simple-usr-bin-curl.json"), "{}").unwrap();
        std::fs::write(rules.join("deny-always-simple-usr-bin-a.json"), "{}").unwrap();
        std::fs::write(rules.join("deny-notes.txt"), "x").unwrap();
        // Missing dir => no issues, no panic.
        assert!(check_opensnitch_in(&d.join("nope"), &kp, None, true).is_empty());
        // First run: baseline.
        assert!(check_opensnitch_in(&rules, &kp, None, true).is_empty());
        assert_eq!(
            std::fs::read_to_string(&kp).unwrap(),
            "deny-always-simple-usr-bin-a.json"
        );
        // Second run: nothing new.
        assert!(check_opensnitch_in(&rules, &kp, None, true).is_empty());
        // New rule is reported once; with persist=false it is reported again.
        std::fs::write(rules.join("deny-once-simple-usr-bin-rclone.json"), "{}").unwrap();
        let v = check_opensnitch_in(&rules, &kp, None, false);
        assert_eq!(v.len(), 1);
        assert!(v[0].title.contains("/usr/bin/rclone"), "{}", v[0].title);
        assert_eq!(check_opensnitch_in(&rules, &kp, None, true).len(), 1);
        assert!(check_opensnitch_in(&rules, &kp, None, true).is_empty());
        // Recently modified (after suspend) rules are reported even if known.
        let past = now_epoch() - 100;
        assert_eq!(check_opensnitch_in(&rules, &kp, Some(past), true).len(), 2);
        assert!(check_opensnitch_in(&rules, &kp, Some(now_epoch() + 100), true).is_empty());
    }

    #[test]
    fn kernel_matching() {
        assert!(analyze_kernel("").is_none());
        assert!(
            analyze_kernel("usb 1-2: USB disconnect, device number 3\nPM: suspend exit").is_none()
        );
        let log = "PM: suspend exit\n  xhci_hcd 0000:3c:00.0: HC died; cleaning up \nxhci_hcd 0000:3c:00.0: HC died; cleaning up\npcieport 0000:07:00.0: Unable to change power state from D3hot to D0, device inaccessible\nusb 1-2: USB disconnect HC died";
        let i = analyze_kernel(log).unwrap();
        assert!(
            i.title.starts_with("Kernel errors (3 lines, 2 unique): "),
            "{}",
            i.title
        );
        assert!(i.fix.contains("Thunderbolt"));
        let i = analyze_kernel("Call Trace:\nBUG: something").unwrap();
        assert!(i.fix.contains("journalctl"));
    }

    #[test]
    fn kernel_line_cap() {
        let log: String = (0..20).map(|n| format!("BUG: thing {n}\n")).collect();
        let i = analyze_kernel(&log).unwrap();
        assert!(i.title.contains("20 lines, 20 unique"));
        assert_eq!(i.title.matches(" | ").count(), MAX_KERNEL_LINES - 1);
    }

    #[test]
    fn failed_unit_parsing() {
        assert!(parse_failed_units("").is_empty());
        let s = "foo.service loaded failed failed Foo\nbar.timer loaded failed failed Bar\n";
        assert_eq!(parse_failed_units(s), vec!["foo.service", "bar.timer"]);
        assert_eq!(
            parse_failed_units("\u{25cf} foo.service loaded failed failed Foo"),
            vec!["foo.service"]
        );
        assert!(parse_failed_units("0 loaded units listed.").is_empty());
    }

    #[test]
    fn fingerprint_classification() {
        assert!(
            fingerprint_issue(
                true,
                "found 1 devices\nDevice at /x\nFingerprints for user a:\n - #0: right-index-finger"
            )
            .is_none()
        );
        assert!(fingerprint_issue(true, "No devices available").is_some());
        assert!(fingerprint_issue(true, "Impossible to establish: No such device").is_some());
        let i = fingerprint_issue(false, "\nlist_fingerprints failed: boom\nmore").unwrap();
        assert!(i.title.ends_with("list_fingerprints failed: boom"));
        assert!(
            fingerprint_issue(false, "")
                .unwrap()
                .title
                .contains("fprintd-list failed")
        );
    }

    #[test]
    fn connectivity() {
        assert!(connectivity_issue("full\n").is_none());
        assert!(connectivity_issue("portal").is_none());
        assert!(connectivity_issue("limited\n").is_some());
        assert!(connectivity_issue("none").is_some());
    }

    #[test]
    fn time_helpers() {
        assert_eq!(fmt_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(fmt_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(fmt_utc(1_761_847_086), "2025-10-30T17:58:06Z");
        assert_eq!(since_for(Some(1000), 2000), "@1000");
        assert_eq!(since_for(None, 2000), "@1990");
        assert_eq!(since_for(None, 5), "@0");
    }

    #[test]
    fn body_and_markup() {
        let v = vec![issue("a <b> & c", "fix it")];
        assert_eq!(body_of(&v), "- a <b> & c\n  Fix: fix it");
        assert_eq!(
            escape_markup(&body_of(&v)),
            "- a &lt;b&gt; &amp; c\n  Fix: fix it"
        );
    }

    #[test]
    fn config_defaults_and_merge() {
        assert_eq!(parse_config("").unwrap(), Config::default());
        let c = parse_config("delay_secs = 3\ncheck_dns = false").unwrap();
        assert_eq!(c.delay_secs, 3);
        assert!(!c.check_dns);
        assert!(c.check_kernel);
        assert!(c.check_bluetooth);
        assert_eq!(c.dns_hosts, Config::default().dns_hosts);
        assert_eq!(
            c.auto_reset_failed_units,
            vec!["open-fprintd-resume.service"]
        );
        assert!(parse_config("delay_secs = \"x\"").is_err());
        // The auto-generated config file round-trips.
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert_eq!(parse_config(&text).unwrap(), Config::default());
    }

    #[test]
    fn history_stats_parsing() {
        let content = "2026-10-30T17:58:30Z manual issues=1: Kernel errors\n2026-10-30T19:03:07Z resume ok\n2026-10-30T20:07:36Z resume ok\n";
        let stats = parse_history_stats(content);
        assert_eq!(stats.total, 3);
        assert_eq!(stats.ok, 2);
        assert_eq!(stats.issues, 1);
        assert_eq!(
            stats.last.as_deref(),
            Some("2026-10-30T20:07:36Z resume ok")
        );
    }

    #[test]
    fn args() {
        let a = |v: &[&str]| parse_args(v.iter().map(|s| s.to_string()));
        assert_eq!(
            a(&[]).unwrap(),
            Args {
                mode: CommandMode::Daemon
            }
        );
        assert_eq!(
            a(&["--now", "--since", "-2h"]).unwrap(),
            Args {
                mode: CommandMode::Now {
                    since: Some("-2h".into())
                }
            }
        );
        assert_eq!(
            a(&["--since=@5", "--now"]).unwrap(),
            Args {
                mode: CommandMode::Now {
                    since: Some("@5".into())
                }
            }
        );
        assert_eq!(
            a(&["status"]).unwrap(),
            Args {
                mode: CommandMode::Status { json: false }
            }
        );
        assert_eq!(
            a(&["status", "--json"]).unwrap(),
            Args {
                mode: CommandMode::Status { json: true }
            }
        );
        assert_eq!(
            a(&["--status"]).unwrap(),
            Args {
                mode: CommandMode::Status { json: false }
            }
        );
        assert_eq!(
            a(&["history"]).unwrap(),
            Args {
                mode: CommandMode::History { limit: 15 }
            }
        );
        assert_eq!(
            a(&["history", "--limit", "5"]).unwrap(),
            Args {
                mode: CommandMode::History { limit: 5 }
            }
        );
        assert_eq!(
            a(&["check-config"]).unwrap(),
            Args {
                mode: CommandMode::CheckConfig
            }
        );
        assert!(a(&["--since"]).is_err());
        assert!(a(&["--bogus"]).is_err());
        assert!(a(&["status", "--now"]).is_err());
    }

    #[tokio::test]
    async fn run_times_out_and_reports_missing() {
        let (ok, out) = run("sh", &["-c", "echo hi; echo err >&2"], 5)
            .await
            .unwrap();
        assert!(ok && out.contains("hi") && out.contains("err"));
        let e = run("sleep", &["5"], 0).await.unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        let e = run("definitely-not-a-command-xyz", &[], 1)
            .await
            .unwrap_err();
        assert!(not_found(&e), "{e}");
    }
}
