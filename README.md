# Geoveil

[![CI](https://github.com/Hamid-Jahani/geoveil/actions/workflows/ci.yml/badge.svg)](https://github.com/Hamid-Jahani/geoveil/actions/workflows/ci.yml)

Geoveil makes your operating system's time zone, locale and location match your VPN exit.

> Status: early (version 0.1.1). Time zone and locale work on Windows, Linux and macOS.
> Location works on Linux (geoclue) and is block-only on macOS. Windows location is not spoofed.
> A tray app (`geoveil-tray`), start at login and a read-only leak check (`geoveil leaks`) are available.

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
| Windows | Yes: `tzutil` | Yes: `Set-Culture`, `Set-WinHomeLocation` (per user) | No (not planned) |
| Linux | Yes: `timedatectl` | Yes: `localectl` (new sessions only) | Yes: geoclue static source (geoclue 2.7 or newer) |
| macOS | Yes: `systemsetup` | Yes: `defaults -g AppleLocale` | Block only: turns Location Services off |

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

### Prebuilt binaries

Tagged releases on the [Releases page](https://github.com/Hamid-Jahani/geoveil/releases) carry one archive per platform (Windows x86_64, Linux x86_64, macOS Intel and Apple Silicon). Each holds `geoveil` and `geoveil-tray`, and `SHA256SUMS` lists the checksums.

**The binaries are not code-signed.** Windows SmartScreen may warn ("Windows protected your PC": choose More info, then Run anyway). macOS Gatekeeper blocks files downloaded with a browser until you allow them in System Settings > Privacy & Security, or remove the quarantine flag with `xattr -d com.apple.quarantine geoveil geoveil-tray`. Check the archive against `SHA256SUMS`, or build from source. See the [Code signing policy](#code-signing-policy).

Every archive from v0.1.1 on also carries a GitHub build attestation, which proves it was built by this repository's release workflow from the tagged source:

```sh
gh attestation verify geoveil-v0.1.1-x86_64-pc-windows-msvc.zip --repo Hamid-Jahani/geoveil
```

### Requirements

- Rust stable, 1.88 or newer ([rustup.rs](https://rustup.rs)). The tray app (`geoveil-tray`) needs 1.90 or newer.
- A C compiler and linker for your platform (one dependency, `ring`, compiles C code):
  - **Windows:** Microsoft C++ Build Tools ("Desktop development with C++" workload), or Visual Studio with that workload.
  - **Linux:** `gcc` or `clang` (for example `sudo apt install build-essential` on Debian/Ubuntu).
  - **macOS:** Xcode Command Line Tools (`xcode-select --install`).

### Build

```sh
git clone https://github.com/Hamid-Jahani/geoveil.git
cd geoveil
cargo build --release --workspace    # geoveil and geoveil-tray (Rust 1.90 or newer)
cargo build --release                # geoveil only (Rust 1.88 or newer)
```

The binaries are:

- `target/release/geoveil` and `target/release/geoveil-tray` on Linux and macOS,
- `target\release\geoveil.exe` and `target\release\geoveil-tray.exe` on Windows.

Copy them somewhere on your `PATH` if you like. Keep both in the same directory: the tray looks for `geoveil` next to itself first.

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
- You sign out of Windows or shut it down: a hidden window asks Windows to wait ("Geoveil is restoring your settings") and the daemon restores before it lets the sign-out continue, for at most 20 seconds. This also works for a daemon started by the tray, which has no console. Details: [docs/platforms.md](docs/platforms.md#sign-out-and-shutdown-geoveil-run).

If an apply or restore fails, the daemon retries after 30 seconds, then doubles the wait each time, up to 10 minutes.

`--sticky` keeps the spoofed settings when the VPN goes down or the daemon stops. It only warns. Your real location can then leak while the VPN is off. Run `geoveil restore` to undo.

### Tray app

`geoveil-tray` puts an icon in the system tray (menu bar on macOS). Its menu shows:

- the status (engaged, paused, watching with no VPN, daemon not running, or an error),
- the applied country, time zone and locale,
- **Pause** / **Resume following the VPN**: while paused the daemon leaves the settings as they are,
- **Restore original settings now**: restores and pauses, so the next poll does not re-apply; Resume engages again,
- **Open log folder**,
- **Quit**: if the tray started the daemon, Quit stops it too, which restores your settings (unless `--sticky`).

The icon is grey when your real settings are in place, green when engaged, amber when paused and red on an error.

The tray talks to `geoveil run` over a local connection that only your user account can open: a named pipe on Windows (`\\.\pipe\geoveil-<your SID>`, access limited to your account, remote clients rejected) and a Unix socket elsewhere (`$XDG_RUNTIME_DIR/geoveil/geoveil.sock` on Linux, else `~/.local/share/geoveil/run/geoveil.sock`; `~/Library/Application Support/geoveil/run/geoveil.sock` on macOS; `/var/run/geoveil/geoveil.sock` for a daemon running as root). The socket has mode 0600, and its folder, used for nothing else, is set to 0700 each time the daemon starts. Both sides refuse a peer that runs as another user. If no daemon answers, the tray starts `geoveil run` itself, in the background, with the options saved by `geoveil autostart enable` (pass `--no-start-daemon` to prevent that). The daemon's output goes to `geoveil.log` in the log folder (`%LOCALAPPDATA%\geoveil\logs`, `~/.local/share/geoveil/logs`, `~/Library/Application Support/geoveil/logs`).

On Linux the tray uses the StatusNotifierItem protocol (KDE, and GNOME with the AppIndicator extension, which Ubuntu ships). It needs no GTK. Stock GNOME without the extension shows no tray icon; `geoveil run` works without it.

**The tray's daemon runs as you, not as root.** Because both sides refuse a peer of another user, the tray cannot control a daemon started with `sudo`. So a daemon started by the tray (or at login) can only change what your user may change:

- **macOS:** only the locale (`--locale`). The time zone and location need root, so every engage reports them as "needs elevated privileges". To follow the VPN with the time zone, run `sudo geoveil run` yourself instead of the tray.
- **Linux:** the time zone and locale change only if polkit allows your user (many desktops ask for a password or refuse in the background). `--location` writes files in `/etc` and needs root.
- **Windows:** the time zone and locale work; only turning off "Set time zone automatically" needs admin, as with `geoveil run`.

### Start at login

```sh
geoveil --locale autostart enable    # start geoveil-tray at login; it runs `geoveil run --locale`
geoveil autostart status
geoveil autostart disable
```

`enable` registers `geoveil-tray` (next to `geoveil`, or `--tray-path PATH`) for your user only. No admin rights and no system service are involved:

| OS | Entry |
|---|---|
| Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `Geoveil` (written with `reg.exe`). `status` also notices when you turned it off in Task Manager. |
| Linux | `~/.config/systemd/user/geoveil-tray.service`, enabled with `systemctl --user enable` (started with `graphical-session.target`). Desktops without systemd session integration (XFCE, MATE, Cinnamon, i3 and other plain window managers) never reach that target, so the tray does not start there although `status` says "enabled"; add `geoveil-tray --autostart` to the desktop's own startup programs instead. |
| macOS | `~/Library/LaunchAgents/io.geoveil.tray.plist`, loaded by launchd at the next login. |

`enable` also saves `--locale`, `--location` and `--state-dir` in `daemon.json` in the geoveil config folder (`%APPDATA%\geoveil`, `~/.config/geoveil`, `~/Library/Application Support/geoveil`). The tray passes them to the daemon it starts. You can also set `sticky`, `interval_secs` and `geoveil_path` there by hand.

### Check for leaks

```sh
geoveil leaks                 # changes nothing, needs no admin rights
geoveil leaks --active-dns    # also run Mullvad's DNS leak test (opt-in)
geoveil --json leaks          # machine-readable report
```

`leaks` checks whether information escapes the VPN and prints one line per check (`PASS`, `INFO`, `N/A`, `WARN`, `LEAK` or `ERROR`):

- **vpn:** the VPN interfaces found and the exit IP.
- **ipv6:** asks IPv6-only endpoints which address they see. A non-VPN IPv6 address of this machine while a VPN is up is a leak.
- **webrtc:** sends STUN requests over the default route and from each physical interface with a gateway, as a browser's WebRTC would. An answer that is not the VPN exit is a leak. If no request could be bound to a physical interface (on Linux before kernel 5.7 that needs root), or no STUN server could be reached at all, the result is `ERROR`, never `PASS`.
- **dns:** lists the configured resolvers per interface. A public resolver on a non-VPN interface while a VPN is up is a leak when a STUN request from that interface also reached the internet outside the tunnel; if it is only configured (your VPN's kill switch may block it), it is a warning. A LAN resolver is a warning. On macOS only the default resolvers count, not the "scoped" ones. With `--active-dns`, Mullvad's test shows which resolvers really answered.
- **verify:** while a profile is engaged, the usual verify results.

`--no-ipv6`, `--no-stun` and `--stun HOST:PORT` (repeatable) skip or redirect probes. Exit code 3 means at least one leak was found. The report lists every host it contacted. Details per OS: [docs/platforms.md](docs/platforms.md#leak-checks).

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
| `--location` | Also drive the OS location service (Linux: geoclue; macOS: turn Location Services off; Windows: not available, a warning is printed). |
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
| 3 | `leaks` found at least one leak. |

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
- **`geoveil leaks` only:** `https://api6.ipify.org`, `https://ipv6.icanhazip.com`, and STUN over UDP to `stun.l.google.com:19302` and `stun.cloudflare.com:3478` (or your `--stun` servers). With `--active-dns` also `https://am.i.mullvad.net` and `https://<random>.dnsleak.am.i.mullvad.net`. The report lists every host it contacted.

ip-api.com's free tier only supports plain **HTTP**. When the VPN carries your traffic, the request goes through the tunnel, but it is not encrypted between the VPN exit and ip-api.com.

All requests use the normal system route. When your VPN carries the default route, they go through the VPN, so the services see the VPN exit IP, not your real one. Requests carry a `geoveil/<version>` User-Agent.

`apply --lat/--lon` makes no network requests at all. `status` makes none either. `apply --ip X` sends X to the four geolocation services and skips the public IP lookup.

## Code signing policy

Windows release binaries are built and signed only by the automated release workflow ([`.github/workflows/release.yml`](.github/workflows/release.yml)) from the tagged source in this repository. Nothing is signed from a developer machine. Signing through the SignPath Foundation's free open-source program has been applied for; until it is approved, the binaries are unsigned.

- **Authors (committers):** [Hamid-Jahani](https://github.com/Hamid-Jahani)
- **Reviewers:** [Hamid-Jahani](https://github.com/Hamid-Jahani). All changes reach `main` through pull requests that must pass CI on Windows, Linux and macOS.
- **Approvers:** [Hamid-Jahani](https://github.com/Hamid-Jahani). Every signing request is approved manually.

All team members use multi-factor authentication for GitHub and the signing service.

**Privacy:** Geoveil sends no telemetry. It contacts only the services listed under [Privacy](#privacy), and only for the commands described there; `apply --lat/--lon` and `status` make no network requests.

**System changes:** Geoveil changes the time zone, and with `--locale` / `--location` the region and location settings, only when you run `apply` or `run` (or start the tray, which runs `run`). It saves your original settings first and restores them with `geoveil restore`, when the VPN goes down, or when the daemon stops. To remove Geoveil, run `geoveil restore` and `geoveil autostart disable`, then delete the binaries and the state directory listed under [Safety](#safety).

## Limitations

- Windows location is not spoofed, and no Windows location backend is planned. `--location` on Windows prints a warning and does nothing.
- On Windows, a sign-out or shutdown waits at most 20 seconds for the restore. If the restore does not finish in time, the snapshot is kept: run `geoveil restore` after the next sign-in. `geoveil status` shows whether a snapshot is left.
- macOS location is only blocked, not spoofed: Location Services is turned off. Real spoofing on macOS is not planned. On recent macOS versions, System Integrity Protection (SIP) may keep this from taking effect until a reboot, or at all.
- Linux location only affects apps that use geoclue (GNOME, KDE, Firefox with the geoclue provider, and so on). It needs geoclue 2.7 or newer.
- Linux locale changes only affect sessions started after the change. Log out and back in.
- On Linux, `localectl` refuses locales that are not installed. Geoveil checks `locale -a` first and tells you how to install the missing one.
- Running apps may need a restart to see a new locale (Windows, macOS).
- Windows locale changes the regional format and home location only. The input language list (keyboard layouts) is never touched.
- Some countries have no known Windows GeoID in the built-in table. Then only the regional format is changed.
- `geoveil leaks` detects DNS, IPv6 and WebRTC leaks but does not fix them. With a VPN on your router or a proxy, this machine cannot see your real IP, so it cannot prove there is no leak. See [docs/platforms.md](docs/platforms.md#leak-checks).
- Geoveil does not change what your browser reports through its own APIs (for example the browser geolocation API or WebRTC). No browser extension bridge is planned. `geoveil leaks` shows what WebRTC would expose.
- `geoveil status` reads the state file; it does not ask a running daemon over IPC yet (the tray does).
- VPN detection uses adapter names. An unusual VPN adapter name may not be recognized. Run `geoveil detect` to check.

## Roadmap

| Phase | What | Status |
|---|---|---|
| 1 | Core, VPN detection, geolocation consensus, time zone and locale on 3 OSes, CLI | Released in v0.1.0 |
| 2 | Linux geoclue location backend | Released in v0.1.0 |
| 3 | Windows location via a virtual NMEA serial port | Not pursued |
| 4 | Browser extension bridge (native messaging, push the profile to the browser) | Not pursued |
| 5 | Leak checks (DNS, IPv6, WebRTC/STUN): `geoveil leaks` | Released in v0.1.0 |
| 6 | Windows GNSS sensor driver (UMDF) | Not pursued |
| 7 | macOS expert mode (real location spoofing) | Not pursued; macOS location stays block-only |
| 8 | Tray UI talking to the daemon over local IPC, start at login, release builds | Released in v0.1.0 |

## Project layout

```text
crates/
  geoveil-core      pure logic, no OS calls: profile, VPN detection, IP geolocation and voting,
                    offline time zone and locale lookup, orchestrator, snapshot store,
                    leak-check verdicts and STUN codec
  geoveil-platform  OS backends (Windows, Linux, macOS) behind one trait, interface listing,
                    command runner, DNS resolver discovery and interface-bound STUN probes
  geoveil-cli       the `geoveil` binary: detect, apply, restore, status, run, leaks, autostart
  geoveil-ipc       local IPC between the daemon and the tray: protocol, per-user endpoint
  geoveil-tray      the `geoveil-tray` binary: tray icon and menu
docs/
  design.md         design and phases
  platforms.md      what each OS backend does, exactly
```

## Development

```sh
cargo build --workspace      # --workspace includes the tray (Rust 1.90+)
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Every OS backend is compiled and tested on every OS. Backends run external commands through a `CommandRunner` trait. Tests use a fake runner that records the exact commands, so they never change your system.

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs build, tests and clippy (warnings are errors) on Windows, Linux and macOS for every push and pull request. Pushing a `v*` tag runs [`.github/workflows/release.yml`](.github/workflows/release.yml), which tests and builds both binaries for each release target and publishes the archives with `SHA256SUMS` (unsigned).

Please do not run `apply`, `run` or `restore` on a machine you care about while developing a backend. Use the tests, or a virtual machine.

Bug reports and pull requests are welcome at [github.com/Hamid-Jahani/geoveil](https://github.com/Hamid-Jahani/geoveil). See [CONTRIBUTING.md](CONTRIBUTING.md) and [CHANGELOG.md](CHANGELOG.md). Report security issues privately as described in [SECURITY.md](SECURITY.md).

## Acknowledgements

Geoveil is inspired by the [GeoSpoof](https://github.com/GeoSpoof/geospoof) browser extension (Apache-2.0). It shares no code with GeoSpoof and is not affiliated with it. "GeoSpoof" is a trademark of GeoSpoof LLC.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE). Copyright 2026 Hamid ([@Hamid-Jahani](https://github.com/Hamid-Jahani)).
