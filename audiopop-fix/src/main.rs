//! audiopop-fix - Diagnostic and remediation tool for Intel HDA audio power-save click and pop issues on Linux.

use serde::Serialize;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AudioPowerStatus {
    pub module_loaded: bool,
    pub power_save_secs: Option<u32>,
    pub power_save_controller: Option<String>,
    pub pop_risk: String,
    pub persistent_config_present: bool,
    pub recommendation: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Status { json: bool },
    DisablePop,
    EnablePowersave,
    GenerateConf,
    TestTone,
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

pub fn read_audio_status() -> AudioPowerStatus {
    let power_save_path = Path::new("/sys/module/snd_hda_intel/parameters/power_save");
    let controller_path = Path::new("/sys/module/snd_hda_intel/parameters/power_save_controller");

    let module_loaded = power_save_path.exists();
    let power_save_secs = fs::read_to_string(power_save_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let power_save_controller = fs::read_to_string(controller_path)
        .ok()
        .map(|s| s.trim().to_string());

    let persistent_config_present = Path::new("/etc/modprobe.d/audio_powersave.conf").exists()
        || Path::new("/etc/modprobe.d/snd_hda_intel.conf").exists();

    let (pop_risk, recommendation) = match (power_save_secs, power_save_controller.as_deref()) {
        (Some(0), Some("N") | None) => (
            "LOW / ELIMINATED: Power-saving D3hot sleep is disabled; no pops or clicks.".to_string(),
            "Audio codec is running continuously without sleep-wake transients.".to_string(),
        ),
        (Some(secs), Some("Y")) if secs > 0 => (
            format!("HIGH: Audio sleep timeout is set to {secs}s with controller power-save active."),
            "When audio stops for 1 second, codec powers down. Starting audio causes capacitor charging pop in headphones. Run 'audiopop-fix disable-pop' to fix.".to_string(),
        ),
        _ => (
            "MODERATE / UNKNOWN: Module parameters are non-standard or not loaded.".to_string(),
            "Check audio driver status or run 'audiopop-fix generate-conf' to review modprobe settings.".to_string(),
        ),
    };

    AudioPowerStatus {
        module_loaded,
        power_save_secs,
        power_save_controller,
        pop_risk,
        persistent_config_present,
        recommendation,
    }
}

pub fn apply_live_settings(secs: u32, controller: &str) -> Result<(), String> {
    let p1 = Path::new("/sys/module/snd_hda_intel/parameters/power_save");
    let p2 = Path::new("/sys/module/snd_hda_intel/parameters/power_save_controller");

    if !p1.exists() {
        return Err("snd_hda_intel kernel module is not loaded.".into());
    }

    let d1 = fs::write(p1, format!("{secs}\n"));
    let d2 = fs::write(p2, format!("{controller}\n"));

    if d1.is_ok() && d2.is_ok() {
        return Ok(());
    }

    if std::io::stdin().is_terminal() {
        eprintln!(
            "{}",
            color(
                "33",
                "Writing to audio kernel module parameters requires root. Elevating..."
            )
        );
        let cmd = format!(
            "echo {secs} > {} && echo {controller} > {}",
            p1.display(),
            p2.display()
        );
        if let Ok(s) = Command::new("sudo").arg("sh").arg("-c").arg(&cmd).status()
            && s.success()
        {
            return Ok(());
        }
    }

    Err(format!(
        "Permission denied updating sysfs audio parameters.\n\
        Run with sudo: 'echo {secs} | sudo tee {} && echo {controller} | sudo tee {}'",
        p1.display(),
        p2.display()
    ))
}

pub fn print_conf_template() {
    println!(
        "# Place this file at /etc/modprobe.d/audio_powersave.conf to persist across reboots:\n\
        options snd_hda_intel power_save=0 power_save_controller=N\n\
        \n\
        # To apply immediately without rebooting:\n\
        # audiopop-fix disable-pop"
    );
}

pub fn print_status(status: &AudioPowerStatus, json: bool) {
    if json {
        if let Ok(j) = serde_json::to_string_pretty(status) {
            println!("{j}");
        }
        return;
    }

    println!(
        "{}",
        color(
            "1;36",
            "=== Intel HDA Audio Power-Save & Pop Diagnostic ==="
        )
    );
    println!(
        "Kernel Module:         {}",
        if status.module_loaded {
            color("1;32", "snd_hda_intel (loaded)")
        } else {
            color("1;31", "not loaded")
        }
    );

    let secs_str = status
        .power_save_secs
        .map(|s| format!("{s}s timeout"))
        .unwrap_or_else(|| "N/A".to_string());
    let ctrl_str = status.power_save_controller.as_deref().unwrap_or("N/A");

    println!("Power-save timeout:    {secs_str}");
    println!("Power-save controller: {ctrl_str}");
    println!(
        "Modprobe config:       {}",
        if status.persistent_config_present {
            color("1;32", "Configured in /etc/modprobe.d/")
        } else {
            color("33", "Not configured (defaults active)")
        }
    );
    println!();
    println!(
        "Click/Pop Risk:        {}",
        if status.power_save_secs == Some(0) {
            color("1;32", &status.pop_risk)
        } else {
            color("1;33", &status.pop_risk)
        }
    );
    println!("Recommendation:        {}", status.recommendation);
    println!();
    println!("Commands:");
    println!("  audiopop-fix disable-pop       -> Turn off aggressive sleep (stops pops)");
    println!("  audiopop-fix enable-powersave  -> Restore standard power-saving");
    println!("  audiopop-fix generate-conf     -> View /etc/modprobe.d/ snippet");
    println!("  audiopop-fix test-tone         -> Play a short tone to verify output");
}

pub fn play_test_tone() {
    println!("Testing audio output with short beep...");
    let succeeded = Command::new("pw-play")
        .args(["/usr/share/sounds/freedesktop/stereo/complete.oga"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !succeeded {
        let _ = Command::new("paplay")
            .args(["/usr/share/sounds/freedesktop/stereo/complete.oga"])
            .status();
    }
    println!("Test playback finished.");
}

pub fn print_help() {
    println!(
        "audiopop-fix - Diagnostic and remediation tool for Intel HDA audio power-save click/pop issues\n\n\
        USAGE:\n\
            audiopop-fix [COMMAND] [OPTIONS]\n\n\
        COMMANDS:\n\
            status [--json]      Show current audio power saving settings and click/pop risk\n\
            disable-pop          Turn off power-save timeout to eliminate headphone clicks\n\
            enable-powersave     Restore standard 1-second power-save sleep\n\
            generate-conf        Print /etc/modprobe.d/ configuration for persistence\n\
            test-tone            Play a brief test sound to verify clean output\n\
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
        "disable-pop" => Action::DisablePop,
        "enable-powersave" => Action::EnablePowersave,
        "generate-conf" => Action::GenerateConf,
        "test-tone" => Action::TestTone,
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
            let s = read_audio_status();
            print_status(&s, json);
        }
        Action::DisablePop => match apply_live_settings(0, "N") {
            Ok(()) => {
                println!(
                    "{}",
                    color(
                        "1;32",
                        "Successfully disabled audio sleep (power_save=0, power_save_controller=N)."
                    )
                );
                println!(
                    "To make this permanent across reboots, run: 'audiopop-fix generate-conf | sudo tee /etc/modprobe.d/audio_powersave.conf'"
                );
            }
            Err(e) => {
                eprintln!("{}: {e}", color("1;31", "Error"));
                std::process::exit(1);
            }
        },
        Action::EnablePowersave => match apply_live_settings(1, "Y") {
            Ok(()) => {
                println!(
                    "{}",
                    color(
                        "1;32",
                        "Successfully enabled standard power-save (power_save=1, power_save_controller=Y)."
                    )
                );
            }
            Err(e) => {
                eprintln!("{}: {e}", color("1;31", "Error"));
                std::process::exit(1);
            }
        },
        Action::GenerateConf => {
            print_conf_template();
        }
        Action::TestTone => {
            play_test_tone();
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
        assert_eq!(
            parse_args(&["bin".into(), "disable-pop".into()]),
            Action::DisablePop
        );
        assert_eq!(
            parse_args(&["bin".into(), "generate-conf".into()]),
            Action::GenerateConf
        );
    }

    #[test]
    fn test_status_json() {
        let status = AudioPowerStatus {
            module_loaded: true,
            power_save_secs: Some(0),
            power_save_controller: Some("N".into()),
            pop_risk: "LOW".into(),
            persistent_config_present: true,
            recommendation: "Optimal".into(),
        };
        let s = serde_json::to_string(&status).unwrap();
        assert!(s.contains("\"power_save_secs\":0"));
        assert!(s.contains("\"persistent_config_present\":true"));
    }
}
