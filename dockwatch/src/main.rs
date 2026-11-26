//! dockwatch - Thunderbolt & USB-C dock, external display, and audio hotplug monitor.

use serde::Serialize;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DrmConnector {
    pub name: String,
    pub connected: bool,
    pub is_internal: bool,
    pub modes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TypeCPort {
    pub name: String,
    pub data_role: Option<String>,
    pub power_role: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DockState {
    pub is_docked: bool,
    pub internal_display: Option<String>,
    pub external_displays: Vec<String>,
    pub typec_ports: Vec<TypeCPort>,
    pub audio_sink: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Status { json: bool },
    Daemon,
    RunHooks,
    Help,
}

pub fn is_color_enabled() -> bool {
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if env::var("TERM").map(|v| v == "dumb").unwrap_or(false) {
        return false;
    }
    std::io::stdout().is_terminal()
}

pub fn color(code: &str, text: &str) -> String {
    if is_color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn scan_drm_connectors() -> Vec<DrmConnector> {
    let mut list = Vec::new();
    let drm_path = Path::new("/sys/class/drm");
    if let Ok(entries) = fs::read_dir(drm_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // Match card*-<connector>, skip card1 without connector
            if name.starts_with("card") && name.contains('-') {
                let status_path = path.join("status");
                let connected = fs::read_to_string(&status_path)
                    .map(|s| s.trim() == "connected")
                    .unwrap_or(false);

                let is_internal =
                    name.contains("eDP") || name.contains("LVDS") || name.contains("DSI");

                let mut modes = Vec::new();
                if let Ok(modes_content) = fs::read_to_string(path.join("modes")) {
                    for line in modes_content.lines().take(3) {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            modes.push(trimmed.to_string());
                        }
                    }
                }

                list.push(DrmConnector {
                    name,
                    connected,
                    is_internal,
                    modes,
                });
            }
        }
    }
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

pub fn scan_typec_ports() -> Vec<TypeCPort> {
    let mut ports = Vec::new();
    let typec_path = Path::new("/sys/class/typec");
    if let Ok(entries) = fs::read_dir(typec_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("port") {
                let data_role = fs::read_to_string(path.join("data_role"))
                    .ok()
                    .map(|s| s.trim().to_string());
                let power_role = fs::read_to_string(path.join("power_role"))
                    .ok()
                    .map(|s| s.trim().to_string());

                ports.push(TypeCPort {
                    name,
                    data_role,
                    power_role,
                });
            }
        }
    }
    ports.sort_by(|a, b| a.name.cmp(&b.name));
    ports
}

pub fn get_default_audio_sink() -> Option<String> {
    // Try wpctl first (PipeWire)
    if let Ok(out) = Command::new("wpctl").arg("status").output()
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        // Look for line under Audio -> Sinks with '*'
        for line in s.lines() {
            if line.contains('*') && (line.contains("vol:") || line.contains('[')) {
                let cleaned =
                    line.trim_start_matches(|c: char| c == '*' || c == '.' || c.is_whitespace());
                return Some(cleaned.to_string());
            }
        }
    }

    // Try pactl fallback
    if let Ok(out) = Command::new("pactl").args(["get-default-sink"]).output()
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }

    None
}

pub fn query_dock_state() -> DockState {
    let connectors = scan_drm_connectors();
    let typec_ports = scan_typec_ports();
    let audio_sink = get_default_audio_sink();

    let mut internal_display = None;
    let mut external_displays = Vec::new();

    for c in connectors {
        if c.connected {
            if c.is_internal {
                internal_display = Some(c.name);
            } else {
                let desc = if !c.modes.is_empty() {
                    format!("{} ({})", c.name, c.modes[0])
                } else {
                    c.name
                };
                external_displays.push(desc);
            }
        }
    }

    let is_docked = !external_displays.is_empty();

    DockState {
        is_docked,
        internal_display,
        external_displays,
        typec_ports,
        audio_sink,
    }
}

pub fn execute_hook(hook_name: &str) {
    if let Some(config_dir) = dirs_config_dir() {
        let hook_path = config_dir.join("dockwatch").join(hook_name);
        if hook_path.exists() {
            println!("Executing hook: {}", hook_path.display());
            let status = Command::new("sh").arg("-c").arg(&hook_path).status();
            match status {
                Ok(s) if s.success() => println!("Hook finished successfully."),
                Ok(s) => eprintln!("Hook exited with status: {s}"),
                Err(e) => eprintln!("Failed to run hook: {e}"),
            }
        }
    }
}

fn dirs_config_dir() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

