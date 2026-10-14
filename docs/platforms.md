# Platform backends

This page lists exactly what Geoveil reads and writes on each OS. It is derived from the code in `crates/geoveil-platform` and `crates/geoveil-core`. If the code and this page disagree, the code wins. Please open an issue.

## Common behaviour

Each setting Geoveil can change is a **backend**. Every backend has four steps:

- **snapshot**: read the current setting. Changes nothing.
- **apply**: set the system to match the profile.
- **restore**: put back exactly what the snapshot read.
- **verify**: read the setting again and compare it with the profile. This is what `geoveil status` shows.

Rules that apply to all backends:

- Backends are enabled in this order: `timezone` (always), `locale` (with `--locale`), `location` (with `--location`).
- All snapshots are saved to `state.json` **before** any backend is applied. The file is written to a temp file, synced, and renamed into place.
- If a backend's snapshot fails, that backend is skipped and reported as failed. It is never changed.
- When already engaged (for example the VPN exit changed), existing snapshots are kept. Only backends that have no snapshot yet are snapshotted.
- Restore runs in reverse order. `state.json` is deleted only when every restore succeeded.
- A snapshot whose backend is not enabled cannot be restored. It is reported as "no such backend enabled; enable it to restore this setting". Run `restore` with the same `--locale` / `--location` flags you used for `apply`.
- External programs are started directly, never through a shell. Each value is one argument. Values read from the state file are validated before use.
- Each external command is killed after 120 seconds (for example a password prompt nobody answers).
- If command output contains phrases such as "access denied", "permission denied", "not authorized", "polkit" or "you need administrator access", the error is reported as "needs elevated privileges". A program that is not installed is reported as "not supported on this system".

## Windows

Backends: `timezone`, and `locale` with `--locale`. There is no Windows location backend yet. With `--location`, Geoveil logs a warning ("Windows location spoofing lands in phase 3") and continues without it.

### Time zone

| Step | What runs |
|---|---|
| snapshot | `tzutil /g`, and `reg query HKLM\SYSTEM\CurrentControlSet\Services\tzautoupdate /v Start` |
| apply | `tzutil /s "<Windows zone id>"`. Then, if `tzautoupdate` `Start` is readable and not 4: `reg add HKLM\SYSTEM\CurrentControlSet\Services\tzautoupdate /v Start /t REG_DWORD /d 4 /f` |
| restore | `tzutil /s "<saved id>"`. Then, if a `Start` value was saved and the current value differs, `reg add ... /v Start /t REG_DWORD /d <saved> /f` |
| verify | `tzutil /g` and `reg query ...` |

- The IANA zone of the profile is mapped to a Windows zone id with the embedded CLDR `windowsZones` table (plus tzdata alias names). If there is no mapping, apply fails with a clear message.
- The saved id is stored exactly as `tzutil /g` printed it, including a `_dstoff` suffix if daylight saving adjustment was off. So restore brings back the exact setting.
- `tzutil` sometimes prints `Error: ...` and still exits 0. Geoveil treats that output as a failure.
- **Automatic time zone.** "Set time zone automatically" is the `tzautoupdate` service. `Start` = 3 means on, 4 means off. When it is on, Windows puts the real zone back on its own. Geoveil sets it to 4 while engaged and restores the saved value afterwards. Changing it needs admin rights. Without admin, apply only logs a warning and the time zone change still counts as done.
- **Privileges.** `tzutil /s` works without admin. The `tzautoupdate` change needs admin.
- **Verify** is OK when the current id (ignoring `_dstoff`, case-insensitive) equals the expected id, and automatic time zone is not on. If it is on, verify says "Set time zone automatically is ON and may revert it".

Snapshot data example:

```json
{"windows_id": "W. Europe Standard Time", "tzautoupdate_start": 3}
```

### Locale (opt-in)

All steps run `powershell -NoProfile -NonInteractive -Command <script>`.

