# powerwatch

User-level daemon that watches UPower `OnBattery` (system D-Bus) and, after a 5 s debounce:

- **On battery:** sets the power profile (default `power-saver`, previous profile remembered),
  stops the configured Docker containers (remembered in `$XDG_STATE_HOME/powerwatch/state.toml`),
  enables Wi-Fi powersave on the active Wi-Fi connection via `nmcli` (skipped with a log line if polkit denies),
  runs `on_battery_cmds`, and sends a desktop notification.
- **On AC:** restores the previous profile, starts only the containers it stopped (reverse order),
  restores Wi-Fi powersave, runs `on_ac_cmds`, notifies.

Each action is independent; failures are logged to stderr (journald) and never stop the loop.

## Usage

    cargo build --release -p powerwatch
    powerwatch --dry-run --once   # log what would happen, change nothing
    powerwatch --once             # apply current state and exit
    powerwatch                    # run as daemon

## Config

`~/.config/powerwatch/config.toml` (created on first run):

    containers = ["prefect-server", "prefect-postgres"]
    battery_profile = "power-saver"
    wifi_powersave = true
    notify = true
    on_battery_cmds = []
    on_ac_cmds = []

## Install (user service)

    cp powerwatch.service ~/.config/systemd/user/   # adjust ExecStart path if needed
    systemctl --user daemon-reload
    systemctl --user enable --now powerwatch
    journalctl --user -u powerwatch -f
