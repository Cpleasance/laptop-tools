# resumecheck

User-level daemon that listens for logind `PrepareForSleep` on the system bus.
On resume it waits `delay_secs` (15) and runs independent, time-bounded checks:

| Check | What |
|---|---|
| opensnitch | `/etc/opensnitchd/rules/deny-*` newer than suspend time, or not in the persisted known set (`$XDG_STATE_HOME/resumecheck/known_deny_rules`). App path decoded from filename (lossy: `-` vs `/`). First run baselines silently. |
| dns | Resolves `dns_hosts` with retries up to `dns_window_secs` (60); fails if none resolve. Also `nmcli` connectivity none/limited. |
| fingerprint | `fprintd-list $USER`, 10s timeout (skipped if not installed). |
| kernel | `journalctl -k --since=...` for `HC died`, `not responding, assume dead`, `Unable to change power state`, `Call Trace`, `BUG:` (deduped, max 5 shown; `USB disconnect` ignored). |
| units | `systemctl --failed` and `systemctl --user --failed`. Configured `auto_reset_failed_units` are automatically reset. |
| bluetooth | `rfkill list bluetooth` to catch soft-blocked Bluetooth controllers after resume. |

Any issue -> one desktop notification (session bus) with a fix per issue. All OK -> log only
unless `notify_ok = true`. Every run appends a line to `$XDG_STATE_HOME/resumecheck/history.log`.

## Usage

    resumecheck                       # daemon
    resumecheck status                # show active checks, config path, and resume stats
    resumecheck history [--limit 15]  # view chronological log of past resumes and issues
    resumecheck check-config          # validate configuration without running checks
    resumecheck --now                 # run once, print results (kernel log since 1h ago)
    resumecheck --now --since "-4h"   # any journalctl time spec

In daemon mode the kernel window starts at the suspend time (resume time minus 10s if unknown).
`--now` does not update the known-deny-rule set.

## Config

`~/.config/resumecheck/config.toml` is created with defaults on first run: `delay_secs`, `notify_ok`,
`dns_hosts`, `dns_window_secs`, `check_opensnitch`, `check_dns`, `check_fingerprint`, `check_kernel`,
`check_failed_units`, `check_bluetooth`, `auto_reset_failed_units`.

## Install (manual)

    cargo build --release -p resumecheck
    install -Dm755 ../target/release/resumecheck ~/.local/bin/resumecheck
    install -Dm644 resumecheck.service ~/.config/systemd/user/resumecheck.service
    # edit ExecStart to %h/.local/bin/resumecheck, then:
    systemctl --user daemon-reload && systemctl --user enable --now resumecheck
