# thresholdctl

Fast, lightweight CLI for inspecting and switching battery charge thresholds on ThinkPad and modern Linux laptops.

## Overview

Modern Linux laptops (especially Lenovo ThinkPads) support setting battery charge thresholds directly in hardware to extend battery lifespan.
- **Desk Mode (`75% start / 80% stop`):** Prevents keeping Li-ion cells constantly at 100% and high voltage while docked or plugged in, quadrupling battery longevity.
- **Travel Mode (`95% start / 100% stop`):** Charges the battery to 100% when you need maximum untethered battery life.

`thresholdctl` allows instant switching between these profiles with a single command, supports `--json` for Waybar / Polybar status bars, and includes a udev rule generator for passwordless threshold changes.

## Usage

```bash
# Check current thresholds, battery charge, and profile mode
thresholdctl status
thresholdctl status --json

# Switch to Desk Profile (75% start / 80% stop)
thresholdctl desk

# Switch to Travel Profile (95% start / 100% stop)
thresholdctl travel

# Set custom thresholds (start stop)
thresholdctl set 60 70

# Print udev rule for passwordless switching without sudo
thresholdctl generate-udev-rule
```

## Passwordless Configuration (Optional)

By default, `/sys/class/power_supply/BAT0/charge_control_*` is owned by `root`. To allow your standard user account to switch thresholds without typing `sudo`:

```bash
thresholdctl generate-udev-rule | sudo tee /etc/udev/rules.d/99-battery-thresholds.rules
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=power_supply
```
