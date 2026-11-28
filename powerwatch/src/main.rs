//! powerwatch: switch to a low-power configuration when UPower reports OnBattery.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;

const DEBOUNCE: Duration = Duration::from_secs(5);
const RESTORE_RETRY: Duration = Duration::from_secs(30);
const FLAP_WINDOW: Duration = Duration::from_secs(5 * 60);
const FLAP_ALERT_COOLDOWN: Duration = Duration::from_secs(30 * 60);
const FLAP_THRESHOLD: usize = 4;
const LOW_BATTERY_HYSTERESIS: u8 = 5;
const TRANSITION_HISTORY: usize = 10;

// ---------- logging ----------

macro_rules! log {
    ($lvl:expr, $($a:tt)*) => { eprintln!("<{}>{}", $lvl, format!($($a)*)) };
}
// syslog-style priority prefixes, understood by journald on stderr.
macro_rules! info { ($($a:tt)*) => { log!(6, $($a)*) } }
macro_rules! warn { ($($a:tt)*) => { log!(4, $($a)*) } }
macro_rules! err { ($($a:tt)*) => { log!(3, $($a)*) } }

// ---------- config & state ----------

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
struct Config {
    containers: Vec<String>,
    battery_profile: String,
    wifi_powersave: bool,
    notify: bool,
    on_battery_cmds: Vec<String>,
    on_ac_cmds: Vec<String>,
    low_battery_threshold: u8,
    on_low_battery_cmds: Vec<String>,
    power_budget_watts: f64,
    heavy_containers: Vec<String>,
    on_high_power_cmds: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            containers: vec!["prefect-server".into(), "prefect-postgres".into()],
            battery_profile: "balanced".into(),
            wifi_powersave: false,
            notify: true,
            on_battery_cmds: vec![],
            on_ac_cmds: vec![],
            low_battery_threshold: 15,
            on_low_battery_cmds: vec![],
            power_budget_watts: 0.0,
            heavy_containers: vec![],
            on_high_power_cmds: vec![],
        }
    }
}

#[derive(Deserialize, Serialize, Default, Debug, PartialEq, Clone)]
struct PowerTransition {
    at_unix_seconds: u64,
    on_battery: bool,
}

#[derive(Deserialize, Serialize, Default, Debug, PartialEq, Clone)]
#[serde(default)]
struct State {
    /// Last state we applied (None = never).
    on_battery: Option<bool>,
    /// Profile to restore on AC; Some only if we changed the profile.
    prev_profile: Option<String>,
    /// Containers we stopped, in stop order.
    stopped: Vec<String>,
    /// Wi-Fi connection whose powersave we changed, and its original value.
    wifi_con: Option<String>,
    wifi_prev: Option<String>,
    /// Prevent repeatedly running low-battery actions until the battery recovers.
    low_battery_alerted: bool,
    /// Last stable power transitions, for `powerwatch status`.
    recent_transitions: Vec<PowerTransition>,
    /// Whether high power draw shedding is currently active.
    high_power_shed: bool,
    /// Heavy containers stopped due to exceeding power budget.
    heavy_stopped: Vec<String>,
}

impl State {
    /// True if there is something we changed that still needs restoring.
    fn pending(&self) -> bool {
        self.prev_profile.is_some()
            || !self.stopped.is_empty()
            || self.wifi_con.is_some()
            || !self.heavy_stopped.is_empty()
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

fn config_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("powerwatch/config.toml")
}

fn state_path() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("powerwatch/state.toml")
}

#[derive(Default, Debug, Clone, PartialEq)]
struct BatteryMetrics {
    capacity: Option<u8>,
    status: Option<String>,
    power_watts: Option<f64>,
    energy_now_wh: Option<f64>,
    energy_full_wh: Option<f64>,
    energy_full_design_wh: Option<f64>,
    cycle_count: Option<u32>,
    threshold_start: Option<u8>,
    threshold_stop: Option<u8>,
}

fn read_battery_metrics() -> BatteryMetrics {
    let mut m = BatteryMetrics::default();
    let supplies = match std::fs::read_dir("/sys/class/power_supply") {
        Ok(s) => s,
        Err(_) => return m,
    };
    for supply in supplies.flatten() {
        let path = supply.path();
        let Ok(kind) = std::fs::read_to_string(path.join("type")) else {
            continue;
        };
        if kind.trim() != "Battery" {
            continue;
        }

        if let Ok(s) = std::fs::read_to_string(path.join("capacity"))
            && let Ok(v) = s.trim().parse::<u8>()
            && v <= 100
        {
            m.capacity = Some(v);
        }
        if let Ok(s) = std::fs::read_to_string(path.join("status")) {
            m.status = Some(s.trim().to_string());
        }
        if let Ok(s) = std::fs::read_to_string(path.join("power_now"))
            && let Ok(uw) = s.trim().parse::<f64>()
        {
            m.power_watts = Some(uw / 1_000_000.0);
        } else if let (Ok(curr), Ok(volt)) = (
            std::fs::read_to_string(path.join("current_now")),
            std::fs::read_to_string(path.join("voltage_now")),
        ) && let (Ok(ua), Ok(uv)) =
            (curr.trim().parse::<f64>(), volt.trim().parse::<f64>())
        {
            m.power_watts = Some((ua * uv) / 1_000_000_000_000.0);
        }
        if let Ok(s) = std::fs::read_to_string(path.join("energy_now"))
            && let Ok(uwh) = s.trim().parse::<f64>()
        {
            m.energy_now_wh = Some(uwh / 1_000_000.0);
        }
        if let Ok(s) = std::fs::read_to_string(path.join("energy_full"))
            && let Ok(uwh) = s.trim().parse::<f64>()
        {
            m.energy_full_wh = Some(uwh / 1_000_000.0);
        }
        if let Ok(s) = std::fs::read_to_string(path.join("energy_full_design"))
            && let Ok(uwh) = s.trim().parse::<f64>()
        {
            m.energy_full_design_wh = Some(uwh / 1_000_000.0);
        }
        if let Ok(s) = std::fs::read_to_string(path.join("cycle_count"))
            && let Ok(c) = s.trim().parse::<u32>()
        {
            m.cycle_count = Some(c);
        }
        let start_th = std::fs::read_to_string(path.join("charge_control_start_threshold"))
            .or_else(|_| std::fs::read_to_string(path.join("charge_start_threshold")));
        if let Ok(s) = start_th
            && let Ok(v) = s.trim().parse::<u8>()
        {
            m.threshold_start = Some(v);
        }
        let stop_th = std::fs::read_to_string(path.join("charge_control_end_threshold"))
            .or_else(|_| std::fs::read_to_string(path.join("charge_stop_threshold")));
        if let Ok(s) = stop_th
            && let Ok(v) = s.trim().parse::<u8>()
        {
            m.threshold_stop = Some(v);
        }
        break;
    }
    m
}

fn read_battery_capacity() -> Result<Option<u8>, String> {
    Ok(read_battery_metrics().capacity)
}

fn read_cpu_power_status() -> Option<String> {
    let cpufreq = Path::new("/sys/devices/system/cpu/cpu0/cpufreq");
    if !cpufreq.exists() {
        return None;
    }
    let mut parts = Vec::new();
    if let Ok(gov) = std::fs::read_to_string(cpufreq.join("scaling_governor")) {
        let gov = gov.trim();
        if !gov.is_empty() {
            parts.push(format!("governor: {gov}"));
        }
    }
    if let Ok(epp) = std::fs::read_to_string(cpufreq.join("energy_performance_preference")) {
        let epp = epp.trim();
        if !epp.is_empty() {
            parts.push(format!("EPP: {epp}"));
        }
    }
    if let Ok(raw_freq) = std::fs::read_to_string(cpufreq.join("scaling_cur_freq"))
        && let Ok(khz) = raw_freq.trim().parse::<u64>()
    {
        parts.push(format!("freq: {} MHz", khz / 1000));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn read_rapl_power_limits() -> Option<(f64, Option<f64>)> {
    let rapl = Path::new("/sys/class/powercap/intel-rapl:0");
    if !rapl.exists() {
        return None;
    }
    let pl1 = std::fs::read_to_string(rapl.join("constraint_0_power_limit_uw"))
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|uw| uw / 1_000_000.0)?;
    let pl2 = std::fs::read_to_string(rapl.join("constraint_1_power_limit_uw"))
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|uw| uw / 1_000_000.0);
    Some((pl1, pl2))
}

