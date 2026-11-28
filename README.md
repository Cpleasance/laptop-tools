# ThinkPad PowerWatch 🔋 — Rust Daemons for Linux Battery Scaling (EPP) & Sleep Recovery

[![CI](https://github.com/Cpleasance/laptop-tools/actions/workflows/ci.yml/badge.svg)](https://github.com/Cpleasance/laptop-tools/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust: 2021](https://img.shields.io/badge/Rust-2021_Edition-orange?logo=rust)](https://www.rust-lang.org)
[![Platform: Linux](https://img.shields.io/badge/Platform-Linux-blue?logo=linux)](https://github.com/Cpleasance/laptop-tools)

A unified suite of high-performance, unprivileged Rust daemons for Linux laptops (engineered for Lenovo ThinkPad running Fedora/GNOME/systemd, but portable across all modern Linux distributions). Fixes aggressive 400 MHz EPP throttling on battery unplug, sleep resume driver drops, and Li-ion charging cycle wear.

All tools run unprivileged in user space, support `--json` for integration with desktop status bars (Waybar, Polybar, GNOME Shell extensions), and honor the standard `NO_COLOR` specification.

| Tool | What it does |
|---|---|
| [powerwatch](powerwatch/) | When unplugged, tunes CPU energy profile to `balanced` (preventing 400 MHz starvation), sheds heavy containers under high power draw budgets, preserves Wi-Fi latency, detects Caffeine/media idle inhibitors, and reports live power draw (W), battery health, and Intel RAPL power limits ($PL1/PL2$). |
| [resumecheck](resumecheck/) | Post-suspend health daemon with systemd sleep delay inhibitor lock: checks DNS, fingerprint sensor, kernel dmesg errors, failed systemd units (auto-resetting stubborn services like `open-fprintd`), Bluetooth radio state, and OpenSnitch firewall deny rules. |
| [thresholdctl](thresholdctl/) | Instant battery charge threshold manager: switch between Desk Profile (`75% start / 80% stop` for quadrupled battery cycle life) and Travel Profile (`95% start / 100% stop` for full trips), with udev rule generator for passwordless operation. |
| [thermalwatch](thermalwatch/) | Real-time thermal monitor, ThinkPad fan control (`auto`, `0-7`, `full-speed`, `disengaged`), and CPU throttling root-cause diagnostic (differentiating thermal limits, RAPL power constraints, and EPP profiles). |
| [dockwatch](dockwatch/) | Thunderbolt 3 & USB-C dock hotplug daemon: tracks external displays (resolutions and connection states), monitors Type-C data/power negotiation, tracks default PipeWire audio sinks, and runs custom user hooks on dock/undock. |
| [audiopop-fix](audiopop-fix/) | Diagnostic and remediation tool for Intel HDA audio codec sleep: eliminates annoying headphone capacitor charging clicks/pops caused by 1-second aggressive codec sleep (`power_save=1`), with instant sysfs live-switching and modprobe configuration generator. |

## Build & Install

```bash
make              # cargo build --release (all 6 tools)
make test         # run unit tests across all workspace crates
make status       # inspect full laptop diagnostic across all 6 tools
make check-config # validate daemon configurations
make install      # install binaries to ~/.local/bin and systemd user services
```

Binaries land in `target/release/`. See each tool's directory for dedicated documentation and configuration options.

## License

MIT
