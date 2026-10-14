# Geoveil

[![CI](https://github.com/Hamid-Jahani/geoveil/actions/workflows/ci.yml/badge.svg)](https://github.com/Hamid-Jahani/geoveil/actions/workflows/ci.yml)

Geoveil makes your operating system's time zone, locale and location match your VPN exit.

> Status: early (version 0.1.0). Time zone and locale work on Windows, Linux and macOS.
> Location works on Linux (geoclue) and is block-only on macOS. Windows location is planned.

## Why

A VPN changes your IP address. It does not change anything else on your computer.
Browser-only spoofers change what one browser reports. They do not change the OS.

So a website, an app or a script can still see:

- your real **time zone** (for example through the system clock settings),
- your real **locale** and region formats (date, number and currency formats),
- your real **location** from the OS location service (Wi-Fi based, GPS).

If the IP says "Germany" and the system time zone says "America/Chicago", that mismatch is easy to spot.
Geoveil reads where your VPN exit is and sets the system-wide settings to match. When the VPN goes down, `geoveil run` puts your real settings back.

Geoveil does **not** provide a VPN and does **not** change your IP. You keep using your own VPN client (WireGuard, OpenVPN, a commercial app). Geoveil only reads the VPN's exit and adjusts system settings.

## What it does today

| OS | Time zone | Locale (opt-in, `--locale`) | Location (opt-in, `--location`) |
|---|---|---|---|
| Windows | Yes: `tzutil` | Yes: `Set-Culture`, `Set-WinHomeLocation` (per user) | Planned (phase 3, phase 6) |
| Linux | Yes: `timedatectl` | Yes: `localectl` (new sessions only) | Yes: geoclue static source (geoclue 2.7 or newer) |
| macOS | Yes: `systemsetup` | Yes: `defaults -g AppleLocale` | Block only: turns Location Services off. Real spoofing is planned (phase 7) |

The time zone is always changed. Locale and location are only changed when you pass `--locale` or `--location`.

Exact commands, files and caveats for each OS are in [docs/platforms.md](docs/platforms.md).

## How it works

1. **Detect the VPN.** Geoveil lists network interfaces and looks for VPN adapters (WireGuard, Wintun, OpenVPN, TAP, `tun0`, `wg0`, `utun3`, and known commercial adapters). Overlay networks such as Tailscale and ZeroTier are ignored.
2. **Find the public IP.** It asks plain "what is my IP" services, over IPv4 first. While the VPN is up, this is the VPN exit IP.
3. **Locate the IP.** Four free IP-geolocation services are asked about that exact IP at the same time. The winning country must have more answers than any other country (a tie means no result), and at least two services must agree. If no public IP can be found, `detect` and `apply` ask each service to locate the caller's own address instead. The coordinates are taken from one real answer (the one closest to the other agreeing answers), never an average.
4. **Find the time zone offline.** The time zone comes from the coordinates, using time zone boundary data built into the binary. The services' own time zone strings are only a fallback. The locale comes from the country (for example `DE` becomes `de-DE`).
5. **Save the originals.** Before changing anything, Geoveil reads your current settings and writes them to a state file on disk.
6. **Apply.** Each setting is changed. A failure in one does not stop the others. Every result is reported.
7. **Restore.** When the VPN goes down, when you stop the daemon, or when you run `geoveil restore`, the saved settings are put back. If Geoveil crashed, the state file is still on disk, and the next `geoveil run` or `geoveil restore` uses it.

## Install

There are no prebuilt binaries yet. Build from source.

### Requirements

- Rust stable, 1.88 or newer ([rustup.rs](https://rustup.rs)).
- A C compiler and linker for your platform (one dependency, `ring`, compiles C code):
  - **Windows:** Microsoft C++ Build Tools ("Desktop development with C++" workload), or Visual Studio with that workload.
  - **Linux:** `gcc` or `clang` (for example `sudo apt install build-essential` on Debian/Ubuntu).
  - **macOS:** Xcode Command Line Tools (`xcode-select --install`).

### Build

```sh
git clone https://github.com/Hamid-Jahani/geoveil.git
cd geoveil
cargo build --release
```

The binary is:

- `target/release/geoveil` on Linux and macOS,
- `target\release\geoveil.exe` on Windows.

Copy it somewhere on your `PATH` if you like.

## Quick start

Connect your VPN first. Then:

```sh
# See what Geoveil would do. Changes nothing.
geoveil detect

# Match the system to the current VPN exit, once.
geoveil apply

# Show what is applied and check that the OS really reports it.
geoveil status

# Put your original settings back.
geoveil restore
```

`detect` lists every interface, marks the ones it treats as VPN, and shows the resolved profile:

```text
interfaces:
  VPN up    wg0  [10.2.0.2]
      up    eth0  [192.168.1.20, fe80::1]
VPN interfaces: wg0
profile:
location  52.5200, 13.4050 (±5000 m)
country   DE
timezone  Europe/Berlin
locale    de-DE
source    VPN exit 203.0.113.7
```

If no VPN interface is found, `detect` warns you that the location is probably your real one. Note that `apply` does not check for a VPN. It uses whatever your public IP is.

### Follow the VPN automatically

```sh
geoveil run
```

The daemon checks the interfaces every 5 seconds (`--interval-secs N`). It also re-checks the public IP every 60 seconds while a VPN is up. Then:

- VPN comes up: it looks up the exit and applies the profile.
- VPN exit changes: it applies the new profile. The original settings saved the first time are kept.
- VPN goes down: it restores the original settings.
- You press Ctrl-C, or the process gets SIGTERM/SIGHUP (Linux, macOS) or a console close, logoff or shutdown event (Windows): it restores and exits.

If an apply or restore fails, the daemon retries after 30 seconds, then doubles the wait each time, up to 10 minutes.

`--sticky` keeps the spoofed settings when the VPN goes down or the daemon stops. It only warns. Your real location can then leak while the VPN is off. Run `geoveil restore` to undo.

### Choose the location yourself

```sh
# Locate a specific IP instead of your current public IP.
geoveil apply --ip 203.0.113.7

# Use coordinates. No network requests are made.
geoveil apply --lat 52.52 --lon 13.405

# Coordinates with a reported accuracy in metres (default 100).
geoveil apply --lat 52.52 --lon 13.405 --accuracy 50
```

With `--lat/--lon` there is no country, so the locale is `en-US`. Use `--ip` if you also want a matching locale.

### Global options

These work with every command, before or after the command name.

| Option | Meaning |
|---|---|
| `--locale` | Also change the system locale / region formats. Off by default. |
| `--location` | Also drive the OS location service (Linux: geoclue; macOS: turn Location Services off; Windows: not yet available, a warning is printed). |
| `--json` | Print JSON on stdout (`run` only logs). Logs stay on stderr. |
| `--state-dir PATH` | Where the state file and lock file live. See [Safety](#safety). |
| `--provider-timeout-secs N` | Timeout for each public-IP and geolocation request, 1 to 120 seconds. Default 5. |
| `-v`, `-vv`, `-vvv` | More log output (info, debug, trace). `RUST_LOG` overrides this. |

**Use the same flags for `restore` that you used for `apply`.** A setting can only be restored when its backend is enabled. If you applied with `--locale`, restore with `geoveil --locale restore`. Otherwise the locale part is reported as failed, the state file is kept, and you can run the command again with the right flag.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. `restore` with nothing to restore also returns 0. |
| 1 | Error (for example no geolocation consensus, or another Geoveil holds the lock). |
| 2 | Partial: some settings were applied or restored, others failed. |

## Privileges

| OS | Without admin/root | Needs admin/root |
|---|---|---|
| Windows | Time zone (`tzutil /s`). Locale (per user). | Turning off "Set time zone automatically" (a registry value under `HKLM`). Without admin this step is skipped with a warning, and Windows may later put your real time zone back. |
| Linux | Nothing that changes the system, unless polkit allows your user. | Time zone (`timedatectl`) and locale (`localectl`) need root or polkit approval. Location writes files in `/etc` and restarts geoclue, so it needs root. |
| macOS | Locale (`defaults -g`, per user). | Time zone (`systemsetup`) and the automatic time zone switch. Location (`/var/db/locationd`). |

`detect` and `status` change nothing and need no special rights on Windows and Linux. On macOS, run `status` with `sudo` to check the time zone and location. Without root, those lines show as DIFF.

On macOS, if you run with `sudo`, the locale change is still made for you (the user who ran `sudo`), not for root.

If a setting needs a right you do not have, the error names the setting and the problem. The other settings still apply.

## Safety

- **Snapshot before apply.** Your current settings are written to disk before anything is changed. A setting that cannot be read is never changed.
- **Originals are kept.** When the VPN exit changes while engaged, the first snapshot is kept. Spoofed values are never saved as "originals".
- **Restore is retried.** Settings are restored in reverse order. The state file is only deleted when every setting was restored. If something failed, run `geoveil restore` again.
- **Single instance.** `apply`, `restore` and `run` take an exclusive lock (`geoveil.lock` in the state directory). A second one stops with "another geoveil process is running". The OS releases the lock if the process dies.
- **Automatic time zone is handled.** The OS "set time zone automatically" feature would undo the change. While engaged, Geoveil turns it off and later restores its original value: the `tzautoupdate` service on Windows (needs admin), GNOME's `automatic-timezone` setting on Linux, and `com.apple.timezone.auto` on macOS. `status` reports it if it is still on.
- **No shell.** Every external command is started directly with separate arguments. Values from the state file are validated before use. A command that runs longer than 120 seconds is killed.

### Where the state file lives

The state file is `state.json` inside the state directory. Without `--state-dir`:

| OS | Normal user | Running as root |
|---|---|---|
| Windows | `%LOCALAPPDATA%\geoveil` | (same) |
| Linux | `$XDG_DATA_HOME/geoveil`, usually `~/.local/share/geoveil` | `/var/lib/geoveil` |
| macOS | `~/Library/Application Support/geoveil` | `/var/db/geoveil` |

On Linux and macOS, Geoveil first looks for an existing state file in the system directory, then your own data directory, then (under `sudo`) the data directory of the user who ran `sudo` (`/home/<user>/.local/share/geoveil`, or `/Users/<user>/Library/Application Support/geoveil` on macOS; a custom `XDG_DATA_HOME` is not followed). So a plain `geoveil restore` finds a state that was written with `sudo geoveil apply`. A normal user can read root's state (`status` works) but cannot take its lock. Run `restore` the same way you ran `apply`.

`geoveil status` prints the state file path in its JSON output (`state_file`).

### How to recover

1. Run `geoveil restore` with the same flags (`--locale`, `--location`, `--state-dir`, and `sudo` if used) as the original `apply` or `run`.
2. If it reports failures, fix the cause (often a missing privilege) and run it again. The state file is kept until everything is restored.
3. If a daemon is still running, stop it first (Ctrl-C). It restores on exit unless `--sticky` was used.

## Privacy

Geoveil sends no telemetry. The only network requests are:

- **Public IP lookup:** `https://api.ipify.org`, `https://icanhazip.com`, `https://ifconfig.me/ip`.
- **IP geolocation:** `https://ipwho.is`, `https://ipapi.co`, `http://ip-api.com`, `https://ipinfo.io` (free endpoints, no API key). The IP being located is part of the request URL.

ip-api.com's free tier only supports plain **HTTP**. When the VPN carries your traffic, the request goes through the tunnel, but it is not encrypted between the VPN exit and ip-api.com.

All requests use the normal system route. When your VPN carries the default route, they go through the VPN, so the services see the VPN exit IP, not your real one. Requests carry a `geoveil/<version>` User-Agent.

`apply --lat/--lon` makes no network requests at all. `status` makes none either. `apply --ip X` sends X to the four geolocation services and skips the public IP lookup.

## Limitations

- Windows location is not spoofed yet. `--location` on Windows prints a warning and does nothing.
- macOS location is only blocked, not spoofed: Location Services is turned off. On recent macOS versions, System Integrity Protection (SIP) may keep this from taking effect until a reboot, or at all.
- Linux location only affects apps that use geoclue (GNOME, KDE, Firefox with the geoclue provider, and so on). It needs geoclue 2.7 or newer.
- Linux locale changes only affect sessions started after the change. Log out and back in.
- On Linux, `localectl` refuses locales that are not installed. Geoveil checks `locale -a` first and tells you how to install the missing one.
- Running apps may need a restart to see a new locale (Windows, macOS).
- Windows locale changes the regional format and home location only. The input language list (keyboard layouts) is never touched.
- Some countries have no known Windows GeoID in the built-in table. Then only the regional format is changed.
- No leak checks yet. DNS, IPv6 and WebRTC leaks are not detected (planned, phase 5).
- Geoveil does not change what your browser reports through its own APIs. A browser extension bridge is planned (phase 4).
- VPN detection uses adapter names. An unusual VPN adapter name may not be recognized. Run `geoveil detect` to check.

## Roadmap

| Phase | What | Status |
|---|---|---|
| 1 | Core, VPN detection, geolocation consensus, time zone and locale on 3 OSes, CLI | Implemented (on `main`, not released yet) |
| 2 | Linux geoclue location backend | Implemented (on `main`, not released yet) |
| 3 | Windows location via a virtual NMEA serial port | Planned |
| 4 | Browser extension bridge (native messaging, push the profile to the browser) | Planned |
| 5 | Leak checks (DNS, IPv6, WebRTC/STUN) | Planned |
| 6 | Windows GNSS sensor driver (UMDF) | Planned |
| 7 | macOS expert mode (real location spoofing) | Planned |
| 8 | Tray UI (Tauri) talking to the daemon | Planned |

## Project layout

```text
crates/
  geoveil-core      pure logic, no OS calls: profile, VPN detection, IP geolocation and voting,
                    offline time zone and locale lookup, orchestrator, snapshot store
  geoveil-platform  OS backends (Windows, Linux, macOS) behind one trait, interface listing,
                    command runner
  geoveil-cli       the `geoveil` binary: detect, apply, restore, status, run
docs/
  design.md         design and phases
  platforms.md      what each OS backend does, exactly
```

## Development

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Every OS backend is compiled and tested on every OS. Backends run external commands through a `CommandRunner` trait. Tests use a fake runner that records the exact commands, so they never change your system.

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs build, tests and clippy (warnings are errors) on Windows, Linux and macOS for every push and pull request.

Please do not run `apply`, `run` or `restore` on a machine you care about while developing a backend. Use the tests, or a virtual machine.

Bug reports and pull requests are welcome at [github.com/Hamid-Jahani/geoveil](https://github.com/Hamid-Jahani/geoveil). See [CONTRIBUTING.md](CONTRIBUTING.md) and [CHANGELOG.md](CHANGELOG.md). Report security issues privately as described in [SECURITY.md](SECURITY.md).

## Acknowledgements

Geoveil is inspired by the [GeoSpoof](https://github.com/GeoSpoof/geospoof) browser extension (Apache-2.0). It shares no code with GeoSpoof and is not affiliated with it. "GeoSpoof" is a trademark of GeoSpoof LLC.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE). Copyright 2026 Hamid ([@Hamid-Jahani](https://github.com/Hamid-Jahani)).
