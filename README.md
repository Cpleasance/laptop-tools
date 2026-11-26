# laptop-tools

Two small Rust user daemons for a Linux laptop (built on Fedora with GNOME, systemd and UPower).
Each one runs as a `systemctl --user` service and talks to the system D-Bus.

| Tool | What it does |
|---|---|
| [powerwatch](powerwatch/) | When you unplug, switches to the balanced profile, stops chosen Docker containers, and manages Wi-Fi powersave. Can enforce a power draw budget in Watts by shedding heavy containers on high battery drain. Warns on low battery and flapping charger, and reports battery health & real-time power draw in `status`. |
| [resumecheck](resumecheck/) | After the laptop wakes from suspend, checks for problems that tend to follow a resume: DNS and connectivity, the fingerprint reader, kernel USB/driver errors, failed systemd units (with auto-reset for stubborn services), Bluetooth radio status, and new OpenSnitch deny rules. Includes `status`, `history`, and `check-config` CLI commands. |

## Build & Install

    make              # cargo build --release
    make test         # cargo test --workspace
    make status       # inspect powerwatch & resumecheck status
    make check-config # validate both configurations
    make install      # install binaries to ~/.local/bin and systemd user services

Binaries land in `target/release/`. See each tool's README for detailed config options.

## License

MIT