| Step | What the script does |
|---|---|
| snapshot | Reads `(Get-Culture).Name`, `(Get-WinHomeLocation).GeoId`, and every **string** value under `HKCU:\Control Panel\International` |
| apply | `Set-Culture -CultureInfo '<locale>'`, then `Set-WinHomeLocation -GeoId <id>` if the country has a known GeoID |
| restore | `Set-Culture` with the saved culture, `Set-WinHomeLocation` with the saved GeoID, then writes every saved `International` value back with `Set-ItemProperty` |
| verify | Reads culture and home GeoID again |

- These are per-user settings. No admin is needed.
- **Custom regional formats.** `Set-Culture` resets your own format overrides (short date, time format, first day of week, decimal symbol, and so on). These are string values under `HKCU\Control Panel\International`. The snapshot saves all of them, and restore writes them back after `Set-Culture`. Value names must match `[A-Za-z0-9]{1,64}`. Other names are skipped with a warning. Values are passed to PowerShell as base64-encoded JSON, so any character is safe.
- Apps that are already running may only see the restored formats after a restart.
- `Set-WinUserLanguageList` is **not** used. It would replace your input languages and keyboard layouts.
- The GeoID comes from a built-in table that only lists IDs known to be correct. For countries not in it (for example AX, BL, BQ, SS, TL, XK and many small territories), only the culture is changed and a warning is logged.
- The culture name in the profile must look like a BCP-47 tag (`^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})*$`), or apply refuses it. Saved culture names may also contain `_` (for example `de-DE_phoneb`).
- **Verify** is OK when the culture equals the profile locale (case-insensitive) and the GeoID matches. If the country has no known GeoID, the GeoID is not checked.

Snapshot data example:

```json
{"culture": "en-US", "geo_id": 244, "international": {"sShortDate": "yyyy-MM-dd"}}
```

## Linux

Backends: `timezone`, `locale` with `--locale`, `location` with `--location`.

### Time zone

| Step | What runs |
|---|---|
| snapshot | `timedatectl show -p Timezone --value`, and `gsettings get org.gnome.desktop.datetime automatic-timezone` |
| apply | `timedatectl set-timezone <zone>`. Then, if GNOME's `automatic-timezone` is `true`: `gsettings set org.gnome.desktop.datetime automatic-timezone false` |
| restore | `timedatectl set-timezone <saved zone>`. Then, if it was `true` before and is not now: `gsettings set ... automatic-timezone true` |
| verify | `timedatectl show -p Timezone --value` and `gsettings get ...` |

- **Privileges.** `timedatectl set-timezone` needs root or polkit approval. A polkit refusal is reported as "needs elevated privileges".
- Apply only accepts zones in the embedded table (CLDR zones and tzdata aliases), `UTC`, `Etc/UTC` and `Etc/GMT±N`.
- The saved zone is checked by shape only (letters, digits, `/ _ + -`, at most 128 characters), so any valid system zone can be restored, even one the table does not list.
- If `timedatectl` reports no zone, the snapshot fails and the time zone is not changed.
- **Automatic time zone.** `gsettings` runs as the user Geoveil runs as. If `gsettings` or the schema is missing (not GNOME, no session bus), the setting is left alone.
- **Verify** is OK when the zone is exactly the expected one and GNOME automatic time zone is not on.

### Locale (opt-in)

| Step | What runs |
|---|---|
| snapshot | `localectl status` (every `VAR=value` line of "System Locale") |
| apply | `locale -a` to check the locale is installed, then `localectl set-locale LANG=<posix locale>` |
| restore | `localectl set-locale <every saved VAR=value>` |
| verify | `localectl status` |

