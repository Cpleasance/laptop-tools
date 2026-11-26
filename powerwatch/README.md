# powerwatch

User-level daemon that watches UPower `OnBattery` (system D-Bus) and, after a 5 s debounce:

- **On battery:** sets the power profile (default `balanced`, previous profile remembered),
  stops the configured Docker containers (remembered in `$XDG_STATE_HOME/powerwatch/state.toml`),
  optionally enables Wi-Fi powersave on the active Wi-Fi connection via `nmcli` (skipped with a log line if polkit denies),
  runs `on_battery_cmds`, and sends a desktop notification.
- **On AC:** restores the previous profile, starts only the containers it stopped (reverse order),
  restores Wi-Fi powersave, runs `on_ac_cmds`, notifies.

Each action is independent; failures are logged to stderr (journald) and never stop the loop.
If an AC-side restore fails, powerwatch retries outstanding restores every 30 seconds while AC
remains connected. Configured extra commands run via `sh -c`; only use commands you trust.
Run `powerwatch status` to inspect current power state, real-time power draw (Watts) and time
estimates, battery health/cycles, charge thresholds, CPU scaling/EPP status, pending restores,
and recent stable transitions. Configuration can be hot-reloaded on the fly by sending `SIGHUP`
(`systemctl --user reload powerwatch`). Repeated AC/battery changes trigger a notification at four
changes in five minutes, throttled to at most once every 30 minutes.

## Usage

    cargo build --release -p powerwatch
    powerwatch --dry-run --once   # log what would happen, change nothing
    powerwatch --once             # apply current state and exit
    powerwatch status             # power state, wattage, battery health, CPU EPP/freq, and transitions
    powerwatch check-config       # validate configuration without applying it
    powerwatch                    # run as daemon

## Config

`~/.config/powerwatch/config.toml` (created on first run):

    containers = ["prefect-server", "prefect-postgres"]
    battery_profile = "balanced"
    wifi_powersave = false
    notify = true
    on_battery_cmds = []
    on_ac_cmds = []
    low_battery_threshold = 15
    on_low_battery_cmds = []
    power_budget_watts = 0.0
    heavy_containers = []
    on_high_power_cmds = []

`battery_profile` defaults to `balanced` rather than `power-saver` to keep dual-core 15W CPUs
responsive on high-resolution displays. Wi-Fi powersave defaults to `false` to avoid packet
latency jitter.

`power_budget_watts` (optional, default 0 to disable): If power draw exceeds this budget on battery,
`heavy_containers` are temporarily shed and `on_high_power_cmds` run until power draw recovers or
AC is plugged back in.

`low_battery_threshold` runs the optional commands and sends one notification per low-battery
episode; set it to `0` to disable. The alert rearms after charging or recovering at least five
percentage points above the threshold. Validate settings with `powerwatch check-config`.

## Install (user service)

    cp powerwatch.service ~/.config/systemd/user/   # adjust ExecStart path if needed
    systemctl --user daemon-reload
    systemctl --user enable --now powerwatch
    journalctl --user -u powerwatch -f