async fn detect_idle_inhibition() -> Option<String> {
    let conn = zbus::Connection::session().await.ok()?;
    let call = conn.call_method(
        Some("org.gnome.SessionManager"),
        "/org/gnome/SessionManager",
        Some("org.gnome.SessionManager"),
        "IsInhibited",
        &(8u32), // INHIBIT_FLAG_IDLE = 8
    );
    match tokio::time::timeout(Duration::from_millis(500), call).await {
        Ok(Ok(reply)) => {
            if let Ok(inhibited) = reply.body().deserialize::<bool>() {
                if inhibited {
                    Some("Inhibited (Caffeine or active media keeping screen awake)".into())
                } else {
                    Some("Normal (screen sleep timeout active)".into())
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

const DEFAULT_CONFIG: &str = r#"# powerwatch configuration
# Containers to stop on battery (restarted on AC only if powerwatch stopped them).
containers = ["prefect-server", "prefect-postgres"]
# Power profile used on battery (power-saver / balanced / performance).
# Using 'balanced' keeps the CPU responsive and avoids heavy 400-800MHz throttling.
battery_profile = "balanced"
# Enable Wi-Fi powersave on the active Wi-Fi connection while on battery
# (applies on the next reconnect; the connection is never bounced).
wifi_powersave = false
# Desktop notifications.
notify = true
# Extra shell commands (run with `sh -c`).
on_battery_cmds = []
on_ac_cmds = []
# Low-battery warning/actions threshold; 0 disables this feature.
low_battery_threshold = 15
on_low_battery_cmds = []
# Maximum power budget in Watts on battery (0 disables dynamic shedding).
# If power draw exceeds this budget, heavy_containers are shed and on_high_power_cmds run.
power_budget_watts = 0.0
heavy_containers = []
on_high_power_cmds = []
"#;

fn parse_config(s: &str) -> Result<Config, String> {
    toml::from_str(s).map_err(|e| e.to_string())
}

fn validate_config(cfg: &Config) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    if !is_known_profile(&cfg.battery_profile) {
        errors.push(format!(
            "battery_profile must be power-saver, balanced, or performance (got {:?})",
            cfg.battery_profile
        ));
    }
    if cfg.low_battery_threshold > 100 {
        errors.push("low_battery_threshold must be between 0 and 100".into());
    }
    if cfg.power_budget_watts < 0.0 {
        errors.push("power_budget_watts must be non-negative".into());
    }
    let mut seen = std::collections::HashSet::new();
    for container in &cfg.containers {
        if container.trim().is_empty() {
            errors.push("containers must not contain empty names".into());
        } else if !seen.insert(container) {
            errors.push(format!("duplicate container name: {container}"));
        }
    }
    for container in &cfg.heavy_containers {
        if container.trim().is_empty() {
            errors.push("heavy_containers must not contain empty names".into());
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check_config_file() -> Result<bool, String> {
    let path = config_path();
    let cfg = match std::fs::read_to_string(&path) {
        Ok(text) => parse_config(&text).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    validate_config(&cfg)
        .map_err(|errors| format!("{}:\n{}", path.display(), errors.join("\n")))?;
    Ok(path.exists())
}

fn age_label(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

fn load_config(dry: bool) -> Config {
    let p = config_path();
    match std::fs::read_to_string(&p) {
        Ok(s) => parse_config(&s).unwrap_or_else(|e| {
            err!("bad config {}: {e}; using defaults", p.display());
            Config::default()
        }),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            err!("cannot read config {}: {e}; using defaults", p.display());
            Config::default()
        }
        Err(_) => {
            if dry {
                info!("[dry-run] would create default config at {}", p.display());
            } else if let Err(e) = p
                .parent()
                .map(std::fs::create_dir_all)
                .transpose()
                .and_then(|_| std::fs::write(&p, DEFAULT_CONFIG))
            {
                warn!("cannot create {}: {e}", p.display());
            } else {
                info!("created default config at {}", p.display());
            }
            Config::default()
        }
    }
}

#[derive(Default)]
struct FlapDetector {
    changes: VecDeque<Instant>,
    last_alert: Option<Instant>,
}

impl FlapDetector {
    fn observe(&mut self, now: Instant) -> bool {
        while self
            .changes
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > FLAP_WINDOW)
        {
            self.changes.pop_front();
        }
        self.changes.push_back(now);
        let cooled_down = self
            .last_alert
            .is_none_or(|at| now.saturating_duration_since(at) >= FLAP_ALERT_COOLDOWN);
        if self.changes.len() >= FLAP_THRESHOLD && cooled_down {
            self.last_alert = Some(now);
            true
        } else {
            false
        }
    }
}

/// Load state. A corrupt file is moved aside (never silently discarded) so the
/// operator can recover the list of stopped containers from it.
fn load_state_from(p: &Path, dry: bool) -> State {
    let s = match std::fs::read_to_string(p) {
        Ok(s) => s,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                err!("cannot read state {}: {e}", p.display());
            }
            return State::default();
        }
    };
    match toml::from_str(&s) {
        Ok(st) => st,
        Err(e) => {
            let bad = p.with_extension("corrupt");
            err!(
                "corrupt state {}: {e}; keeping a copy at {} and starting fresh",
                p.display(),
                bad.display()
            );
            if !dry {
                let _ = std::fs::rename(p, &bad);
            }
            State::default()
        }
    }
}

fn save_state_to(p: &Path, st: &State) {
    let res = (|| -> std::io::Result<()> {
        use std::io::Write;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(
            toml::to_string(st)
                .map_err(std::io::Error::other)?
                .as_bytes(),
        )?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, p)
    })();
    if let Err(e) = res {
        err!("cannot save state {}: {e}", p.display());
    }
}

// ---------- pure helpers ----------

/// Split an `nmcli -t` line on unescaped ':' and unescape `\:` and `\\`.
fn split_terse(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut it = line.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                if let Some(n) = it.next() {
                    fields.last_mut().unwrap().push(n);
                }
            }
            ':' => fields.push(String::new()),
            c => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

/// First active Wi-Fi connection from `nmcli -t -f NAME,TYPE,DEVICE connection show --active`.
fn parse_active_wifi(out: &str) -> Option<(String, String)> {
    out.lines().find_map(|l| {
        let f = split_terse(l);
        (f.len() == 3 && f[1] == "802-11-wireless").then(|| (f[0].clone(), f[2].clone()))
    })
}

/// Normalise an nmcli powersave value ("enable", "3", "0 (default)", ...) to its number.
fn parse_powersave(s: &str) -> Option<&'static str> {
    let t = s.trim();
    let w = t.split_whitespace().next().unwrap_or("");
    match w {
        "0" | "default" => Some("0"),
        "1" | "ignore" => Some("1"),
        "2" | "disable" => Some("2"),
        "3" | "enable" => Some("3"),
        _ => None,
    }
}

fn is_known_profile(p: &str) -> bool {
    ["power-saver", "balanced", "performance"].contains(&p)
}

/// Profile to remember for restore. tuned-ppd may report "unknown"; use balanced then.
fn profile_to_remember(cur: &str) -> String {
    if is_known_profile(cur) {
        cur.to_string()
    } else {
        "balanced".to_string()
    }
}

/// Profile to restore on AC (never fails on empty/unknown saved values).
fn profile_to_restore(prev: &str) -> String {
    profile_to_remember(prev)
}

/// Whether to switch to the battery profile. When the backend reports an unreadable
/// profile ("unknown") and we have already switched on a previous run, don't redo it.
fn should_set_profile(cur: &str, target: &str, st: &State) -> bool {
    if cur == target {
        return false;
    }
    let already_ours =
        st.prev_profile.is_some() && st.on_battery == Some(true) && !is_known_profile(cur);
    !already_ours
}

fn is_missing_connection(e: &str) -> bool {
    let e = e.to_lowercase();
    e.contains("unknown connection") || e.contains("not found") || e.contains("does not exist")
}

// ---------- system access (mockable) ----------

trait Sys {
    /// Run a command, return trimmed stdout on success or an error string.
    async fn run(&self, prog: &str, args: &[&str]) -> Result<String, String>;
    fn battery_capacity(&self) -> Result<Option<u8>, String>;
    async fn get_profile(&self) -> Result<String, String>;
    async fn set_profile(&self, p: &str) -> Result<(), String>;
    async fn notify(&self, title: &str, body: &str);
}

struct RealSys {
    session: Mutex<Option<zbus::Connection>>,
}

impl Sys for RealSys {
    async fn run(&self, prog: &str, args: &[&str]) -> Result<String, String> {
        let fut = Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(Duration::from_secs(120), fut)
            .await
            .map_err(|_| format!("{prog} timed out"))?
            .map_err(|e| format!("{prog}: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(format!(
                "{prog} {} failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn battery_capacity(&self) -> Result<Option<u8>, String> {
        read_battery_capacity()
    }

    async fn get_profile(&self) -> Result<String, String> {
        let conn = zbus::Connection::system()
            .await
            .map_err(|e| e.to_string())?;
        let pp = PowerProfilesProxy::new(&conn)
            .await
            .map_err(|e| e.to_string())?;
        pp.active_profile().await.map_err(|e| e.to_string())
    }

    async fn set_profile(&self, p: &str) -> Result<(), String> {
        let conn = zbus::Connection::system()
            .await
            .map_err(|e| e.to_string())?;
        let pp = PowerProfilesProxy::new(&conn)
            .await
            .map_err(|e| e.to_string())?;
        pp.set_active_profile(p)
            .await
            .map_err(|e| format!("set profile {p}: {e}"))
    }

    async fn notify(&self, title: &str, body: &str) {
        let cached = self.session.lock().unwrap().clone();
        let conn = match cached {
            Some(c) => c,
            None => match zbus::Connection::session().await {
                Ok(c) => {
                    *self.session.lock().unwrap() = Some(c.clone());
                    c
                }
                Err(_) => {
                    warn!("no session bus; notification skipped");
                    return;
                }
            },
        };
        let hints: std::collections::HashMap<&str, zbus::Variant> = Default::default();
        let args = (
            "powerwatch",
            0u32,
            "battery",
            title,
            body,
            Vec::<&str>::new(),
            hints,
            8000i32,
        );
        let call = conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &args,
        );
        match tokio::time::timeout(Duration::from_secs(10), call).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!("notification failed: {e}");
                *self.session.lock().unwrap() = None;
            }
            Err(_) => {
                warn!("notification timed out");
                *self.session.lock().unwrap() = None;
            }
        }
    }
}

// ---------- actions ----------

struct Ctx<S: Sys> {
    sys: S,
    cfg: Config,
    dry: bool,
    st: State,
    state_file: PathBuf,
}

impl<S: Sys> Ctx<S> {
    fn persist(&self) {
        if !self.dry {
            save_state_to(&self.state_file, &self.st);
        }
    }

    async fn extra_cmds(&self, cmds: &[String], notes: &mut Vec<String>) {
        for c in cmds {
            if self.dry {
                info!("[dry-run] would run: sh -c {c:?}");
                continue;
            }
            match self.sys.run("sh", &["-c", c]).await {
                Ok(_) => info!("ran extra command: {c}"),
                Err(e) => {
                    err!("extra command failed: {e}");
                    notes.push(format!("command failed: {c}"));
                }
            }
        }
    }

    fn record_transition(&mut self, on_battery: bool) {
        if self.st.on_battery == Some(on_battery) {
            return;
        }
        let at_unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.st.recent_transitions.push(PowerTransition {
            at_unix_seconds,
            on_battery,
        });
        if self.st.recent_transitions.len() > TRANSITION_HISTORY {
            self.st.recent_transitions.remove(0);
        }
        self.st.on_battery = Some(on_battery);
        self.persist();
    }

    async fn check_low_battery(&mut self, on_battery: bool) {
        if !on_battery {
            if self.st.low_battery_alerted {
                self.st.low_battery_alerted = false;
                self.persist();
            }
            return;
        }
        let threshold = self.cfg.low_battery_threshold;
        if threshold == 0 {
            return;
        }
        match self.sys.battery_capacity() {
            Ok(Some(capacity)) if capacity <= threshold && !self.st.low_battery_alerted => {
                self.st.low_battery_alerted = true;
                self.persist();
                let message = format!("Battery is at {capacity}% (threshold: {threshold}%).");
                warn!("{message}");
                let cmds = self.cfg.on_low_battery_cmds.clone();
                let mut notes = vec![message];
                if let Some(idle) = detect_idle_inhibition().await
                    && idle.contains("Inhibited")
                {
                    notes.push("Caffeine is currently keeping the display awake.".into());
                }
                self.extra_cmds(&cmds, &mut notes).await;
                self.notify("Low battery", &notes).await;
            }
            Ok(Some(capacity))
                if capacity >= threshold.saturating_add(LOW_BATTERY_HYSTERESIS)
                    && self.st.low_battery_alerted =>
            {
                self.st.low_battery_alerted = false;
                self.persist();
                info!("battery recovered to {capacity}%; low-battery actions re-armed");
            }
            Ok(_) => {}
            Err(e) => warn!("read battery capacity: {e}"),
        }
    }

    async fn check_power_budget(&mut self, on_battery: bool) {
        if !on_battery || self.cfg.power_budget_watts <= 0.0 {
            if self.st.high_power_shed {
                let mut notes = Vec::new();
                self.restore_heavy_workloads(&mut notes).await;
            }
            return;
        }
        let metrics = read_battery_metrics();
        let Some(watts) = metrics.power_watts else {
            return;
        };
        let budget = self.cfg.power_budget_watts;
        let hysteresis = 2.0;

        if watts > budget && !self.st.high_power_shed {
            self.st.high_power_shed = true;
            self.persist();
            let msg = format!(
                "Power draw ({watts:.1} W) exceeded budget ({budget:.1} W). Shedding heavy workloads."
            );
            warn!("{msg}");
            let mut notes = vec![msg];
            let heavy = self.cfg.heavy_containers.clone();
            for c in heavy {
                if !self.st.heavy_stopped.contains(&c)
                    && !self.st.stopped.contains(&c)
                    && let Ok(true) = self.container_running(&c).await
                {
                    if !self.dry {
                        self.st.heavy_stopped.push(c.clone());
                        self.persist();
                        if self.sys.run("docker", &["stop", &c]).await.is_ok() {
                            info!("shed heavy container {c}");
                            notes.push(format!("shed: {c}"));
                        }
                    } else {
                        notes.push(format!("[dry-run] shed: {c}"));
                    }
                }
            }
            let cmds = self.cfg.on_high_power_cmds.clone();
            self.extra_cmds(&cmds, &mut notes).await;
            self.notify("Power budget exceeded", &notes).await;
        } else if watts <= (budget - hysteresis).max(0.0) && self.st.high_power_shed {
            info!(
                "power draw recovered to {watts:.1} W (budget {budget:.1} W); restoring heavy workloads"
            );
            let mut notes = Vec::new();
            self.restore_heavy_workloads(&mut notes).await;
            if !notes.is_empty() {
                self.notify("Power budget restored", &notes).await;
            }
        }
    }

    async fn restore_heavy_workloads(&mut self, notes: &mut Vec<String>) {
        if !self.st.high_power_shed && self.st.heavy_stopped.is_empty() {
            return;
        }
        self.st.high_power_shed = false;
        let heavy = std::mem::take(&mut self.st.heavy_stopped);
        self.persist();
        for c in heavy {
            if !self.dry {
                if self.sys.run("docker", &["start", &c]).await.is_ok() {
                    info!("restored heavy container {c}");
                    notes.push(format!("restored heavy container: {c}"));
                }
            } else {
                notes.push(format!("[dry-run] restore heavy container: {c}"));
            }
        }
    }

    async fn container_running(&self, c: &str) -> Result<bool, String> {
        self.sys
            .run("docker", &["inspect", "-f", "{{.State.Running}}", c])
            .await
            .map(|r| r == "true")
    }

    async fn on_battery(&mut self) {
        let mut notes = Vec::new();
        // Power profile.
        match self.sys.get_profile().await {
            Ok(cur) => {
                let target = self.cfg.battery_profile.clone();
                if should_set_profile(&cur, &target, &self.st) {
                    let first = self.st.prev_profile.is_none();
                    if first {
                        self.st.prev_profile = Some(profile_to_remember(&cur));
                        self.persist();
                    }
                    if self.dry {
                        info!("[dry-run] would set ActiveProfile={target}");
                        if first {
                            self.st.prev_profile = None; // dry-run must not leave state behind
                        }
                    } else {
                        match self.sys.set_profile(&target).await {
                            Ok(()) => {
                                info!("profile {cur} -> {target}");
                                notes.push(format!("profile: {target}"));
                            }
                            Err(e) => {
                                err!("set profile: {e}");
                                if first {
                                    // Nothing changed, so nothing to restore.
                                    self.st.prev_profile = None;
                                    self.persist();
                                }
                            }
                        }
                    }
                } else {
                    info!("profile {cur}: no change needed (target {target})");
                }
            }
            Err(e) => err!("get profile: {e}"),
        }
        // Containers.
        let names = self.cfg.containers.clone();
        for c in &names {
            if self.st.stopped.contains(c) {
                continue;
            }
            match self.container_running(c).await {
                Ok(true) => {
                    if self.dry {
                        info!("[dry-run] would run: docker stop {c}");
                        notes.push(format!("stopped: {c}"));
                        continue;
                    }
                    // Record before stopping so a crash mid-way can't lose track.
                    self.st.stopped.push(c.clone());
                    self.persist();
                    match self.sys.run("docker", &["stop", c]).await {
                        Ok(_) => {
                            info!("stopped container {c}");
                            notes.push(format!("stopped: {c}"));
                        }
                        Err(e) => {
                            err!("{e}");
                            // If it did stop (e.g. timeout after the fact), keep tracking it.
                            if !matches!(self.container_running(c).await, Ok(false)) {
                                self.st.stopped.retain(|x| x != c);
                                self.persist();
                            }
                        }
                    }
                }
                Ok(false) => info!("container {c} not running; leaving it"),
                Err(e) => warn!("container {c}: {e}"),
            }
        }
        // Wi-Fi.
        if self.cfg.wifi_powersave {
            match self.wifi_enable().await {
                Ok(Some(con)) => {
                    notes.push(format!("Wi-Fi powersave on ({con}, applies on reconnect)"))
                }
                Ok(None) => {}
                Err(e) => warn!("wifi powersave skipped: {e}"),
            }
        }
        let cmds = self.cfg.on_battery_cmds.clone();
        self.extra_cmds(&cmds, &mut notes).await;
        self.st.on_battery = Some(true);
        self.persist();
        self.notify("On battery", &notes).await;
    }

    async fn restore_ac_changes(&mut self, notes: &mut Vec<String>) {
        if let Some(prev) = self.st.prev_profile.clone() {
            let prev = profile_to_restore(&prev);
            if self.dry {
                info!("[dry-run] would set ActiveProfile={prev}");
            } else {
                match self.sys.set_profile(&prev).await {
                    Ok(()) => {
                        info!("profile restored to {prev}");
                        notes.push(format!("profile: {prev}"));
                        self.st.prev_profile = None;
                        self.persist();
                    }
                    Err(e) => err!("restore profile: {e}"),
                }
            }
        }
        // Restart in reverse stop order (dependencies such as the DB stopped last).
        let list: Vec<String> = self.st.stopped.iter().rev().cloned().collect();
        for c in list {
            if self.dry {
                info!("[dry-run] would run: docker start {c}");
                continue;
            }
            match self.sys.run("docker", &["start", &c]).await {
                Ok(_) => {
                    info!("started container {c}");
                    notes.push(format!("started: {c}"));
                    self.st.stopped.retain(|x| x != &c);
                    self.persist();
                }
                Err(e) => err!("{e}"),
            }
        }
        match self.wifi_restore().await {
            Ok(Some(con)) => notes.push(format!("Wi-Fi powersave restored ({con})")),
            Ok(None) => {}
            Err(e) => warn!("wifi restore: {e}"),
        }
        self.restore_heavy_workloads(notes).await;
        if self.st.pending() {
            warn!("not everything could be restored; will retry while AC remains connected");
        }
    }

    async fn on_ac(&mut self) {
        let mut notes = Vec::new();
        self.restore_ac_changes(&mut notes).await;
        let cmds = self.cfg.on_ac_cmds.clone();
        self.extra_cmds(&cmds, &mut notes).await;
        self.st.on_battery = Some(false);
        self.persist();
        self.notify("On AC power", &notes).await;
    }

    async fn wifi_power_save_of(&self, con: &str) -> Result<&'static str, String> {
        let raw = self
            .sys
            .run(
                "nmcli",
                &[
                    "-g",
                    "802-11-wireless.powersave",
                    "connection",
                    "show",
                    "id",
                    con,
                ],
            )
            .await?;
        Ok(parse_powersave(&raw).unwrap_or("0"))
    }

    /// Enable Wi-Fi powersave on the active Wi-Fi connection. Returns the connection if we
    /// changed it. The setting takes effect on the next (re)connect; the link is never bounced
    /// because NetworkManager cannot reapply this property live.
    async fn wifi_enable(&mut self) -> Result<Option<String>, String> {
        let out = self
            .sys
            .run(
                "nmcli",
                &[
                    "-t",
                    "-f",
                    "NAME,TYPE,DEVICE",
                    "connection",
                    "show",
                    "--active",
                ],
            )
            .await?;
        let Some((con, _dev)) = parse_active_wifi(&out) else {
            info!("no active Wi-Fi connection");
            return Ok(None);
        };
        // Roamed to a different connection: put the old one back first so nothing is lost.
        if self.st.wifi_con.as_deref().is_some_and(|old| old != con)
            && let Err(e) = self.wifi_restore().await
        {
            warn!("wifi restore of previous connection: {e}");
        }
        let cur = self.wifi_power_save_of(&con).await?;
        if cur == "3" {
            info!("wifi powersave already enabled on {con}");
            return Ok(None);
        }
        if self.dry {
            info!(
                "[dry-run] would run: nmcli connection modify id {con:?} 802-11-wireless.powersave 3"
            );
            return Ok(Some(con));
        }
        if self.st.wifi_con.as_deref() != Some(con.as_str()) {
            self.st.wifi_con = Some(con.clone());
            self.st.wifi_prev = Some(cur.to_string());
            self.persist();
        }
        if let Err(e) = self
            .sys
            .run(
                "nmcli",
                &[
                    "connection",
                    "modify",
                    "id",
                    &con,
                    "802-11-wireless.powersave",
                    "3",
                ],
            )
            .await
        {
            if self.st.wifi_prev.as_deref() == Some(cur) && cur != "3" {
                // Nothing was changed.
                self.st.wifi_con = None;
                self.st.wifi_prev = None;
                self.persist();
            }
            return Err(e);
        }
        info!("wifi powersave 3 on {con} (applies on reconnect)");
        Ok(Some(con))
    }

    /// Restore the powersave value we changed, unless someone else has changed it since.
    async fn wifi_restore(&mut self) -> Result<Option<String>, String> {
        let Some(con) = self.st.wifi_con.clone() else {
            return Ok(None);
        };
        let value = self
            .st
            .wifi_prev
            .as_deref()
            .and_then(parse_powersave)
            .unwrap_or("0");
        if self.dry {
            info!(
                "[dry-run] would run: nmcli connection modify id {con:?} 802-11-wireless.powersave {value}"
            );
            return Ok(Some(con));
        }
        let changed = match self.wifi_power_save_of(&con).await {
            Ok("3") => true,
            Ok(other) => {
                info!("wifi powersave on {con} is {other}, not our value; leaving it");
                false
            }
            Err(e) if is_missing_connection(&e) => {
                info!("wifi connection {con} is gone; nothing to restore");
                false
            }
            Err(e) => return Err(e),
        };
        if changed {
            self.sys
                .run(
                    "nmcli",
                    &[
                        "connection",
                        "modify",
                        "id",
                        &con,
                        "802-11-wireless.powersave",
                        value,
                    ],
                )
                .await?;
            info!("wifi powersave {value} restored on {con}");
        }
        self.st.wifi_con = None;
        self.st.wifi_prev = None;
        self.persist();
        Ok(changed.then_some(con))
    }

    async fn notify(&self, title: &str, notes: &[String]) {
        if !self.cfg.notify || notes.is_empty() {
            return;
        }
        let body = notes.join("\n");
        if self.dry {
            info!(
                "[dry-run] would notify: {title}: {}",
                body.replace('\n', "; ")
            );
            return;
        }
        self.sys.notify(title, &body).await;
    }

    async fn apply(&mut self, on_battery: bool) {
        info!(
            "applying state: {}",
            if on_battery { "battery" } else { "AC" }
        );
        self.record_transition(on_battery);
        if on_battery {
            self.on_battery().await
        } else {
            self.on_ac().await
        }
        self.check_low_battery(on_battery).await;
        self.check_power_budget(on_battery).await;
    }
}

// ---------- D-Bus ----------

#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
trait UPower {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "net.hadess.PowerProfiles",
    default_service = "net.hadess.PowerProfiles",
    default_path = "/net/hadess/PowerProfiles"
)]
trait PowerProfiles {
    #[zbus(property)]
    fn active_profile(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_active_profile(&self, p: &str) -> zbus::Result<()>;
}

/// Returns (cached proxy for the change stream, uncached proxy for polling UPower directly).
async fn connect() -> zbus::Result<(UPowerProxy<'static>, UPowerProxy<'static>)> {
    let conn = zbus::Connection::system().await?;
    let p = UPowerProxy::new(&conn).await?;
    let poll = UPowerProxy::builder(&conn)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    poll.on_battery().await?;
    Ok((p, poll))
}

#[derive(Serialize)]
struct PowerwatchStatusJson<'a> {
    on_battery: bool,
    power_state: &'static str,
    battery_pct: Option<u8>,
    battery_status: Option<String>,
    power_watts: Option<f64>,
    health_pct: Option<u32>,
    charge_threshold_start: Option<u8>,
    charge_threshold_stop: Option<u8>,
    power_profile: &'a str,
    cpu_status: Option<String>,
    rapl_pl1_w: Option<f64>,
    rapl_pl2_w: Option<f64>,
    screen_sleep: Option<String>,
    pending_restores: bool,
}

