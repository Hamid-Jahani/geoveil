# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-10-14

Nothing has been released yet. The workspace version is `0.1.0`. This section
describes phases 1, 2, 5 and 8 of [docs/design.md](docs/design.md).

### Added

#### Core (`geoveil-core`)

- `Profile` type: latitude, longitude, accuracy, IANA timezone, BCP-47 locale,
  ISO 3166 country code and source.
- `Backend` trait: `snapshot`, `apply`, `restore`, `verify`.
- IP geolocation through four free, key-less providers (ipwho.is, ipapi.co,
  ip-api.com, ipinfo.io), queried concurrently with a per-request timeout
  (default 5 s).
- The public IP is found first (IPv4 preferred) and every provider then
  geolocates that exact IP.
- Majority vote on country. At least two providers must agree. Answers about
  different IPs are never pooled. Coordinates come from the medoid of the
  agreeing answers (a point a provider actually reported).
- Timezone from coordinates using offline boundary data (`tzf-rs`). Falls
  back to the providers' timezone, then to UTC.
- Embedded country-to-locale table and IANA-to-Windows timezone map (CLDR
  `windowsZones`).
- VPN detection: pure state machine that classifies interfaces (WireGuard,
  Wintun, OpenVPN, TAP-Windows, `wg*`, `tun*`, `tap*`, `utun*`, `ppp*`,
  `ipsec*` and common commercial adapters) and emits up / changed / down
  events. Overlay networks (Tailscale, ZeroTier) and virtual adapters are
  ignored.
- Orchestrator that snapshots every backend and writes the snapshot to disk
  before it applies anything. A backend that cannot be snapshotted is never
  applied. When already engaged, the original snapshot is kept. Restores run
  in reverse order, and the snapshot file is deleted only after every restore
  succeeded.
- Crash-safe snapshot store (`state.json`): temp file, fsync, rename.

#### OS backends (`geoveil-platform`)

- All commands run through a `CommandRunner` trait. `SystemRunner` kills a
  process after 120 s. `FakeRunner` records commands for tests.
- Windows:
  - Timezone via `tzutil /g` and `tzutil /s`. "Set time zone automatically"
    (`tzautoupdate` service) is switched off while engaged and restored
    afterwards (best effort; needs admin).
  - Locale (opt-in, `--locale`) via PowerShell `Set-Culture` and
    `Set-WinHomeLocation`, per user. Custom regional format overrides under
    `HKCU\Control Panel\International` are saved and restored. Values passed
    to PowerShell are validated or base64-encoded so they cannot inject code.
- Linux:
  - Timezone via `timedatectl set-timezone`. GNOME automatic timezone is
    switched off while engaged and restored afterwards (best effort).
  - Locale (opt-in) via `localectl set-locale`. A locale that is not
    installed on the system is reported as unsupported.
  - Location (opt-in, `--location`) via the geoclue static source:
    `/etc/geolocation` plus a drop-in in `/etc/geoclue/conf.d/` that disables
    the other sources. Requires geoclue 2.7 or newer.
- macOS:
  - Timezone via `systemsetup -settimezone` (needs root). "Set time zone
    automatically" is switched off while engaged and restored afterwards
    (best effort).
  - Locale (opt-in) via `defaults write -g AppleLocale`. Under `sudo`, the
    change is made for the invoking user. `AppleLanguages` is not touched.
  - Location (opt-in) turns Location Services off while engaged. It blocks,
    it does not spoof.

#### CLI (`geoveil`)

- `geoveil detect`: show interfaces, detected VPN interfaces and the resolved
  profile. Changes nothing.
- `geoveil apply`: engage once, from the public IP, from `--ip`, or from
  manual `--lat` / `--lon` (optional `--accuracy`).
- `geoveil restore`: put the saved original settings back.
- `geoveil status`: show whether a profile is engaged, verify each backend and
  list VPN interfaces.
- `geoveil run`: daemon that follows the VPN. Polls every `--interval-secs`
  (default 5) and rechecks the public IP every 60 s while a VPN is up.
  Restores on VPN down and on stop unless `--sticky` is set.
- Global flags: `--json`, `--locale`, `--location`, `--state-dir`,
  `-v` / `-vv` / `-vvv`, `--provider-timeout-secs`.
- Exit codes: 0 success, 1 failure, 2 some backends failed, 3 `leaks` found
  a leak.

#### Leak checks (phase 5)

