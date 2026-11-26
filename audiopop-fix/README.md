# audiopop-fix

Diagnostic and remediation tool for Intel HDA audio power-save click and pop noises on Linux laptops.

## Overview

On Linux laptops using Intel HD Audio (`snd_hda_intel`), the kernel's aggressive power-saving feature puts the audio codec into `D3hot` low-power sleep after 1 second of inactivity (`power_save=1`).

When an application plays audio or a notification chimes, waking up the analog digital-to-analog converter (DAC) charges output capacitors, causing an audible and annoying "pop" or "click" sound in headphones and external speakers.

`audiopop-fix` diagnoses this configuration, tests audio output, and provides both instant sysfs remediation and persistent configuration.

## Features

- **Live Diagnostic:** Checks current sysfs values for `power_save` and `power_save_controller`.
- **Modprobe Verification:** Checks if a persistent drop-in file is active in `/etc/modprobe.d/`.
- **Instant Fix:** Sets parameters to `0` / `N` on the fly to eliminate pops without rebooting.
- **Tone Test:** Quickly tests audio output using PipeWire `pw-play`.
- **Universal:** Supports `--json` and respects `NO_COLOR`.

## Usage

```bash
# Check current power-save status and pop risk
audiopop-fix status
audiopop-fix status --json

# Disable sleep timeout to eliminate clicks/pops
audiopop-fix disable-pop

# Make configuration permanent across reboots
audiopop-fix generate-conf | sudo tee /etc/modprobe.d/audio_powersave.conf

# Re-enable standard power-saving if desired
audiopop-fix enable-powersave

# Test audio output
audiopop-fix test-tone
```
