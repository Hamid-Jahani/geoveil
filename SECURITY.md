# Security Policy

Geoveil changes system settings (timezone, locale, region, location
services) and often runs with administrator or root rights. Security bugs
matter. Thank you for reporting them responsibly.

## Reporting a vulnerability

Please report vulnerabilities **privately** through GitHub private
vulnerability reporting:

1. Go to <https://github.com/Hamid-Jahani/geoveil/security>.
2. Click **Report a vulnerability**.
3. Describe the issue, the affected OS and version, the Geoveil version or
   commit, and steps to reproduce.

Do **not** open a public issue, discussion or pull request for a security
problem.

You can expect an acknowledgement within a few days. Once the issue is
confirmed, a fix will be prepared and a GitHub security advisory published.
You will be credited unless you ask not to be.

## Scope

Examples of issues that are in scope:

- **Command injection.** Data from IP-geolocation providers (country,
  timezone, coordinates) or from the snapshot file ending up unescaped in a
  command line or a PowerShell script.
- **Restore failures.** Bugs that leave the system in a spoofed state after
  `geoveil restore`, after the `run` daemon stops, or after a crash, or that
  lose the saved original settings.
- **State file and lock issues.** Problems with `state.json` or
  `geoveil.lock`: unsafe permissions, symlink attacks, races between two
  Geoveil processes, or a corrupted state file being silently discarded.
- **Privilege handling.** Geoveil running under `sudo` or as administrator
  doing more than it should, writing files as the wrong user, or following
  symlinks into places it should not write (for example under `/etc`).

Out of scope:

- Geoveil does not provide a VPN and does not change your IP address. Leaks
  from your VPN client itself (DNS, IPv6, WebRTC) are not Geoveil
  vulnerabilities. Leak checks are a planned feature, not a current one.
- Websites detecting your real location by other means (for example browser
  fingerprinting) are a known limitation, not a vulnerability.
- Wrong results from third-party geolocation providers.

## Supported versions

Geoveil has no stable release yet. Security fixes are made on the `main`
branch only.

| Version | Supported |
|---|---|
| `main` (latest) | Yes |
| Older commits | No |

Once releases exist, only the latest release will receive security fixes.
