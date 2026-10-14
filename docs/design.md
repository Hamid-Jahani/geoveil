# Geoveil — system-wide location consistency with your VPN

Date: 2026-10-14
Status: approved (phase 1 in progress)

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

A Tauri tray UI comes in a later phase. It talks to the daemon over local IPC.

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
- Majority vote on country; take lat/lon as the median of agreeing providers.
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
| Location | phase 3 (NMEA virtual port), phase 6 (UMDF driver) | geoclue static source `/etc/geolocation` + disable other sources in `/etc/geoclue/conf.d/` | Location Services off (block only); expert mode later |

All backends shell out via a `CommandRunner` trait so they are testable with a
fake runner that records the commands it would run.

### CLI

```
geoveil detect            show VPN interfaces + exit IP + resolved profile
geoveil apply [--lat --lon | --ip X] [--locale]   engage once
geoveil restore           restore snapshot
geoveil status            current state + verify report
geoveil run [--locale]    daemon: follow VPN up/down/changed automatically
geoveil leaks             phase 5
```

## Error handling

- VPN down → disengage (default) or keep + warn (`--sticky`).
- Providers disagree → majority; no majority → refuse to auto-apply, report.
- Crash → snapshot on disk; `restore` or next `run` restores.
- Backend needs privileges it lacks → error names the backend and the
  privilege; other backends still apply.

## Testing

- Core: unit tests (vote/median, tz lookup, locale map, vpn detection state
  machine, orchestrator with fake backends, snapshot round-trip).
- Platform: unit tests with fake `CommandRunner` asserting exact commands per OS.
- CI: GitHub Actions matrix windows/macos/ubuntu: `cargo test`, `cargo clippy`.

## Phases

1. Core + VPN detect + geo resolve + timezone/locale backends on 3 OSes + CLI  ← now
2. Linux geoclue location backend                                            ← now
3. Windows NMEA virtual-port location backend
4. Browser extension bridge (native messaging, push Profile)
5. Leak checks (DNS, IPv6, WebRTC/STUN)
6. Windows UMDF GNSS sensor driver
7. macOS expert mode
8. Tauri tray UI