- The BCP-47 locale is converted to a glibc name, for example `de-DE` becomes `de_DE.UTF-8` and `sr-Latn-RS` becomes `sr_RS.UTF-8@latin`.
- **Privileges.** `localectl set-locale` needs root or polkit approval.
- **Missing locales.** systemd-localed refuses locales that are not installed. If `locale -a` does not list the locale, apply fails and tells you how to install it, for example `sudo locale-gen de_DE.UTF-8` (Debian/Ubuntu) or `sudo localedef -i de_DE -f UTF-8 de_DE.UTF-8`. If `locale -a` cannot run, apply tries `localectl` anyway.
- If no system locale is configured (`localectl` shows `n/a`), the snapshot fails and the locale is not changed. "Unset" cannot be restored portably.
- **New sessions only.** The system locale only affects sessions started after the change. Running desktop sessions keep their environment. Log out and back in.
- **Verify** is OK when `LANG` equals the expected glibc name.

### Location (opt-in): geoclue static source

| Step | What happens |
|---|---|
| snapshot | Checks that `/etc/geoclue` exists. Runs `<geoclue binary> --version`. Reads `/etc/geolocation` and `/etc/geoclue/conf.d/90-geoveil.conf` (saved as "absent" if missing) |
| apply | Writes `/etc/geolocation` and `/etc/geoclue/conf.d/90-geoveil.conf`, then runs `systemctl restart geoclue` |
| restore | Writes back each file's saved contents, or deletes it if it did not exist before. Then `systemctl restart geoclue` |
| verify | Reads both files |

`/etc/geolocation` holds latitude, longitude, altitude (always 0) and accuracy in metres, one per line, with 6 decimals:

```text
# Written by geoveil. Restored when geoveil disengages.
52.520000
13.405000
0.000000
5000.000000
```

The accuracy is 5000 m for a profile resolved from an IP, and 100 m (or `--accuracy`) for manual coordinates.

The drop-in enables the static source and turns off every other source:

```ini
# Written by geoveil. Removed or restored when geoveil disengages.
[static-source]
enable=true

[wifi]
enable=false

[3g]
enable=false

[cdma]
enable=false

[modem-gps]
enable=false

[network-nmea]
enable=false

[compass]
enable=false
```

- **Privileges.** Root: the files are in `/etc`, and `systemctl restart` needs it too. A permission error is reported as "needs elevated privileges".
- **geoclue version.** geoclue reads `/etc/geoclue/conf.d/` drop-ins only from version 2.7. Geoveil looks for the daemon at `/usr/libexec/geoclue`, `/usr/lib/geoclue-2.0/geoclue`, `/usr/libexec/geoclue-2.0/geoclue` and `/usr/lib/geoclue`. If it finds a version older than 2.7, the snapshot fails and the backend is never applied. If it cannot find a version, it continues and logs a warning.
- If `/etc/geoclue` does not exist, the backend reports that geoclue is not installed and is skipped.
- Files are written atomically (temp file `.<name>.geoveil-tmp` in the same directory, created exclusively, mode 0644, synced, renamed). A symlink at a target path is refused, never followed.
- If `systemctl restart geoclue` fails, only a warning is logged. geoclue is started on demand over D-Bus and reads the files the next time it starts.
- Only apps that ask geoclue for the location see it: GNOME, KDE, Firefox with the geoclue provider, and so on.
- **Verify** checks Geoveil's files only: the coordinates and accuracy in `/etc/geolocation` (within 0.000001) and that the drop-in is unchanged. It does not ask geoclue what it reports.

## macOS

Backends: `timezone`, `locale` with `--locale`, `location` with `--location`.

### Time zone

| Step | What runs |
|---|---|
| snapshot | `systemsetup -gettimezone`, and `defaults read /Library/Preferences/com.apple.timezone.auto Active` |
| apply | `systemsetup -settimezone <zone>`. Then, if automatic time zone may be on: `defaults write /Library/Preferences/com.apple.timezone.auto Active -bool false` |
| restore | `systemsetup -settimezone <saved zone>`. Then the `Active` key is put back: written with its saved value, or deleted if it was not set before |
| verify | `systemsetup -gettimezone` and `defaults read ...` |

