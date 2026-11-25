# laptop-tools

Two small Rust user daemons for a Linux laptop (built on Fedora with GNOME, systemd and UPower).
Each one runs as a `systemctl --user` service and talks to the system D-Bus.

| Tool | What it does |
|---|---|
| [powerwatch](powerwatch/) | When you unplug, switches to the power-saver profile, stops chosen Docker containers and turns on Wi-Fi powersave. When you plug back in, it puts everything back the way it was. Also warns about low battery and about a flapping charger. |
| [resumecheck](resumecheck/) | After the laptop wakes from suspend, it checks for problems that tend to follow a resume: DNS and connectivity, the fingerprint reader, kernel USB/driver errors, failed systemd units, and new OpenSnitch deny rules. If anything is wrong it sends one notification with a suggested fix for each issue. |

## Build

    cargo build --release
    cargo test --workspace

Binaries land in `target/release/`. See each tool's README for config and service install.

## License

MIT