- `geoveil leaks [--json] [--active-dns] [--stun HOST:PORT]... [--no-stun]
  [--no-ipv6]`: read-only report with one verdict per check (`PASS`, `INFO`,
  `N/A`, `WARN`, `LEAK`, `ERROR`). Exit code 3 when a leak is found, 1 when a
  check could not run and no leak was found.
- IPv6: IPv6-only endpoints (`api6.ipify.org`, `ipv6.icanhazip.com`) with
  proxies off; a non-VPN global address of this machine (or its /64) while a
  VPN is up is a leak.
- WebRTC: hand-written STUN Binding codec (RFC 8489, FINGERPRINT checked,
  tested against the RFC 5769 vectors) and UDP probes bound to each physical
  interface with a gateway plus the default route (`IP_UNICAST_IF` /
  `IPV6_UNICAST_IF` on Windows, `SO_BINDTOIFINDEX` on Linux, `IP_BOUND_IF` on
  macOS), retried after 500, 1000 and 2000 ms.
- DNS: resolver discovery per interface (`Get-DnsClientServerAddress` on
  Windows, `resolvectl status` with a `/etc/resolv.conf` fallback on Linux,
  `scutil --dns` on macOS). Public resolvers on non-VPN interfaces are a
  leak, LAN resolvers a warning.
- `--active-dns` (opt-in): Mullvad's unique-subdomain DNS leak test.
- While engaged, the orchestrator's verify results are included (a mismatch
  is a warning, never a leak).

#### Safety

- Exclusive lock (`geoveil.lock`) on the state directory for `apply`,
  `restore` and `run`, so two processes cannot overwrite the saved originals.
- State directory selection that finds a snapshot written with or without
  `sudo` (`/var/lib/geoveil` on Linux, `/var/db/geoveil` on macOS, otherwise
  the per-user data directory).
- Daemon crash recovery: a leftover snapshot is restored at startup when no
  VPN is up.
- Daemon retries failed applies and restores with exponential backoff
  (30 s up to 10 min).
- Daemon stops and restores on Ctrl-C, SIGTERM and SIGHUP, and on Windows
  console close, logoff and shutdown events. A failed final restore is
  reported in the exit code.
- Windows sign-out and shutdown: `geoveil run` owns a hidden top-level window
  that sets a shutdown block reason on `WM_QUERYENDSESSION` and, on
  `WM_ENDSESSION`, stops the daemon and waits up to 20 s for the restore. A
  cancelled sign-out stops nothing. This covers daemons without a console
  (started by the tray), where the console logoff / shutdown events are not
  delivered.

#### Tray, IPC and start at login (phase 8)

- `geoveil run` serves local IPC (new `geoveil-ipc` crate): versioned JSON
  lines (`status`, `restore_now`, `pause`, `resume`, `shutdown`) over a named
  pipe on Windows (protected DACL for the current user, first instance only,
  remote clients rejected) or a Unix socket elsewhere (mode 0600). Both ends
  refuse peers running as another user. The status includes engaged, paused,
  profile, VPN interfaces and the last error.
- `geoveil-tray` (new crate, needs Rust 1.90): tray icon with status,
  country / time zone, Pause / Resume, Restore now, Open log folder and Quit.
  Starts `geoveil run` with the saved options when no daemon answers, and
  stops it again on Quit. Linux uses the KSNI backend (no GTK).
- `geoveil autostart enable|disable|status`: per-user login entry for the
  tray (Windows HKCU Run key via `reg.exe`, Linux `systemd --user` unit,
  macOS LaunchAgent). `enable` saves `--locale`, `--location` and
  `--state-dir` for the tray-started daemon.

#### Project

- GitHub Actions CI on Windows, Ubuntu and macOS: `cargo build`,
  `cargo test` and `cargo clippy -D warnings`.
- Release workflow on `v*` tags: builds `geoveil` and `geoveil-tray` for
  Windows x86_64, Linux x86_64 and macOS x86_64 / arm64, publishes zip /
  tar.gz archives and `SHA256SUMS`. The binaries are not code-signed.

### Not done

- Windows location spoofing (phases 3 and 6) and the browser extension
  bridge (phase 4) are not being pursued. `--location` on Windows only logs a
  warning.
- macOS location stays block-only (phase 7, expert mode, is not pursued).
- `geoveil status` does not query a running daemon over IPC.
- Code signing of release binaries.

[Unreleased]: https://github.com/Hamid-Jahani/geoveil/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Hamid-Jahani/geoveil/releases/tag/v0.1.0
