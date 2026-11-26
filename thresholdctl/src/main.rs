//! thresholdctl - Fast, clean CLI for managing ThinkPad & Linux laptop battery charge thresholds.

use serde::Serialize;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct BatteryThresholdInfo {
    pub battery: String,
    pub capacity_pct: Option<u8>,
    pub status: Option<String>,
    pub start_threshold: Option<u8>,
    pub stop_threshold: Option<u8>,
    pub mode: String,
    pub writable: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Status { json: bool },
    Desk,
    Travel,
    Set { start: u8, stop: u8 },
    GenerateUdevRule,
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

pub fn find_battery_path() -> Option<PathBuf> {
    let power_supply = Path::new("/sys/class/power_supply");
    if !power_supply.exists() {
        return None;
    }

    // Try BAT0, BAT1 first
    for name in &["BAT0", "BAT1", "BATC"] {
        let p = power_supply.join(name);
        if p.exists() {
            return Some(p);
        }
    }

    // Fallback: search directory for any BAT*
    if let Ok(entries) = fs::read_dir(power_supply) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.starts_with("BAT") {
                return Some(entry.path());
            }
        }
    }

    None
}

pub fn read_threshold_file(bat_path: &Path, filenames: &[&str]) -> Option<u8> {
    for name in filenames {
        let p = bat_path.join(name);
        if let Ok(content) = fs::read_to_string(&p)
            && let Ok(val) = content.trim().parse::<u8>()
        {
            return Some(val);
        }
    }
    None
}

pub fn get_threshold_write_path<'a>(bat_path: &'a Path, filenames: &'a [&str]) -> Option<PathBuf> {
    for name in filenames {
        let p = bat_path.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub fn check_writable(path: &Path) -> bool {
    fs::OpenOptions::new().write(true).open(path).is_ok()
}

pub fn query_battery(bat_path: &Path) -> BatteryThresholdInfo {
    let bat_name = bat_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "BAT0".to_string());

    let capacity = fs::read_to_string(bat_path.join("capacity"))
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok());

    let status = fs::read_to_string(bat_path.join("status"))
        .ok()
        .map(|s| s.trim().to_string());

    let start_threshold = read_threshold_file(
        bat_path,
        &["charge_control_start_threshold", "charge_start_threshold"],
    );
    let stop_threshold = read_threshold_file(
        bat_path,
        &["charge_control_end_threshold", "charge_stop_threshold"],
    );

    let mode = match (start_threshold, stop_threshold) {
        (Some(start), Some(stop)) => {
            if stop <= 80 && start <= 75 {
                "Desk Mode (Optimized battery lifespan)".to_string()
            } else if stop >= 95 {
                "Travel Mode (Full 100% capacity)".to_string()
            } else {
                format!("Custom ({start}% - {stop}%)")
            }
        }
        _ => "Not configured or unsupported".to_string(),
    };

    let writable = get_threshold_write_path(
        bat_path,
        &["charge_control_end_threshold", "charge_stop_threshold"],
    )
    .map(|p| check_writable(&p))
    .unwrap_or(false);

    BatteryThresholdInfo {
        battery: bat_name,
        capacity_pct: capacity,
        status,
        start_threshold,
        stop_threshold,
        mode,
        writable,
    }
}

pub fn validate_thresholds(start: u8, stop: u8) -> Result<(), &'static str> {
    if stop > 100 {
        return Err("Stop threshold cannot exceed 100%");
    }
    if start >= stop {
        return Err("Start threshold must be strictly less than stop threshold");
    }
    Ok(())
}