async fn print_status(json: bool) -> Result<(), String> {
    let (proxy, _) = connect()
        .await
        .map_err(|e| format!("cannot reach UPower: {e}"))?;
    let on_battery = proxy
        .on_battery()
        .await
        .map_err(|e| format!("read UPower state: {e}"))?;
    let profile = RealSys {
        session: Mutex::new(None),
    }
    .get_profile()
    .await
    .map_err(|e| format!("read power profile: {e}"))?;
    let state = load_state_from(&state_path(), true);
    let metrics = read_battery_metrics();
    let rapl = read_rapl_power_limits();
    let cpu_info = read_cpu_power_status();
    let idle = detect_idle_inhibition().await;

    if json {
        let health_pct = if let (Some(full), Some(design)) =
            (metrics.energy_full_wh, metrics.energy_full_design_wh)
        {
            Some(((full / design) * 100.0).round() as u32)
        } else {
            None
        };
        let status_json = PowerwatchStatusJson {
            on_battery,
            power_state: if on_battery {
                "on battery"
            } else {
                "AC connected"
            },
            battery_pct: metrics.capacity,
            battery_status: metrics.status.clone(),
            power_watts: metrics.power_watts,
            health_pct,
            charge_threshold_start: metrics.threshold_start,
            charge_threshold_stop: metrics.threshold_stop,
            power_profile: &profile,
            cpu_status: cpu_info,
            rapl_pl1_w: rapl.map(|(p1, _)| p1),
            rapl_pl2_w: rapl.and_then(|(_, p2)| p2),
            screen_sleep: idle,
            pending_restores: state.pending(),
        };
        if let Ok(j) = serde_json::to_string_pretty(&status_json) {
            println!("{j}");
        }
        return Ok(());
    }

    println!(
        "Power state: {}",
        if on_battery {
            "on battery"
        } else {
            "AC connected"
        }
    );
    if let Some(cap) = metrics.capacity {
        let st = metrics.status.as_deref().unwrap_or("Unknown");
        println!("Battery: {cap}% ({st})");
    } else {
        println!("Battery: unavailable");
    }
    if let Some(p_w) = metrics.power_watts {
        let is_discharging = metrics.status.as_deref() == Some("Discharging");
        let is_charging = metrics.status.as_deref() == Some("Charging");
        let time_est = if is_discharging
            && p_w > 0.1
            && let Some(now_wh) = metrics.energy_now_wh
        {
            let hours = now_wh / p_w;
            let h = hours as u32;
            let m = ((hours - (h as f64)) * 60.0) as u32;
            format!(", ~{h}h {m:02}m remaining")
        } else if is_charging
            && p_w > 0.1
            && let (Some(full_wh), Some(now_wh)) = (metrics.energy_full_wh, metrics.energy_now_wh)
        {
            let needed = full_wh - now_wh;
            if needed > 0.0 {
                let hours = needed / p_w;
                let h = hours as u32;
                let m = ((hours - (h as f64)) * 60.0) as u32;
                format!(", ~{h}h {m:02}m until full")
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        println!("Power draw: {:.2} W{}", p_w, time_est);
    }
    if let (Some(full), Some(design)) = (metrics.energy_full_wh, metrics.energy_full_design_wh) {
        let health_pct = ((full / design) * 100.0).round() as u32;
        let cycles = metrics
            .cycle_count
            .map(|c| format!(", {c} cycles"))
            .unwrap_or_default();
        println!(
            "Battery health: {health_pct}% of design ({:.1} Wh / {:.1} Wh design{})",
            full, design, cycles
        );
    }
    if let (Some(start), Some(stop)) = (metrics.threshold_start, metrics.threshold_stop) {
        println!("Charge thresholds: {start}% start / {stop}% stop");
    }
    println!("Power profile: {profile}");
    if let Some(cpu_info_str) = cpu_info {
        println!("CPU status: {cpu_info_str}");
    }
    if let Some((pl1, pl2)) = rapl {
        if let Some(short) = pl2 {
            println!(
                "CPU power limits: PL1 {:.1} W (sustained) / PL2 {:.1} W (burst)",
                pl1, short
            );
        } else {
            println!("CPU power limit: PL1 {:.1} W (sustained)", pl1);
        }
    }
    if let Some(idle_str) = idle {
        println!("Screen sleep: {idle_str}");
    }
    let cfg = load_config(true);
    if cfg.power_budget_watts > 0.0 {
        let shed_status = if state.high_power_shed {
            "shedding active"
        } else {
            "within budget"
        };
        println!(
            "Power budget: {:.1} W ({shed_status})",
            cfg.power_budget_watts
        );
    }
    println!(
        "Pending restores: {}",
        if state.pending() { "yes" } else { "no" }
    );
    if let Some(profile) = &state.prev_profile {
        println!("  profile: {profile}");
    }
    if !state.stopped.is_empty() {
        println!("  containers: {}", state.stopped.join(", "));
    }
    if !state.heavy_stopped.is_empty() {
        println!(
            "  heavy containers shed: {}",
            state.heavy_stopped.join(", ")
        );
    }
    if let Some(connection) = &state.wifi_con {
        println!("  Wi-Fi connection: {connection}");
    }
    println!("Recent stable power changes:");
    if state.recent_transitions.is_empty() {
        println!("  none recorded");
    } else {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        for transition in state.recent_transitions.iter().rev() {
            println!(
                "  {}: {}",
                age_label(now.saturating_sub(transition.at_unix_seconds)),
                if transition.on_battery {
                    "on battery"
                } else {
                    "AC connected"
                }
            );
        }
    }
    Ok(())
}

const POLL: Duration = Duration::from_secs(60);
const MAX_POLL_FAILURES: u32 = 3;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut dry = false;
    let mut once = false;
    let mut status = false;
    let mut check_config = false;
    let mut json = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--dry-run" => dry = true,
            "--once" => once = true,
            "status" | "--status" => status = true,
            "--json" => json = true,
            "check-config" | "--check-config" => check_config = true,
            "-h" | "--help" => {
                println!(
                    "usage: powerwatch [--dry-run] [--once] | powerwatch status [--json] | powerwatch check-config"
                );
                return;
            }
            o => {
                eprintln!(
                    "unknown argument: {o}\nusage: powerwatch [--dry-run] [--once] | powerwatch status [--json] | powerwatch check-config"
                );
                std::process::exit(2);
            }
        }
    }
    if (status && check_config) || ((status || check_config) && (dry || once)) {
        eprintln!("status/check-config cannot be combined with --dry-run or --once");
        std::process::exit(2);
    }
    if status {
        if let Err(e) = print_status(json).await {
            eprintln!("powerwatch status: {e}");
            std::process::exit(1);
        }
        return;
    }
    if check_config {
        match check_config_file() {
            Ok(true) => println!("{}: configuration is valid", config_path().display()),
            Ok(false) => println!("no config found; defaults are valid"),
            Err(e) => {
                eprintln!("powerwatch check-config: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let cfg = load_config(dry);
    if let Err(errors) = validate_config(&cfg) {
        for error in errors {
            err!("invalid configuration: {error}");
        }
        std::process::exit(2);
    }
    let state_file = state_path();
    let st = load_state_from(&state_file, dry);
    let mut ctx = Ctx {
        sys: RealSys {
            session: Mutex::new(None),
        },
        cfg,
        dry,
        st,
        state_file,
    };
    if dry {
        info!("dry-run: no changes will be made");
    }

    // Retry connecting to the system bus / UPower rather than crash.
    let (proxy, poll_proxy) = loop {
        match connect().await {
            Ok(p) => break p,
            Err(e) if once => {
                err!("cannot reach UPower: {e}");
                std::process::exit(1);
            }
            Err(e) => {
                err!("cannot reach UPower: {e}; retrying in 10s");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    };

    let mut cur = match proxy.on_battery().await {
        Ok(v) => v,
        Err(e) => {
            err!("read OnBattery: {e}");
            std::process::exit(1);
        }
    };
    info!("OnBattery = {cur}");
    // Startup: act on current state. Every step is idempotent and only touches what
    // the state file says we changed, so this is safe after a restart.
    ctx.apply(cur).await;
    if once {
        return;
    }

    let mut stream = proxy.receive_on_battery_changed().await;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    let mut applied = cur;
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut poll = tokio::time::interval(POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;
    let mut restore_retry = tokio::time::interval(RESTORE_RETRY);
    restore_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    restore_retry.tick().await;
    let mut flap_detector = FlapDetector::default();
    let mut poll_failures = 0u32;
    loop {
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            ch = stream.next() => {
                match ch {
                    Some(c) => {
                        if let Ok(v) = c.get().await
                            && v != cur
                        {
                            cur = v;
                            info!("OnBattery changed -> {v}; debouncing {}s", DEBOUNCE.as_secs());
                            deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                            if flap_detector.observe(Instant::now()) {
                                let notes = vec!["The AC/battery status has changed repeatedly. Check the charger, cable, and USB-C port.".into()];
                                warn!("power source is flapping; notification throttled for {} minutes", FLAP_ALERT_COOLDOWN.as_secs() / 60);
                                ctx.notify("Unstable charging connection", &notes).await;
                            }
                        }
                    }
                    None => {
                        err!("property stream ended");
                        std::process::exit(1); // systemd restarts us
                    }
                }
            }
            _ = poll.tick() => {
                // Safety net for missed signals (UPower restart, bus reconnect).
                match poll_proxy.on_battery().await {
                    Ok(v) => {
                        poll_failures = 0;
                        if v != cur {
                            info!("poll: OnBattery = {v}; debouncing {}s", DEBOUNCE.as_secs());
                            cur = v;
                            deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                            if flap_detector.observe(Instant::now()) {
                                let notes = vec!["The AC/battery status has changed repeatedly. Check the charger, cable, and USB-C port.".into()];
                                warn!("power source is flapping; notification throttled for {} minutes", FLAP_ALERT_COOLDOWN.as_secs() / 60);
                                ctx.notify("Unstable charging connection", &notes).await;
                            }
                        }
                    }
                    Err(e) => {
                        poll_failures += 1;
                        warn!("poll OnBattery failed ({poll_failures}/{MAX_POLL_FAILURES}): {e}");
                        if poll_failures >= MAX_POLL_FAILURES {
                            err!("UPower unreachable; exiting for restart");
                            std::process::exit(1);
                        }
                    }
                }
                if deadline.is_none() {
                    ctx.check_low_battery(cur).await;
                    ctx.check_power_budget(cur).await;
                }
            }
            _ = restore_retry.tick() => {
                if !cur && !applied && deadline.is_none() && ctx.st.pending() {
                    let mut notes = Vec::new();
                    ctx.restore_ac_changes(&mut notes).await;
                    if !notes.is_empty() {
                        info!("AC restoration retry progress: {}", notes.join("; "));
                        if !ctx.st.pending() {
                            ctx.notify("Restored on AC", &notes).await;
                        }
                    }
                }
            }
            _ = sleep => {
                deadline = None;
                if cur != applied {
                    ctx.apply(cur).await;
                    applied = cur;
                } else {
                    info!("state settled back to {}; nothing to do", if cur { "battery" } else { "AC" });
                }
            }
            _ = async { match hup.as_mut() { Some(h) => { h.recv().await; } None => std::future::pending().await } } => {
                info!("SIGHUP: reloading configuration");
                let new_cfg = load_config(ctx.dry);
                if let Err(errors) = validate_config(&new_cfg) {
                    for e in errors {
                        err!("reloaded config invalid: {e}");
                    }
                } else {
                    ctx.cfg = new_cfg;
                    info!("configuration successfully reloaded");
                    if cur {
                        ctx.check_power_budget(cur).await;
                    }
                }
            }
            _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {
                info!("SIGTERM; exiting");
                return;
            }
            _ = tokio::signal::ctrl_c() => { info!("SIGINT; exiting"); return; }
        }
    }
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    /// Scripted fake: commands are matched by their space-joined text; unmatched => Ok("").
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        replies: RefCell<HashMap<String, Result<String, String>>>,
        profile: RefCell<String>,
        battery: Cell<Option<u8>>,
        set_profile_fails: bool,
        notes: RefCell<Vec<(String, String)>>,
    }

    impl Fake {
        fn reply(&self, cmd: &str, r: Result<&str, &str>) {
            self.replies
                .borrow_mut()
                .insert(cmd.into(), r.map(Into::into).map_err(Into::into));
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
        fn has(&self, c: &str) -> bool {
            self.calls.borrow().iter().any(|x| x == c)
        }
    }

    impl Sys for &Fake {
        async fn run(&self, prog: &str, args: &[&str]) -> Result<String, String> {
            let key = format!("{prog} {}", args.join(" "));
            self.calls.borrow_mut().push(key.clone());
            self.replies
                .borrow()
                .get(&key)
                .cloned()
                .unwrap_or(Ok(String::new()))
        }
        fn battery_capacity(&self) -> Result<Option<u8>, String> {
            Ok(self.battery.get())
        }
        async fn get_profile(&self) -> Result<String, String> {
            Ok(self.profile.borrow().clone())
        }
        async fn set_profile(&self, p: &str) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("set_profile {p}"));
            if self.set_profile_fails {
                Err("boom".into())
            } else {
                Ok(())
            }
        }
        async fn notify(&self, t: &str, b: &str) {
            self.notes.borrow_mut().push((t.into(), b.into()));
        }
    }

    const WIFI_LIST: &str = "eduroam 1:802-11-wireless:wlp4s0\ntailscale0:tun:tailscale0";
    const LIST: &str = "nmcli -t -f NAME,TYPE,DEVICE connection show --active";
    const PS: &str = "nmcli -g 802-11-wireless.powersave connection show id eduroam 1";
    const MOD3: &str = "nmcli connection modify id eduroam 1 802-11-wireless.powersave 3";

    fn tmpfile(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("powerwatch-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("state.toml")
    }

    fn ctx<'a>(f: &'a Fake, tag: &str, st: State) -> Ctx<&'a Fake> {
        Ctx {
            sys: f,
            cfg: Config::default(),
            dry: false,
            st,
            state_file: tmpfile(tag),
        }
    }

    fn running(f: &Fake, c: &str, v: bool) {
        f.reply(
            &format!("docker inspect -f {{{{.State.Running}}}} {c}"),
            Ok(if v { "true" } else { "false" }),
        );
    }

    // ----- pure -----

    #[test]
    fn config_defaults_and_merge() {
        assert_eq!(parse_config("").unwrap(), Config::default());
        let c = parse_config("containers = [\"a\"]\nnotify = false").unwrap();
        assert_eq!(c.containers, vec!["a"]);
        assert!(!c.notify);
        assert_eq!(c.battery_profile, "balanced");
        assert!(!c.wifi_powersave);
        assert!(parse_config("containers = 5").is_err());
    }

    #[test]
    fn shipped_default_config_matches_defaults() {
        assert_eq!(parse_config(DEFAULT_CONFIG).unwrap(), Config::default());
    }

    #[test]
    fn config_validation_catches_invalid_values() {
        assert!(validate_config(&Config::default()).is_ok());
        let cfg = parse_config(
            r#"battery_profile = "turbo"
low_battery_threshold = 101
containers = ["db", "db", ""]"#,
        )
        .unwrap();
        let errors = validate_config(&cfg).unwrap_err();
        assert_eq!(errors.len(), 4);
    }

    #[test]
    fn flap_detector_requires_repeated_changes_and_throttles() {
        let mut detector = FlapDetector::default();
        let start = Instant::now();
        for i in 0..FLAP_THRESHOLD - 1 {
            assert!(!detector.observe(start + Duration::from_secs(i as u64)));
        }
        assert!(detector.observe(start + Duration::from_secs(3)));
        assert!(!detector.observe(start + Duration::from_secs(4)));

        let later = start + FLAP_ALERT_COOLDOWN;
        for i in 0..FLAP_THRESHOLD - 1 {
            assert!(!detector.observe(later + Duration::from_secs(i as u64)));
        }
        assert!(detector.observe(later + Duration::from_secs(3)));
    }

    #[test]
    fn terse_split_and_wifi_parse() {
        assert_eq!(split_terse(r"a\:b:c\\d:e"), vec!["a:b", r"c\d", "e"]);
        assert_eq!(
            parse_active_wifi(WIFI_LIST),
            Some(("eduroam 1".into(), "wlp4s0".into()))
        );
        assert_eq!(
            parse_active_wifi(r"my\:net:802-11-wireless:wlan0"),
            Some(("my:net".into(), "wlan0".into()))
        );
        assert_eq!(
            parse_active_wifi("lo:loopback:lo\ndocker0:bridge:docker0"),
            None
        );
        assert_eq!(parse_active_wifi(""), None);
        // Wi-Fi with no device (inactive-looking) still has 3 fields; malformed lines are skipped.
        assert_eq!(
            parse_active_wifi("junk\nx:802-11-wireless:d"),
            Some(("x".into(), "d".into()))
        );
    }

    #[test]
    fn powersave_values() {
        for (i, o) in [
            ("enable", "3"),
            ("3", "3"),
            ("default", "0"),
            ("0 (default)", "0"),
            ("ignore", "1"),
            ("disable", "2"),
            (" enable\n", "3"),
        ] {
            assert_eq!(parse_powersave(i), Some(o), "{i}");
        }
        assert_eq!(parse_powersave("wat"), None);
        assert_eq!(parse_powersave(""), None);
    }

    #[test]
    fn profile_decisions() {
        assert_eq!(profile_to_remember("unknown"), "balanced");
        assert_eq!(profile_to_remember("performance"), "performance");
        assert_eq!(profile_to_restore(""), "balanced");
        assert_eq!(profile_to_restore("unknown"), "balanced");
        assert_eq!(profile_to_restore("power-saver"), "power-saver");
        let fresh = State::default();
        assert!(should_set_profile("balanced", "power-saver", &fresh));
        assert!(should_set_profile("unknown", "power-saver", &fresh));
        assert!(!should_set_profile("power-saver", "power-saver", &fresh));
        let done = State {
            on_battery: Some(true),
            prev_profile: Some("balanced".into()),
            ..Default::default()
        };
        assert!(!should_set_profile("unknown", "power-saver", &done));
        assert!(should_set_profile("balanced", "power-saver", &done)); // user changed it
    }

    #[test]
    fn missing_connection_detection() {
        assert!(is_missing_connection("Error: unknown connection 'x'."));
        assert!(!is_missing_connection("Permission denied"));
    }

    #[test]
    fn state_roundtrip_atomic_and_corrupt() {
        let p = tmpfile("state");
        assert_eq!(load_state_from(&p, false), State::default());
        let st = State {
            on_battery: Some(true),
            prev_profile: Some("balanced".into()),
            stopped: vec!["a".into(), "b".into()],
            wifi_con: Some("eduroam 1".into()),
            wifi_prev: Some("default".into()),
            ..Default::default()
        };
        save_state_to(&p, &st);
        assert!(!p.with_extension("tmp").exists());
        assert_eq!(load_state_from(&p, false), st);
        assert!(st.pending());
        assert!(!State::default().pending());

        std::fs::write(&p, "stopped = [[[ nope").unwrap();
        assert_eq!(load_state_from(&p, false), State::default());
        assert!(!p.exists());
        assert_eq!(
            std::fs::read_to_string(p.with_extension("corrupt")).unwrap(),
            "stopped = [[[ nope"
        );
    }

    #[test]
    fn corrupt_state_dry_run_leaves_file() {
        let p = tmpfile("corrupt-dry");
        std::fs::write(&p, "{{{").unwrap();
        load_state_from(&p, true);
        assert!(p.exists());
    }

    #[test]
    fn old_state_without_new_fields_loads() {
        let p = tmpfile("old");
        std::fs::write(&p, "on_battery = true\nstopped = [\"x\"]\n").unwrap();
        let st = load_state_from(&p, false);
        assert_eq!(st.stopped, vec!["x"]);
        assert_eq!(st.prev_profile, None);
    }

    // ----- state machine -----

    #[tokio::test]
    async fn battery_then_ac_full_cycle() {
        let f = Fake::default();
        *f.profile.borrow_mut() = "unknown".into();
        running(&f, "prefect-server", true);
        running(&f, "prefect-postgres", true);
        f.reply(LIST, Ok(WIFI_LIST));
        f.reply(PS, Ok("default"));
        let mut c = ctx(&f, "cycle", State::default());
        c.cfg.battery_profile = "power-saver".into();
        c.cfg.wifi_powersave = true;
        c.apply(true).await;
        assert_eq!(c.st.prev_profile.as_deref(), Some("balanced"));
        assert_eq!(c.st.stopped, vec!["prefect-server", "prefect-postgres"]);
        assert_eq!(c.st.wifi_con.as_deref(), Some("eduroam 1"));
        assert_eq!(c.st.wifi_prev.as_deref(), Some("0"));
        assert!(f.has("set_profile power-saver"));
        assert!(f.has(MOD3));
        assert!(
            !f.calls().iter().any(|x| x.contains("reapply")),
            "must not touch the live link"
        );
        // persisted
        assert_eq!(load_state_from(&c.state_file, false), c.st);

        // AC: wifi reads back as enable.
        f.reply(PS, Ok("enable"));
        f.calls.borrow_mut().clear();
        c.apply(false).await;
        let calls = f.calls();
        assert_eq!(calls[0], "set_profile balanced");
        let starts: Vec<_> = calls
            .iter()
            .filter(|x| x.starts_with("docker start"))
            .cloned()
            .collect();
        assert_eq!(
            starts,
            vec![
                "docker start prefect-postgres",
                "docker start prefect-server"
            ]
        );
        assert!(f.has("nmcli connection modify id eduroam 1 802-11-wireless.powersave 0"));
        assert!(!c.st.pending());
        assert_eq!(c.st.on_battery, Some(false));
        assert_eq!(load_state_from(&c.state_file, false), c.st);
    }

    #[tokio::test]
    async fn restart_mid_battery_is_idempotent() {
        let f = Fake::default();
        *f.profile.borrow_mut() = "unknown".into(); // tuned-ppd
        // Containers are already stopped by a previous run.
        running(&f, "prefect-server", false);
        running(&f, "prefect-postgres", false);
        f.reply(LIST, Ok(WIFI_LIST));
        f.reply(PS, Ok("enable"));
        let prior = State {
            on_battery: Some(true),
            prev_profile: Some("balanced".into()),
            stopped: vec!["prefect-server".into(), "prefect-postgres".into()],
            wifi_con: Some("eduroam 1".into()),
            wifi_prev: Some("default".into()),
            ..Default::default()
        };
        let mut c = ctx(&f, "restart", prior.clone());
        c.apply(true).await;
        assert_eq!(
            c.st, prior,
            "state (incl. original wifi value) must be preserved"
        );
        let calls = f.calls();
        assert!(
            !calls.iter().any(|x| x.starts_with("set_profile")),
            "{calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|x| x.contains("docker stop") || x.contains("docker start")),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|x| x.contains("modify")), "{calls:?}");
        assert!(f.notes.borrow().is_empty(), "no noise when nothing changed");
    }

    #[tokio::test]
    async fn does_not_restore_what_it_did_not_change() {
        let f = Fake::default();
        *f.profile.borrow_mut() = "balanced".into(); // user was already on balanced
        running(&f, "prefect-server", false); // user had them stopped
        running(&f, "prefect-postgres", false);
        f.reply(LIST, Ok(WIFI_LIST));
        f.reply(PS, Ok("enable"));
        let mut c = ctx(&f, "untouched", State::default());
        c.apply(true).await;
        assert!(!c.st.pending(), "{:?}", c.st);
        assert!(!f.calls().iter().any(|x| x.starts_with("set_profile")
            || x.contains("modify")
            || x.contains("docker stop")));
        c.apply(false).await;
        assert!(!f.calls().iter().any(|x| x.starts_with("set_profile")
            || x.contains("modify")
            || x.contains("docker start")));
    }

    #[tokio::test]
    async fn failed_profile_set_leaves_nothing_to_restore() {
        let f = Fake {
            set_profile_fails: true,
            ..Default::default()
        };
        *f.profile.borrow_mut() = "balanced".into();
        let mut c = ctx(&f, "pfail", State::default());
        c.cfg.containers.clear();
        c.cfg.wifi_powersave = false;
        c.apply(true).await;
        assert_eq!(c.st.prev_profile, None);
    }

    #[tokio::test]
    async fn failed_docker_stop_untracks() {
        let f = Fake::default();
        *f.profile.borrow_mut() = "power-saver".into();
        running(&f, "prefect-server", true);
        f.reply("docker stop prefect-server", Err("daemon error"));
        let mut c = ctx(&f, "dstop", State::default());
        c.cfg.containers = vec!["prefect-server".into()];
        c.cfg.wifi_powersave = false;
        c.apply(true).await;
        assert!(c.st.stopped.is_empty());
    }

    #[tokio::test]
    async fn failed_docker_start_stays_tracked() {
        let f = Fake::default();
        f.reply("docker start b", Err("no daemon"));
        let st = State {
            stopped: vec!["a".into(), "b".into()],
            on_battery: Some(true),
            ..Default::default()
        };
        let mut c = ctx(&f, "dstart", st);
        c.apply(false).await;
        assert_eq!(c.st.stopped, vec!["b"]);
        // order: reverse of stop order
        assert_eq!(
            f.calls()
                .iter()
                .filter(|x| x.starts_with("docker start"))
                .cloned()
                .collect::<Vec<_>>(),
            vec!["docker start b", "docker start a"]
        );
    }

    #[tokio::test]
    async fn ac_restore_unknown_or_empty_prev_uses_balanced() {
        for prev in ["unknown", ""] {
            let f = Fake::default();
            let st = State {
                prev_profile: Some(prev.into()),
                ..Default::default()
            };
            let mut c = ctx(&f, "unk", st);
            c.apply(false).await;
            assert!(f.has("set_profile balanced"), "{prev}");
            assert_eq!(c.st.prev_profile, None);
        }
    }

    #[tokio::test]
    async fn ac_profile_restore_failure_is_retained() {
        let f = Fake {
            set_profile_fails: true,
            ..Default::default()
        };
        let st = State {
            prev_profile: Some("balanced".into()),
            ..Default::default()
        };
        let mut c = ctx(&f, "pretain", st);
        c.apply(false).await;
        assert_eq!(c.st.prev_profile.as_deref(), Some("balanced"));
    }

    #[tokio::test]
    async fn pending_ac_restore_can_retry_without_rerunning_hooks() {
        let f = Fake::default();
        f.reply("docker start x", Err("no daemon"));
        let st = State {
            stopped: vec!["x".into()],
            on_battery: Some(true),
            ..Default::default()
        };
        let mut c = ctx(&f, "retry", st);
        c.cfg.containers.clear();
        c.cfg.wifi_powersave = false;
        c.cfg.on_ac_cmds = vec!["echo ac".into()];
        c.apply(false).await;
        assert!(c.st.pending());
        assert_eq!(
            f.calls()
                .iter()
                .filter(|x| x.as_str() == "sh -c echo ac")
                .count(),
            1
        );

        f.reply("docker start x", Ok(""));
        c.restore_ac_changes(&mut Vec::new()).await;
        assert!(!c.st.pending());
        assert_eq!(
            f.calls()
                .iter()
                .filter(|x| x.as_str() == "sh -c echo ac")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn low_battery_action_fires_once_and_rearms_after_recovery() {
        let f = Fake::default();
        f.battery.set(Some(10));
        let mut c = ctx(&f, "low-battery", State::default());
        c.cfg.on_low_battery_cmds = vec!["echo low".into()];
        c.check_low_battery(true).await;
        c.check_low_battery(true).await;
        assert!(c.st.low_battery_alerted);
        assert_eq!(
            f.calls()
                .iter()
                .filter(|x| x.as_str() == "sh -c echo low")
                .count(),
            1
        );
        assert_eq!(f.notes.borrow().len(), 1);

        f.battery.set(Some(20));
        c.check_low_battery(true).await;
        assert!(!c.st.low_battery_alerted);
        f.battery.set(Some(10));
        c.check_low_battery(true).await;
        assert_eq!(
            f.calls()
                .iter()
                .filter(|x| x.as_str() == "sh -c echo low")
                .count(),
            2
        );
        c.check_low_battery(false).await;
        assert!(!c.st.low_battery_alerted);
    }

    #[tokio::test]
    async fn wifi_name_with_spaces_is_one_argument() {
        let f = Fake::default();
        f.reply(LIST, Ok(WIFI_LIST));
        f.reply(PS, Ok("default"));
        let mut c = ctx(&f, "space", State::default());
        c.wifi_enable().await.unwrap();
        assert!(f.has(MOD3));
    }

    #[tokio::test]
    async fn wifi_restore_skips_value_changed_by_user_and_missing_connections() {
        let f = Fake::default();
        f.reply(PS, Ok("disable"));
        let st = State {
            wifi_con: Some("eduroam 1".into()),
            wifi_prev: Some("default".into()),
            ..Default::default()
        };
        let mut c = ctx(&f, "wuser", st);
        assert_eq!(c.wifi_restore().await.unwrap(), None);
        assert!(!f.calls().iter().any(|x| x.contains("modify")));
        assert!(!c.st.pending());

        let f = Fake::default();
        f.reply(PS, Err("Error: unknown connection 'eduroam 1'."));
        let st = State {
            wifi_con: Some("eduroam 1".into()),
            wifi_prev: Some("default".into()),
            ..Default::default()
        };
        let mut c = ctx(&f, "wgone", st);
        assert_eq!(c.wifi_restore().await.unwrap(), None);
        assert!(!c.st.pending());

        // Transient failure keeps the record for a later retry.
        let f = Fake::default();
        f.reply(PS, Err("nmcli timed out"));
        let st = State {
            wifi_con: Some("eduroam 1".into()),
            wifi_prev: Some("default".into()),
            ..Default::default()
        };
        let mut c = ctx(&f, "wtrans", st);
        assert!(c.wifi_restore().await.is_err());
        assert!(c.st.wifi_con.is_some());
    }

    #[tokio::test]
    async fn wifi_roaming_restores_previous_connection_first() {
        let f = Fake::default();
        f.reply(LIST, Ok("other:802-11-wireless:wlp4s0"));
        f.reply(
            "nmcli -g 802-11-wireless.powersave connection show id other",
            Ok("default"),
        );
        let st = State {
            wifi_con: Some("old".into()),
            wifi_prev: Some("2".into()),
            ..Default::default()
        };
        f.reply(
            "nmcli -g 802-11-wireless.powersave connection show id old",
            Ok("enable"),
        );
        let mut c = ctx(&f, "roam", st);
        c.wifi_enable().await.unwrap();
        assert!(f.has("nmcli connection modify id old 802-11-wireless.powersave 2"));
        assert_eq!(c.st.wifi_con.as_deref(), Some("other"));
    }

    #[tokio::test]
    async fn no_wifi_is_fine() {
        let f = Fake::default();
        f.reply(LIST, Ok("lo:loopback:lo"));
        let mut c = ctx(&f, "nowifi", State::default());
        assert_eq!(c.wifi_enable().await.unwrap(), None);
        assert!(!c.st.pending());
    }

    #[tokio::test]
    async fn dry_run_changes_nothing() {
        let f = Fake::default();
        *f.profile.borrow_mut() = "balanced".into();
        running(&f, "prefect-server", true);
        running(&f, "prefect-postgres", true);
        f.reply(LIST, Ok(WIFI_LIST));
        f.reply(PS, Ok("default"));
        let mut c = ctx(&f, "dry", State::default());
        c.dry = true;
        c.apply(true).await;
        let bad: Vec<_> = f
            .calls()
            .into_iter()
            .filter(|x| {
                x.starts_with("set_profile")
                    || x.contains("modify")
                    || x.contains("docker stop")
                    || x.starts_with("sh")
            })
            .collect();
        assert!(bad.is_empty(), "{bad:?}");
        assert!(!c.st.pending());
        assert!(!c.state_file.exists());
        assert!(f.notes.borrow().is_empty());
    }

    #[tokio::test]
    async fn extra_commands_and_notifications() {
        let f = Fake::default();
        f.reply("sh -c false", Err("sh failed"));
        let mut c = ctx(&f, "extra", State::default());
        c.cfg = Config {
            containers: vec![],
            wifi_powersave: false,
            on_battery_cmds: vec!["echo hi".into(), "false".into()],
            ..Config::default()
        };
        *f.profile.borrow_mut() = "power-saver".into();
        c.apply(true).await;
        assert!(f.has("sh -c echo hi"));
        let n = f.notes.borrow();
        assert_eq!(n.len(), 1);
        assert!(n[0].1.contains("command failed: false"));
    }

    #[tokio::test]
    async fn notify_disabled() {
        let f = Fake::default();
        *f.profile.borrow_mut() = "balanced".into();
        let mut c = ctx(&f, "nonotify", State::default());
        c.cfg = Config {
            notify: false,
            containers: vec![],
            wifi_powersave: false,
            ..Config::default()
        };
        c.apply(true).await;
        assert!(f.notes.borrow().is_empty());
    }

    #[test]
    fn power_budget_config_validation() {
        let mut cfg = Config {
            power_budget_watts: -5.0,
            ..Default::default()
        };
        assert!(validate_config(&cfg).is_err());
        cfg.power_budget_watts = 15.0;
        cfg.heavy_containers = vec!["".into()];
        assert!(validate_config(&cfg).is_err());
        cfg.heavy_containers = vec!["build-agent".into()];
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn state_pending_includes_heavy_stopped() {
        let mut st = State::default();
        assert!(!st.pending());
        st.heavy_stopped.push("heavy-db".into());
        assert!(st.pending());
    }

    #[test]
    fn rapl_power_limits_smoke() {
        let _ = read_rapl_power_limits();
    }
}
