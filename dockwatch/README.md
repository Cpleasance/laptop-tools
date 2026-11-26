# dockwatch

Thunderbolt & USB-C dock, external display, and audio hotplug monitor for Linux laptops.

## Overview

When connecting or disconnecting a Thunderbolt 3 or USB-C dock, laptops frequently experience:
- Screen resolution or scaling inconsistencies on Wayland.
- Audio remaining trapped on disconnected dock sinks rather than restoring internal speakers/headphones.
- The need to launch or kill external monitor background utilities.

`dockwatch` monitors DRM connectors and Type-C status, reporting whether the machine is docked or mobile, and executes automated lifecycle hooks (`~/.config/dockwatch/on-dock.sh` and `on-undock.sh`).

## Features

- **Display Connectors:** Tracks eDP internal and external DisplayPort / HDMI connectors and resolutions.
- **Type-C State:** Reports USB Type-C port data and power negotiation states.
- **Audio Sink Tracking:** Identifies PipeWire / WirePlumber default audio routing.
- **Event Hooks:** Runs user scripts on dock and undock events.
- **CLI & Daemon:** Can be queried as a CLI tool (`dockwatch status`) or run as a background service (`dockwatch watch`).

## Usage

```bash
# Check current dock and monitor status
dockwatch status
dockwatch status --json

# Trigger hooks based on current state
dockwatch run-hooks

# Run monitoring loop
dockwatch watch
```