- **Privileges.** `systemsetup` needs root. Without it, it prints "You need administrator access" and still exits 0. Geoveil treats that as "needs elevated privileges". Writing `/Library/Preferences` also needs root.
- **Automatic time zone.** "Set time zone automatically using your current location" is the `Active` key. If the key is missing, macOS treats it as on, so Geoveil also turns it off then. If the key could not be read at all, it is never touched.
- Apply only accepts zones from the embedded table, `UTC`, `Etc/UTC` and `Etc/GMT±N`. The saved zone is checked by shape only, so values such as `GMT` round-trip.
- **Verify** is OK when the zone is exactly the expected one and automatic time zone is not (possibly) on.

### Locale (opt-in)

| Step | What runs |
|---|---|
| snapshot | `defaults read -g AppleLocale` (saved as "unset" if missing) |
| apply | `defaults write -g AppleLocale -string <value>` |
| restore | `defaults write -g AppleLocale -string <saved>`, or `defaults delete -g AppleLocale` if it was unset |
| verify | `defaults read -g AppleLocale` |

- The BCP-47 locale is converted to `AppleLocale` form, for example `de-DE` becomes `de_DE` and `zh-Hans-CN` becomes `zh-Hans_CN`.
- Only `AppleLocale` (region formats) changes. `AppleLanguages` (the UI language order) is never touched.
- **Privileges.** None. This is a per-user setting.
- **Under `sudo`.** `defaults -g` would otherwise write root's settings, which you never see. When `SUDO_UID` and `SUDO_USER` name a non-root user, every `defaults` call runs as that user: `launchctl asuser <uid> sudo -u <user> defaults ...`.
- Running apps pick up the new region formats after a restart.
- **Verify** is OK when `AppleLocale` equals the expected value. The detail names whose settings were read.

### Location (opt-in): block only

macOS cannot be given a fake position without disabling System Integrity Protection (SIP). So this backend **turns Location Services off** while engaged. Apps then fall back to IP geolocation, which already points at the VPN exit. The profile's coordinates are not used.

| Step | What runs |
|---|---|
| snapshot | `defaults read /var/db/locationd/Library/Preferences/ByHost/com.apple.locationd LocationServicesEnabled` (saved as "unset" if missing) |
| apply | `defaults write <same domain> LocationServicesEnabled -int 0`, then `launchctl kickstart -k system/com.apple.locationd` |
| restore | `defaults write ... -int <saved>`, or `defaults delete ...` if it was unset. Then `launchctl kickstart -k system/com.apple.locationd` |
| verify | `defaults read ...` |

- **Privileges.** Root (the preference lives under `/var/db/locationd`).
- **SIP.** On recent macOS versions SIP may stop `locationd` from honouring the change, or block the `launchctl kickstart`. The setting can then be written but not take effect until a reboot, or not at all. A failed kickstart only logs a warning.
- **Verify** is OK only when `LocationServicesEnabled` is 0. The detail always adds: "blocked, not spoofed; may be ineffective on recent macOS with SIP until reboot".

## VPN detection

Interfaces are listed with the `netdev` crate. On Windows the interface name is the adapter GUID, so the friendly name ("WireGuard Tunnel") and the adapter description ("Wintun Userspace Tunnel") are also used for matching.

An interface counts as a VPN when **all** of these are true:

1. It is up and has at least one address.
2. None of its addresses is in Tailscale's IPv6 range `fd7a:115c:a1e0::/48`.
3. Its name plus description (lower case) does **not** contain any of these: `tailscale`, `zerotier`, `teredo`, `isatap`, `hyper-v`, `vethernet`, `virtualbox`, `vmware`, `loopback`, `bluetooth`.
4. And it matches one of these rules:
   - It contains one of: `wireguard`, `wintun`, `openvpn`, `tap-windows`, `nordlynx`, `protonvpn`, `mullvad`, `expressvpn`, `surfshark`, `cisco anyconnect`, `fortinet`, `fortissl`, `globalprotect`, `pangp`.
   - A word in it is `wg`, `tun`, `tap`, `ppp` or `proton` followed by digits only (for example `wg0`, `tun1`, `ppp0`, `proton0`), or a word starts with `ipsec`.
   - A word in it is `utun` followed by digits, **and** the interface has an IPv4 address that is not link-local, not loopback and not `0.0.0.0`.

