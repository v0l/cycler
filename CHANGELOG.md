# Changelog

All notable changes to Cycler are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.2.1] - 2026-10-31

### Changed
- egui and eframe 0.37, which keeps the window running on Wayland when its workspace is hidden.

## [0.2.0] - 2026-10-14

### Added
- Windows `.msi` and macOS `.dmg` installers with the Cycler icon.
- A pack with no BMS counts its state of charge once a capacity test has measured it.

### Changed
- The panel uses the egui_bench theme shared by the other bench tools.

### Fixed
- The OWON OEL load is talked to at 115200 baud.

## [0.1.1] - 2026-09-22

### Added
- `.deb`, `.msi` and `.dmg` installers.

## [0.1.0] - 2026-09-22

First release.

### Added
- Charging through pre-charge, constant current and constant voltage, stopping when the current
  tails off at C/20, with `standard`, `top-balance` and `bulk-only` modes. Lead-acid goes on to
  float; lithium stops.
- Discharge in CC, CV, CR or CP, stopping on the first cell to reach its floor.
- Stop at a state of charge in either direction, for storage.
- Cycle plans of charge, rest and discharge that measure capacity against the pack's rating.
- Pack profiles by chemistry, series and parallel. The BMS is trusted for what it knows, and a
  pack with no BMS runs on voltage limits, with its state of charge estimated from chemistry.
- Stops on pack alarms and on cold, and arms the supply only while a plan is running.
- Pylontech console, OWON SPE/SP/SPS supplies, OWON OEL loads and the Atorch DL24, each behind
  its own device trait so other hardware fits in.
- Every sample logged to CSV.
- `cycler` on the command line and `cycler-ui` with stage rails and a cell comb, which
  `--demo` plays back from recorded data.

[Unreleased]: https://github.com/v0l/cycler/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/v0l/cycler/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/v0l/cycler/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/v0l/cycler/releases/tag/v0.1.0
