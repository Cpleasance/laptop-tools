//! thermalwatch - Fast thermal monitoring, ThinkPad fan control, and CPU throttling diagnostics.

use serde::Serialize;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TempReading {
    pub label: String,
    pub temp_c: f32,
    pub max_c: Option<f32>,
    pub crit_c: Option<f32>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FanReading {
    pub rpm: u32,
    pub level: String,
    pub enabled: bool,
    pub controllable: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThermalReport {
    pub package_temp_c: Option<f32>,
    pub cores: Vec<TempReading>,
    pub fan: Option<FanReading>,
    pub nvme_temp_c: Option<f32>,
    pub wifi_temp_c: Option<f32>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThrottlingDiagnosis {
    pub cur_freq_mhz: u32,
    pub min_freq_mhz: u32,
    pub max_freq_mhz: u32,
    pub governor: String,
    pub epp: String,
    pub package_temp_c: Option<f32>,
    pub pl1_watts: Option<f32>,
    pub pl2_watts: Option<f32>,
    pub verdict: String,
    pub recommendation: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Status { json: bool },
    Diagnose { json: bool },
    SetFan { level: String },
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

pub fn read_sysfs_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse::<u64>().ok()
}

pub fn read_sysfs_string(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub fn collect_thermals() -> ThermalReport {
    let mut cores = Vec::new();
    let mut package_temp_c = None;
    let mut nvme_temp_c = None;
    let mut wifi_temp_c = None;

    let hwmon_dir = Path::new("/sys/class/hwmon");
    if let Ok(entries) = fs::read_dir(hwmon_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = read_sysfs_string(&path.join("name")).unwrap_or_default();

            if name == "coretemp" {
                // Find all temp*_input
                for i in 1..=16 {
                    let input_path = path.join(format!("temp{i}_input"));
                    if let Some(milli_c) = read_sysfs_u64(&input_path) {
                        let c = (milli_c as f32) / 1000.0;
                        let label = read_sysfs_string(&path.join(format!("temp{i}_label")))
                            .unwrap_or_else(|| format!("Sensor {i}"));
                        let max_c = read_sysfs_u64(&path.join(format!("temp{i}_max")))
                            .map(|v| (v as f32) / 1000.0);
                        let crit_c = read_sysfs_u64(&path.join(format!("temp{i}_crit")))
                            .map(|v| (v as f32) / 1000.0);

                        if label.contains("Package") {
                            package_temp_c = Some(c);
                        }

                        cores.push(TempReading {
                            label,
                            temp_c: c,
                            max_c,
                            crit_c,
                        });
                    }
                }
            } else if name == "nvme"
                && let Some(milli_c) = read_sysfs_u64(&path.join("temp1_input"))
            {
                nvme_temp_c = Some((milli_c as f32) / 1000.0);
            } else if name.starts_with("iwlwifi")
                && let Some(milli_c) = read_sysfs_u64(&path.join("temp1_input"))
            {
                wifi_temp_c = Some((milli_c as f32) / 1000.0);
            }
        }
    }

    let fan = collect_fan_info();

    ThermalReport {
        package_temp_c,
        cores,
        fan,
        nvme_temp_c,
        wifi_temp_c,
    }
}

pub fn collect_fan_info() -> Option<FanReading> {
    // 1. Try /proc/acpi/ibm/fan (ThinkPad ACPI)
    let ibm_fan = Path::new("/proc/acpi/ibm/fan");
    if ibm_fan.exists()
        && let Ok(content) = fs::read_to_string(ibm_fan)
    {
        let mut status = "enabled";
        let mut speed = 0u32;
        let mut level = "auto".to_string();

        for line in content.lines() {
            let mut parts = line.splitn(2, ':');
            if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                let k = k.trim();
                let v = v.trim();
                match k {
                    "status" => {
                        status = if v == "enabled" {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    }
                    "speed" => speed = v.parse::<u32>().unwrap_or(0),
                    "level" => level = v.to_string(),
                    _ => {}
                }
            }
        }

        let controllable = fs::OpenOptions::new().write(true).open(ibm_fan).is_ok();

        return Some(FanReading {
            rpm: speed,
            level,
            enabled: status == "enabled",
            controllable,
        });
    }

    // 2. Fallback: Search hwmon for any fan*_input
    let hwmon_dir = Path::new("/sys/class/hwmon");
    if let Ok(entries) = fs::read_dir(hwmon_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let fan_input = path.join("fan1_input");
            if let Some(rpm) = read_sysfs_u64(&fan_input) {
                return Some(FanReading {
                    rpm: rpm as u32,
                    level: "auto".to_string(),
                    enabled: true,
                    controllable: false,
                });
            }
        }
    }

    None
}

pub fn collect_diagnosis(report: &ThermalReport) -> ThrottlingDiagnosis {
    let cpu0_dir = Path::new("/sys/devices/system/cpu/cpu0/cpufreq");
    let cur_freq_mhz = read_sysfs_u64(&cpu0_dir.join("scaling_cur_freq"))
        .map(|v| (v / 1000) as u32)
        .unwrap_or(0);
    let min_freq_mhz = read_sysfs_u64(&cpu0_dir.join("scaling_min_freq"))
        .map(|v| (v / 1000) as u32)
        .unwrap_or(400);
    let max_freq_mhz = read_sysfs_u64(&cpu0_dir.join("scaling_max_freq"))
        .map(|v| (v / 1000) as u32)
        .unwrap_or(3900);

    let governor =
        read_sysfs_string(&cpu0_dir.join("scaling_governor")).unwrap_or_else(|| "unknown".into());
    let epp = read_sysfs_string(&cpu0_dir.join("energy_performance_preference"))
        .unwrap_or_else(|| "unknown".into());

    // RAPL limits
    let rapl_dir = Path::new("/sys/class/powercap/intel-rapl:0");
    let pl1_watts = read_sysfs_u64(&rapl_dir.join("constraint_0_power_limit_uw"))
        .map(|v| (v as f32) / 1_000_000.0);
    let pl2_watts = read_sysfs_u64(&rapl_dir.join("constraint_1_power_limit_uw"))
        .map(|v| (v as f32) / 1_000_000.0);

    let pkg_temp = report.package_temp_c.unwrap_or(45.0);

    let (verdict, recommendation) = if pkg_temp >= 90.0 {
        (
            "CRITICAL THERMAL THROTTLING: CPU temperature exceeds 90°C.".to_string(),
            "CPU is dropping clocks to avoid shutdown. Check fan intake vents for dust or consider fresh thermal paste.".to_string()
        )
    } else if pkg_temp >= 82.0 {
        (
            "MILD THERMAL CONSTRAINTS: Temperatures elevated (>82°C).".to_string(),
            "Fan is actively cooling. System is near thermal limits under load.".to_string(),
        )
    } else if cur_freq_mhz <= 800 && epp == "power" {
        (
            "EPP POWER THROTTLING: Clocks capped to 400-800 MHz by 'power' energy profile.".to_string(),
            "Switch to 'balanced' profile in powerwatch.toml to restore responsive 2.5-3.5 GHz clocking.".to_string()
        )
    } else {
        (
            "OPTIMAL: Thermals, clocks, and power headroom are running smoothly.".to_string(),
            "No thermal or hardware throttling detected.".to_string(),
        )
    };

    ThrottlingDiagnosis {
        cur_freq_mhz,
        min_freq_mhz,
        max_freq_mhz,
        governor,
        epp,
        package_temp_c: report.package_temp_c,
        pl1_watts,
        pl2_watts,
        verdict,
        recommendation,
    }
}

pub fn apply_fan_level(level: &str) -> Result<(), String> {
    let valid_levels = [
        "0",
        "1",
        "2",
        "3",
        "4",
        "5",
        "6",
        "7",
        "auto",
        "disengaged",
        "full-speed",
    ];
    if !valid_levels.contains(&level) {
        return Err(format!(
            "Invalid fan level '{level}'. Allowed: 0-7, auto, disengaged, full-speed"
        ));
    }

    let ibm_fan = Path::new("/proc/acpi/ibm/fan");
    if !ibm_fan.exists() {
        return Err("ThinkPad fan interface /proc/acpi/ibm/fan not available.".to_string());
    }

    let cmd_payload = format!("level {level}\n");
    if fs::write(ibm_fan, &cmd_payload).is_ok() {
        return Ok(());
    }

    // Try elevation if interactive
    if std::io::stdin().is_terminal() {
        eprintln!(
            "{}",
            color(
                "33",
                "Direct write to /proc/acpi/ibm/fan requires root. Elevating..."
            )
        );
        let cmd_str = format!("echo 'level {level}' > /proc/acpi/ibm/fan");
        if let Ok(s) = Command::new("sudo")
            .arg("sh")
            .arg("-c")
            .arg(&cmd_str)
            .status()
            && s.success()
        {
            return Ok(());
        }
    }

    Err("Permission denied writing to /proc/acpi/ibm/fan. Run with sudo: 'sudo thermalwatch fan <level>'".to_string())
}

pub fn print_status(report: &ThermalReport, json: bool) {
    if json {
        if let Ok(j) = serde_json::to_string_pretty(report) {
            println!("{j}");
        }
        return;
    }

    println!("{}", color("1;36", "=== System Thermals & Fan Status ==="));

    if let Some(pkg) = report.package_temp_c {
        let temp_color = if pkg >= 85.0 {
            "1;31"
        } else if pkg >= 70.0 {
            "1;33"
        } else {
            "1;32"
        };
        println!(
            "CPU Package:      {}",
            color(temp_color, &format!("{pkg:.1}°C"))
        );
    }

    for core in &report.cores {
        if !core.label.contains("Package") {
            println!("  {:<14} {:.1}°C", core.label, core.temp_c);
        }
    }

    if let Some(nvme) = report.nvme_temp_c {
        println!("NVMe SSD:         {nvme:.1}°C");
    }
    if let Some(wifi) = report.wifi_temp_c {
        println!("Wi-Fi Adapter:    {wifi:.1}°C");
    }

    println!();
    match &report.fan {
        Some(fan) => {
            let rpm_str = if fan.rpm == 0 {
                "0 RPM (Passive silent cooling)".to_string()
            } else {
                format!("{} RPM", fan.rpm)
            };
            println!("Fan Speed:        {}", color("1;34", &rpm_str));
            println!(
                "Fan Control:      Mode: {} ({})",
                fan.level,
                if fan.enabled { "enabled" } else { "disabled" }
            );
        }
        None => {
            println!("Fan Speed:        Not detected or generic ACPI");
        }
    }
}

pub fn print_diagnose(diag: &ThrottlingDiagnosis, json: bool) {
    if json {
        if let Ok(j) = serde_json::to_string_pretty(diag) {
            println!("{j}");
        }
        return;
    }

    println!(
        "{}",
        color("1;36", "=== CPU Throttling & Power Diagnostic ===")
    );
    println!(
        "Current Frequency: {} MHz (Range: {}-{} MHz)",
        color("1", &diag.cur_freq_mhz.to_string()),
        diag.min_freq_mhz,
        diag.max_freq_mhz
    );
    println!(
        "Governor / EPP:    {} / {}",
        diag.governor,
        color("1;33", &diag.epp)
    );

    if let (Some(pl1), Some(pl2)) = (diag.pl1_watts, diag.pl2_watts) {
        println!("Intel RAPL Limits: PL1: {pl1:.1}W (Sustained) | PL2: {pl2:.1}W (Burst)");
    }
    if let Some(pkg) = diag.package_temp_c {
        println!("Package Temp:      {pkg:.1}°C");
    }

    println!();
    println!("Diagnostic Verdict: {}", color("1;32", &diag.verdict));
    println!("Recommendation:     {}", diag.recommendation);
}

pub fn print_help() {
    println!(
        "thermalwatch - Fast thermal monitoring, ThinkPad fan control, and CPU throttling diagnostics\n\n\
        USAGE:\n\
            thermalwatch [COMMAND] [OPTIONS]\n\n\
        COMMANDS:\n\
            status [--json]      Show CPU core temperatures, NVMe/Wi-Fi temps, and fan RPM\n\
            diagnose [--json]    Diagnose CPU clock throttling (RAPL power cap, thermals, EPP)\n\
            fan <LEVEL>          Set ThinkPad fan speed (0-7, auto, disengaged, full-speed)\n\
            help, -h, --help     Show this help message\n\n\
        OPTIONS:\n\
            --json               Format output as JSON\n\
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
        "diagnose" => {
            let json = args.iter().any(|a| a == "--json");
            Action::Diagnose { json }
        }
        "fan" => {
            if args.len() >= 3 {
                Action::SetFan {
                    level: args[2].clone(),
                }
            } else {
                eprintln!("Usage: thermalwatch fan <auto|0-7|disengaged|full-speed>");
                Action::Help
            }
        }
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

    match action {
        Action::Status { json } => {
            let report = collect_thermals();
            print_status(&report, json);
        }
        Action::Diagnose { json } => {
            let report = collect_thermals();
            let diag = collect_diagnosis(&report);
            print_diagnose(&diag, json);
        }
        Action::SetFan { level } => match apply_fan_level(&level) {
            Ok(()) => {
                println!(
                    "{}",
                    color(
                        "1;32",
                        &format!("Successfully set fan speed level to '{level}'.")
                    )
                );
            }
            Err(e) => {
                eprintln!("{}: {e}", color("1;31", "Error"));
                std::process::exit(1);
            }
        },
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
        assert_eq!(
            parse_args(&["bin".into(), "diagnose".into()]),
            Action::Diagnose { json: false }
        );
        assert_eq!(
            parse_args(&["bin".into(), "fan".into(), "auto".into()]),
            Action::SetFan {
                level: "auto".into()
            }
        );
    }

    #[test]
    fn test_diagnosis_logic() {
        let report = ThermalReport {
            package_temp_c: Some(48.0),
            cores: vec![],
            fan: None,
            nvme_temp_c: None,
            wifi_temp_c: None,
        };
        let diag = collect_diagnosis(&report);
        assert!(diag.package_temp_c == Some(48.0));
    }
}
