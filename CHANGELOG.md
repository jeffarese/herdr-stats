# Changelog

## [0.1.0] - 2026-10-24

First release.

- Live stats in the top-right corner of herdr's tab bar: cpu, gpu, memory,
  disk, temperature, power, fan, battery, network and load.
- Braille (or block) graphs and gauges, icons that change with the reading,
  and alerts for heat, memory pressure, a full disk and a low battery.
- Every field has a fixed width, and the line never runs past herdr's 80
  characters.
- Four presets (minimal, compact, balanced, full) and a config file that
  reloads on save.
- `setup` adds the stats to herdr's config and reloads it; `setup --remove`
  takes them out.
- Actions and popups to preview the presets live, edit the config, and run
  the doctor.
- Native sampling on macOS (Apple Silicon) and Linux, behind a small daemon
  that starts on demand, exits when idle and restarts itself on update.
- Prebuilt binaries for macOS and Linux on x86_64 and aarch64.