The extra `utun` rule exists because macOS creates `utun0`, `utun1`, ... for system services (such as iCloud Private Relay) even with no VPN running. Those only carry IPv6 link-local addresses. Real VPN clients assign an IPv4 address.

The deny list wins over every match. For example, a Hyper-V adapter is never a VPN, whatever else its name contains.

### How the daemon uses it

- It lists interfaces every `--interval-secs` (default 5).
- It fetches the public IP when the set of VPN interfaces changed, or every 60 seconds while a VPN interface is present.
- VPN interface present and public IP known: "up" (first time) or "changed" (different IP). The daemon then resolves and applies the profile.
- VPN interface present but the public IP lookup failed: nothing changes, and it tries again on the next poll.
- No VPN interface: "down". The daemon restores the original settings (unless `--sticky`).
- At startup, if a state file exists and no VPN interface is present, the daemon restores it (unless `--sticky`). If a VPN is up, it keeps the saved originals and resolves the exit again.

### Public IP and geolocation

- The public IP is fetched from `https://api.ipify.org`, `https://icanhazip.com` and `https://ifconfig.me/ip`, over IPv4 only first. Only if every IPv4 attempt fails are they asked again over any address family. The first valid answer wins.
- That exact IP is then sent to `ipwho.is`, `ipapi.co`, `ip-api.com` (plain HTTP) and `ipinfo.io`, all at once, each with the `--provider-timeout-secs` timeout (default 5). The public-IP lookup uses the same timeout.
- If no public IP can be found, `detect` and `apply` ask each service to locate the caller's own address instead (answers about different IPs are still never mixed). The daemon does not do this; it tries again on the next poll.
- Answers about different IPs are never mixed. The largest IPv4 group is used (IPv6 only if no service saw an IPv4 address).
- The winning country needs strictly more answers than any other country, and at least 2 services must agree. Otherwise nothing is applied.
- The coordinates are those of the agreeing answer closest to the others (total great-circle distance).
- The time zone comes from the coordinates using built-in boundary data (`tzf-rs`). If that finds nothing, the time zone most often reported by the agreeing services is used if it is a valid zone, otherwise `UTC`.
- The locale comes from a built-in country table, for example `DE` gives `de-DE`, `CH` gives `de-CH`, `IN` gives `en-IN`. Unknown countries give `en-US`.

## Start at login (`geoveil autostart`)

Per user only; no admin rights, no system service. The entry starts `geoveil-tray --autostart`.

| OS | `enable` | `disable` | `status` |
|---|---|---|---|
| Windows | `reg add HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v Geoveil /t REG_SZ /d "\"<tray>\" --autostart" /f` | `reg delete ...\Run /v Geoveil /f` (a missing value is fine) | `reg query ...\Run /v Geoveil`, then `reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run /v Geoveil` (first byte `03` = turned off in Task Manager) |
| Linux | writes `$XDG_CONFIG_HOME/systemd/user/geoveil-tray.service` (`WantedBy=graphical-session.target`, `Restart=on-failure`), then `systemctl --user daemon-reload` and `systemctl --user enable geoveil-tray.service` | `systemctl --user disable geoveil-tray.service`, removes the file, `systemctl --user daemon-reload` | reads the file and `systemctl --user is-enabled geoveil-tray.service` |
| macOS | writes `~/Library/LaunchAgents/io.geoveil.tray.plist` (`RunAtLoad`, restart only after a crash); launchd loads it at the next login | removes the file | reads the file |

The tray path must be absolute and may not contain control characters or double quotes. It is quoted for the registry, escaped for systemd (`\`, `"`, `%`, `$`) and XML-escaped for the plist.
