# Geoveil — system-wide location consistency with your VPN

Date: 2026-10-14
Status: approved (phases 1, 2, 5 and 8 released in v0.1.0;
phases 3, 4 and 6 are not being pursued; phase 7 stays block-only)

## Goal

When the user's VPN connects, make the whole OS (not just the browser) report a
location consistent with the VPN exit: geolocation, timezone, locale. Restore
the real settings when the VPN drops or the app exits.

Geoveil does **not** provide the tunnel. The user's own VPN client (WireGuard,
OpenVPN, commercial apps) does that. Geoveil detects it automatically.

Inspired by the GeoSpoof browser extension (Apache-2.0). "GeoSpoof" is a
trademark of GeoSpoof LLC, so this project uses a different name.

## Architecture

Rust workspace, one binary per role:

```
crates/
  geoveil-core      pure logic, no OS calls: Profile, geo resolve, tz/locale
                    lookup, orchestrator, snapshot store, vpn detection logic
  geoveil-platform  OS backends behind traits (cfg(windows|linux|macos))
  geoveil-cli       `geoveil` binary: daemon + one-shot commands
```

Phase 8 adds `geoveil-ipc` (local IPC: versioned JSON lines over a per-user
named pipe or Unix socket) and `geoveil-tray` (a tray icon built with
`tray-icon` / `muda`, no webview). The tray is a thin client of `geoveil run`
and starts it when none answers. See the README for the menu and
`geoveil autostart`.

### Core types

- `Profile { lat, lon, accuracy_m, timezone (IANA), locale (BCP-47), country (ISO-3166 alpha-2), source }`
- `Backend` trait: `name()`, `snapshot() -> BackendSnapshot`, `apply(&Profile)`,
  `restore(&BackendSnapshot)`, `verify(&Profile) -> VerifyReport`.
- `Orchestrator`: owns a list of backends. `engage(profile)` takes a snapshot of every
  backend and saves it to disk **before** applying. `disengage()` restores
  from the saved snapshot. Applying is best effort per backend; failures are reported,
  never silently dropped.
- `SnapshotStore`: JSON file in the per-OS state dir. If one exists at
  startup, the previous run crashed while engaged → offer/perform restore.

### VPN detection

Poll every 5 s (configurable):
1. Enumerate interfaces. VPN-like = name/description matches WireGuard, wg*,
   tun*, tap*, utun*, ppp*, ipsec*, OpenVPN, "TAP-Windows", "Wintun", and known
   commercial adapters (NordLynx, ProtonVPN, Mullvad, etc.), and is up with an address.
2. When the set of VPN-like interfaces changes, or every 60 s while one is up, fetch the
   public IP. A changed public IP while a VPN interface is up → VPN exit changed.
3. Emit `VpnEvent::{Up{exit_ip}, Changed{exit_ip}, Down}`.

Detection logic is pure (takes interface snapshots + IP), so it is unit tested;
the platform crate supplies interface enumeration.

### Geo resolve

Exit IP → `Profile`:
- Query ≥3 providers concurrently (ipapi.co, ipwho.is, ip-api.com, ipinfo.io
  free endpoints), timeout 5 s each. Requests go out through the system route,
  which means through the VPN.
- Majority vote on country among answers about the same IP (IPv4 preferred; answers
  about different IPs are never pooled); take lat/lon from the medoid agreeing answer
  (a point some provider actually reported, never a per-axis median/midpoint).
- The public IP is found first (IPv4 only, falling back to any family) and every
  provider then geolocates that exact IP.
- Timezone from lat/lon using offline boundary data (`tzf-rs`), not trusting
  providers' tz strings (fallback to provider majority if lookup fails).
- Locale from country via embedded table (primary language + country, e.g.
  `DE` → `de-DE`). Locale spoofing is opt-in (matches GeoSpoof).
- Manual override: user-specified lat/lon or city skips IP lookup.

### Platform backends (phase 1)

| Backend | Windows | Linux | macOS |
|---|---|---|---|
| Timezone | `tzutil /s` with IANA→Windows ID map (CLDR windowsZones, embedded) | `timedatectl set-timezone` | `systemsetup -settimezone` |
| Locale (opt-in) | `Set-Culture`, `Set-WinHomeLocation` (per-user) | `localectl set-locale` | `defaults write -g AppleLocale` |
| Location | not spoofed (phases 3 and 6 are not being pursued) | geoclue static source `/etc/geolocation` + disable other sources in `/etc/geoclue/conf.d/` | Location Services off (block only) |

All backends shell out via a `CommandRunner` trait so they are testable with a
fake runner that records the commands it would run.

### CLI

```
geoveil detect            show VPN interfaces + exit IP + resolved profile
geoveil apply [--lat --lon | --ip X] [--locale]   engage once
geoveil restore           restore snapshot
geoveil status            current state + verify report
geoveil run [--locale]    daemon: follow VPN up/down/changed automatically
geoveil leaks [--json] [--active-dns]   IPv6 / WebRTC (STUN) / DNS leak report; changes nothing
geoveil autostart enable|disable|status start geoveil-tray at login (per user)
```

`geoveil leaks` keeps its pure logic (STUN codec, verdict rules, report) in
`geoveil-core::leaks` and its OS probes (interfaces, resolver discovery,
interface-bound UDP sockets) in `geoveil-platform::leaks`. Exit code 3 means
a leak was found. Details are in `docs/platforms.md` ("Leak checks").

On Windows, `geoveil run` also owns a hidden top-level window that answers
`WM_QUERYENDSESSION` / `WM_ENDSESSION`, so a sign-out or shutdown waits (up
to 20 s) for the final restore (`session_end`, `shutdown_win`).

## Error handling

- VPN down → disengage (default) or keep + warn (`--sticky`).
- Providers disagree → majority; no majority → refuse to auto-apply, report.
- Crash → snapshot on disk; `restore` or next `run` restores.
- Backend needs privileges it lacks → error names the backend and the
  privilege; other backends still apply.

## Testing

- Core: unit tests (vote/medoid, tz lookup, locale map, vpn detection state
  machine, orchestrator with fake backends, snapshot round-trip).
- Platform: unit tests with fake `CommandRunner` asserting exact commands per OS.
- CI: GitHub Actions matrix windows/macos/ubuntu: `cargo test`, `cargo clippy`.

## Phases

1. Core + VPN detect + geo resolve + timezone/locale backends on 3 OSes + CLI  ← implemented
2. Linux geoclue location backend                                            ← implemented
3. Windows NMEA virtual-port location backend                               ← not pursued
4. Browser extension bridge (native messaging, push Profile)                 ← not pursued
5. Leak checks (DNS, IPv6, WebRTC/STUN)                                      ← implemented
6. Windows UMDF GNSS sensor driver                                           ← not pursued
7. macOS expert mode                       ← not pursued; macOS location stays block-only
8. Tray UI + IPC + start at login + release builds                          ← implemented