pub fn apply_thresholds(bat_path: &Path, start: u8, stop: u8) -> Result<(), String> {
    validate_thresholds(start, stop).map_err(|e| e.to_string())?;

    let start_path = get_threshold_write_path(
        bat_path,
        &["charge_control_start_threshold", "charge_start_threshold"],
    )
    .ok_or_else(|| "Could not find start threshold sysfs path".to_string())?;

    let stop_path = get_threshold_write_path(
        bat_path,
        &["charge_control_end_threshold", "charge_stop_threshold"],
    )
    .ok_or_else(|| "Could not find stop threshold sysfs path".to_string())?;

    // Attempt direct write first (works if writable via udev or root)
    let direct_start = fs::write(&start_path, format!("{start}\n"));
    let direct_stop = fs::write(&stop_path, format!("{stop}\n"));

    if direct_start.is_ok() && direct_stop.is_ok() {
        return Ok(());
    }

    // Direct write failed. If we are in an interactive shell, try elevating via sudo or pkexec.
    if std::io::stdin().is_terminal() {
        eprintln!(
            "{}",
            color(
                "33",
                "Direct write requires privileges. Elevating to apply thresholds..."
            )
        );

        let cmd_str = format!(
            "echo {start} > {} && echo {stop} > {}",
            start_path.display(),
            stop_path.display()
        );

        // Try sudo first
        let status = Command::new("sudo")
            .arg("sh")
            .arg("-c")
            .arg(&cmd_str)
            .status();

        if let Ok(s) = status
            && s.success()
        {
            return Ok(());
        }

        // Try pkexec fallback
        let pk_status = Command::new("pkexec")
            .arg("sh")
            .arg("-c")
            .arg(&cmd_str)
            .status();

        if let Ok(s) = pk_status
            && s.success()
        {
            return Ok(());
        }
    }

    Err(format!(
        "Permission denied writing to {} and {}.\n\
        Run with sudo: 'sudo thresholdctl set {start} {stop}', or install the udev rule with 'thresholdctl generate-udev-rule'.",
        start_path.display(),
        stop_path.display()
    ))
}

pub fn print_status(info: &BatteryThresholdInfo, json: bool) {
    if json {
        if let Ok(j) = serde_json::to_string_pretty(info) {
            println!("{j}");
        }
        return;
    }

    println!(
        "{}",
        color(
            "1;36",
            &format!("=== Battery Charge Thresholds ({}) ===", info.battery)
        )
    );
    let charge_str = match info.capacity_pct {
        Some(pct) => format!("{pct}% ({})", info.status.as_deref().unwrap_or("Unknown")),
        None => "Unknown".to_string(),
    };
    println!("Current charge:  {}", color("1", &charge_str));

    let start_str = info
        .start_threshold
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| "N/A".to_string());
    let stop_str = info
        .stop_threshold
        .map(|v| format!("{v}%"))
        .unwrap_or_else(|| "N/A".to_string());

    println!("Start threshold: {}", color("32", &start_str));
    println!("Stop threshold:  {}", color("32", &stop_str));
    println!("Profile Mode:    {}", color("1;33", &info.mode));

    let perm_str = if info.writable {
        color("32", "Writable without sudo (udev rule active)")
    } else {
        color("33", "Requires elevation or udev rule")
    };
    println!("Permissions:     {perm_str}");
    println!();
    println!("Quick commands:");
    println!("  thresholdctl desk     -> 75% start / 80% stop (Max battery lifespan)");
    println!("  thresholdctl travel   -> 95% start / 100% stop (Full capacity for travel)");
    println!("  thresholdctl set X Y  -> Custom thresholds (e.g. thresholdctl set 60 70)");
}

pub fn print_udev_rule() {
    println!(
        "# Place this file at /etc/udev/rules.d/99-battery-thresholds.rules\n\
        # to allow setting battery thresholds without sudo:\n\
        ACTION==\"add|change\", SUBSYSTEM==\"power_supply\", ATTR{{charge_control_start_threshold}}==\"*\", ATTR{{charge_control_end_threshold}}==\"*\", MODE=\"0664\", TAG+=\"uaccess\"\n\
        \n\
        # To apply immediately:\n\
        # sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=power_supply"
    );
}

