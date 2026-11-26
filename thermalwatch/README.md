# thermalwatch

Fast thermal monitoring, ThinkPad fan control, and CPU throttling diagnostics for Linux laptops.

## Overview

Modern ultrabooks like the ThinkPad X1 Carbon Gen 5 can experience multiple distinct kinds of performance throttling:
1. **Thermal Throttling:** CPU package temperatures reach >90°C and drop clock speeds to protect silicon.
2. **RAPL Power Capping:** Running into hardware Power Limit 1 (PL1) or Power Limit 2 (PL2) limits (e.g. 15W sustained power cap).
3. **EPP Energy Preference Throttling:** When GNOME or systemd profile locks EPP to `power`, capping CPU clocks to 400-800 MHz.

`thermalwatch` diagnoses exactly *which* of these is occurring and allows ThinkPad fan control without third-party bloat.

## Features

- **Component Thermals:** Reports CPU Package, Core 0, Core 1, NVMe SSD, and Intel Wi-Fi chip temperatures in real-time.
- **ThinkPad Fan Control:** Direct readout and adjustment of fan speed (RPM) and modes (`auto`, `0-7`, `full-speed`, `disengaged`).
- **Throttling Diagnostics:** Analyzes CPU frequencies, RAPL power constraints, and temperatures to output an instant diagnostic verdict.
- **Universal Format:** Supports `--json` for Waybar / Polybar / script integration and respects `NO_COLOR`.

## Usage

```bash
# Check current thermals, fan RPM, and sensor readings
thermalwatch status
thermalwatch status --json

# Run instant CPU throttling diagnostic
thermalwatch diagnose
thermalwatch diagnose --json

# Set fan mode (ThinkPad ACPI)
thermalwatch fan auto
thermalwatch fan 7
thermalwatch fan disengaged
```