pub fn print_status(state: &DockState, json: bool) {
    if json {
        if let Ok(j) = serde_json::to_string_pretty(state) {
            println!("{j}");
        }
        return;
    }

    println!("{}", color("1;36", "=== Dock & Display State ==="));
    let status_str = if state.is_docked {
        color("1;32", "Docked (External displays connected)")
    } else {
        color("1;34", "Mobile / Undocked (Internal display only)")
    };
    println!("Operating Mode:    {status_str}");

    let internal_str = state
        .internal_display
        .as_deref()
        .unwrap_or("None or disconnected");
    println!("Internal Panel:    {internal_str}");

    if state.external_displays.is_empty() {
        println!("External Displays: None");
    } else {
        println!(
            "External Displays: {}",
            color("1;32", &state.external_displays.join(", "))
        );
    }

    if !state.typec_ports.is_empty() {
        let port_info: Vec<String> = state
            .typec_ports
            .iter()
            .map(|p| {
                format!(
                    "{} (data: {}, power: {})",
                    p.name,
                    p.data_role.as_deref().unwrap_or("none"),
                    p.power_role.as_deref().unwrap_or("none")
                )
            })
            .collect();
        println!("Type-C Ports:      {}", port_info.join("; "));
    }

    if let Some(sink) = &state.audio_sink {
        println!("Audio Default:     {sink}");
    }
}

pub fn print_help() {
    println!(
        "dockwatch - Thunderbolt & USB-C dock, external display, and audio hotplug monitor\n\n\
        USAGE:\n\
            dockwatch [COMMAND] [OPTIONS]\n\n\
        COMMANDS:\n\
            status [--json]   Show dock state, connected displays, Type-C ports, and audio sink\n\
            watch             Run daemon watching for dock/undock events and triggering hooks\n\
            run-hooks         Run ~/.config/dockwatch/on-dock.sh or on-undock.sh based on state\n\
            help, -h, --help  Show this help message\n\n\
        OPTIONS:\n\
            --json            Format output as JSON\n\
            NO_COLOR=1        Disable ANSI color formatting"
    );
}

pub fn parse_args(args: &[String]) -> Action {
    if args.len() <= 1 {
        return Action::Status { json: false };
    }

    match args[1].as_str() {
        "status" => {
            let json = args.iter().any(|a| a == "--json");
            Action::Status { json }
        }
        "--json" => Action::Status { json: true },
        "watch" => Action::Daemon,
        "run-hooks" => Action::RunHooks,
        "help" | "-h" | "--help" => Action::Help,
        other => {
            eprintln!("Unknown command: {other}");
            Action::Help
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let action = parse_args(&args);

    match action {
        Action::Status { json } => {
            let state = query_dock_state();
            print_status(&state, json);
        }
        Action::RunHooks => {
            let state = query_dock_state();
            if state.is_docked {
                execute_hook("on-dock.sh");
            } else {
                execute_hook("on-undock.sh");
            }
        }
        Action::Daemon => {
            println!("dockwatch daemon started. Monitoring dock and display status changes...");
            let mut last_docked = query_dock_state().is_docked;
            let mut interval = tokio::time::interval(Duration::from_secs(3));

            loop {
                interval.tick().await;
                let current_state = query_dock_state();
                if current_state.is_docked != last_docked {
                    last_docked = current_state.is_docked;
                    if last_docked {
                        println!(
                            "Dock connected! External displays: {:?}",
                            current_state.external_displays
                        );
                        execute_hook("on-dock.sh");
                    } else {
                        println!("Dock disconnected. System is now mobile.");
                        execute_hook("on-undock.sh");
                    }
                }
            }
        }
        Action::Help => {
            print_help();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_args() {
        assert_eq!(parse_args(&["bin".into()]), Action::Status { json: false });
        assert_eq!(
            parse_args(&["bin".into(), "status".into()]),
            Action::Status { json: false }
        );
        assert_eq!(
            parse_args(&["bin".into(), "status".into(), "--json".into()]),
            Action::Status { json: true }
        );
        assert_eq!(parse_args(&["bin".into(), "watch".into()]), Action::Daemon);
        assert_eq!(
            parse_args(&["bin".into(), "run-hooks".into()]),
            Action::RunHooks
        );
    }

    #[test]
    fn test_dock_state_json() {
        let state = DockState {
            is_docked: true,
            internal_display: Some("card1-eDP-1".into()),
            external_displays: vec!["card1-DP-1 (2560x1440)".into()],
            typec_ports: vec![],
            audio_sink: Some("USB Audio Dac".into()),
        };
        let s = serde_json::to_string(&state).unwrap();
        assert!(s.contains("\"is_docked\":true"));
        assert!(s.contains("2560x1440"));
    }
}