pub fn print_help() {
    println!(
        "thresholdctl - Battery charge threshold switcher for ThinkPad and Linux laptops\n\n\
        USAGE:\n\
            thresholdctl [COMMAND] [OPTIONS]\n\n\
        COMMANDS:\n\
            status [--json]      Show current battery thresholds and charging status\n\
            desk                 Set desk profile: 75% start / 80% stop (saves battery cycles)\n\
            travel               Set travel profile: 95% start / 100% stop (full charge for trips)\n\
            set <START> <STOP>   Set custom thresholds (e.g. thresholdctl set 60 80)\n\
            generate-udev-rule   Print udev rule to allow setting thresholds without sudo\n\
            help, -h, --help     Show this help message\n\n\
        OPTIONS:\n\
            --json               Format status output as JSON (for Waybar, Polybar, or scripts)\n\
            NO_COLOR=1           Disable ANSI color formatting"
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
        "desk" => Action::Desk,
        "travel" => Action::Travel,
        "set" => {
            if args.len() >= 4
                && let (Ok(start), Ok(stop)) = (args[2].parse::<u8>(), args[3].parse::<u8>())
            {
                return Action::Set { start, stop };
            }
            eprintln!("Usage: thresholdctl set <start: 0-99> <stop: 1-100>");
            Action::Help
        }
        "generate-udev-rule" => Action::GenerateUdevRule,
        "help" | "-h" | "--help" => Action::Help,
        other => {
            eprintln!("Unknown command: {other}");
            Action::Help
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let action = parse_args(&args);

    let bat_path = match find_battery_path() {
        Some(p) => p,
        None => {
            eprintln!("Error: No battery found in /sys/class/power_supply/");
            std::process::exit(1);
        }
    };

    match action {
        Action::Status { json } => {
            let info = query_battery(&bat_path);
            print_status(&info, json);
        }
        Action::Desk => match apply_thresholds(&bat_path, 75, 80) {
            Ok(()) => {
                println!(
                    "{}",
                    color(
                        "1;32",
                        "Successfully applied Desk Profile (75% start / 80% stop)."
                    )
                );
            }
            Err(e) => {
                eprintln!("{}: {e}", color("1;31", "Error"));
                std::process::exit(1);
            }
        },
        Action::Travel => match apply_thresholds(&bat_path, 95, 100) {
            Ok(()) => {
                println!(
                    "{}",
                    color(
                        "1;32",
                        "Successfully applied Travel Profile (95% start / 100% stop)."
                    )
                );
            }
            Err(e) => {
                eprintln!("{}: {e}", color("1;31", "Error"));
                std::process::exit(1);
            }
        },
        Action::Set { start, stop } => match apply_thresholds(&bat_path, start, stop) {
            Ok(()) => {
                println!(
                    "{}",
                    color(
                        "1;32",
                        &format!("Successfully set thresholds to {start}% start / {stop}% stop.")
                    )
                );
            }
            Err(e) => {
                eprintln!("{}: {e}", color("1;31", "Error"));
                std::process::exit(1);
            }
        },
        Action::GenerateUdevRule => {
            print_udev_rule();
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
    fn test_validate_thresholds() {
        assert!(validate_thresholds(75, 80).is_ok());
        assert!(validate_thresholds(95, 100).is_ok());
        assert!(validate_thresholds(0, 100).is_ok());

        assert!(validate_thresholds(80, 75).is_err());
        assert!(validate_thresholds(80, 80).is_err());
        assert!(validate_thresholds(80, 101).is_err());
    }

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
        assert_eq!(
            parse_args(&["bin".into(), "--json".into()]),
            Action::Status { json: true }
        );
        assert_eq!(parse_args(&["bin".into(), "desk".into()]), Action::Desk);
        assert_eq!(parse_args(&["bin".into(), "travel".into()]), Action::Travel);
        assert_eq!(
            parse_args(&["bin".into(), "set".into(), "60".into(), "70".into()]),
            Action::Set {
                start: 60,
                stop: 70
            }
        );
        assert_eq!(
            parse_args(&["bin".into(), "generate-udev-rule".into()]),
            Action::GenerateUdevRule
        );
        assert_eq!(parse_args(&["bin".into(), "--help".into()]), Action::Help);
    }

    #[test]
    fn test_json_serialization() {
        let info = BatteryThresholdInfo {
            battery: "BAT0".into(),
            capacity_pct: Some(80),
            status: Some("Charging".into()),
            start_threshold: Some(75),
            stop_threshold: Some(80),
            mode: "Desk Mode".into(),
            writable: true,
        };
        let s = serde_json::to_string(&info).unwrap();
        assert!(s.contains("\"start_threshold\":75"));
        assert!(s.contains("\"stop_threshold\":80"));
        assert!(s.contains("\"writable\":true"));
    }
}
