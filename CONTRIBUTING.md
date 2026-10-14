# Contributing to Geoveil

Thanks for your interest in Geoveil. This guide explains how to build the
project, how the code is tested, and what a good pull request looks like.

Before you start, read [docs/design.md](docs/design.md). It describes the
architecture and the planned phases.

## Prerequisites

Rust **1.88** or newer (`rust-version` in `Cargo.toml`) for `geoveil`, and
**1.90** or newer for `geoveil-tray`. A plain `cargo build` / `cargo test`
skips the tray (`default-members`); `--workspace`, which the commands below
and CI use, includes it. CI tests the latest stable.

### Windows

- Rust stable with the MSVC toolchain (`stable-x86_64-pc-windows-msvc`),
  installed with [rustup](https://rustup.rs).
- Visual Studio Build Tools with the **Desktop development with C++**
  workload.
- A Windows SDK (included in that workload by default).

The C++ tools are needed because some dependencies (for example `ring`, used
by `rustls`) compile C code.

### Linux and macOS

- Rust stable, installed with [rustup](https://rustup.rs).
- A C compiler: `gcc` or `clang` on Linux (for example `build-essential` on
  Debian/Ubuntu), the Xcode Command Line Tools on macOS
  (`xcode-select --install`).

## Build, test, lint

Run the same commands CI runs (see [.github/workflows/ci.yml](.github/workflows/ci.yml)):

```sh
cargo build --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

CI runs them on `windows-latest`, `ubuntu-latest` and `macos-latest`. All
three must pass.

Notes:

- `--locked` means `Cargo.lock` must be up to date. If you add or change a
  dependency, commit the updated `Cargo.lock`.
- Clippy warnings fail CI.
- Some Unix-only code (`flock`, signal handlers, `sudo` detection) is not
  compiled on Windows. If you change it from a Windows machine, rely on CI or
  a Linux/macOS machine to check it.

## Workspace layout

| Crate | Role |
|---|---|
| `crates/geoveil-core` | Pure logic, no OS calls: types, geolocation consensus, timezone and locale tables, VPN detection, orchestrator, snapshot store. |
| `crates/geoveil-platform` | OS backends (Windows, Linux, macOS), interface enumeration, `CommandRunner`. |
| `crates/geoveil-cli` | The `geoveil` binary: CLI parsing, one-shot commands, daemon and its IPC server, autostart, state directory and lock. |
| `crates/geoveil-ipc` | Local IPC between the daemon and the tray: JSON-lines protocol, per-user endpoint (named pipe / Unix socket), peer checks, saved daemon options. |
| `crates/geoveil-tray` | The `geoveil-tray` binary (Rust 1.90 or newer, the rest stays at 1.88). Menu logic lives in pure functions (`model.rs`, `worker.rs`) with tests; `app.rs` only wires them to `tray-icon` / `muda` (and `tao` on Windows and macOS; Linux uses the KSNI backend, no GTK). |

## Safety rule for your own machine

**Do not run `geoveil apply`, `geoveil run` or `geoveil restore` on your
development machine casually.** They change real system settings: timezone,
locale, region, geoclue files under `/etc`, Location Services. A bug in your
branch can leave your system in a spoofed state.

Safe to run at any time:

```sh
geoveil --help
geoveil <command> --help
geoveil detect
geoveil status
```

If you must test `apply` / `restore` for real, use a virtual machine or a
throwaway user account, and pass `--state-dir` to keep the snapshot apart from
any real one.

## Testing pattern: `CommandRunner` and `FakeRunner`

Backends must be testable without touching the OS. Every backend runs
external programs only through the `CommandRunner` trait in
`crates/geoveil-platform/src/runner.rs`:

```rust
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput>;
}
```

- `SystemRunner` runs real processes, with a 120 s timeout.
- `FakeRunner` records every call and replays scripted outputs in order
  (`push`, `push_stdout`, `push_err`). When the script runs out, it returns
  success with empty output.

A backend takes an `Arc<dyn CommandRunner>` in its constructor. In tests,
pass a `FakeRunner`, script the outputs, then assert the exact commands:

```rust
let fake = Arc::new(FakeRunner::new());
let backend = TimezoneBackend::new(fake.clone());
fake.push_stdout("America/New_York\n");
let snap = backend.snapshot().unwrap();
assert_eq!(fake.calls()[0], "timedatectl show -p Timezone --value");
```

Backends that write files take a configurable root (for example the Linux
location backend has `LocationBackend::with_root`), so tests write into a
temporary directory instead of `/etc`.

All backend modules compile on every OS. Only `default_backends` picks the
set for the host. This means the Linux and macOS backends are unit tested on
Windows too, and the other way round.

Rules for tests:

- Never use `SystemRunner` in a backend test.
- Never write to real system paths in a test.
- Tests must not need network access, admin rights or a VPN.

## The `Backend` trait contract

Every OS backend implements `geoveil_core::Backend`
(`crates/geoveil-core/src/backend.rs`):

| Method | Contract |
|---|---|
| `name()` | Stable identifier, used as the key in the snapshot file (for example `"timezone"`). Do not rename it. |
| `snapshot()` | Reads the current state. **Never changes anything.** |
| `apply(profile)` | Sets the system to match the profile. **Must be idempotent**: applying the same profile twice gives the same result. |
| `restore(snapshot)` | Returns the system to **exactly** what `snapshot()` captured. Must accept every value `snapshot()` can produce, so a restore is never stricter than its snapshot. |
| `verify(profile)` | Reads back the current state and reports whether it matches the profile. Changes nothing. |

Also:

- Return the right `BackendError` variant. Use `NeedsPrivilege` when the OS
  says the user lacks rights, and `Unsupported` when a tool is missing. The
  message should name the backend and what is needed.
- Validate every value before it goes into a command line or script. Values
  come from network providers and from the snapshot file on disk. Treat both
  as untrusted.
- Add tests that assert the exact commands for `snapshot`, `apply`,
  `restore` and `verify`, including failure cases.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/), as in the
existing history:

```
feat: add Windows NMEA location backend
fix: keep custom date format on restore
ci: cache cargo registry
chore: bump dependencies
docs: explain --state-dir
```

- Subject in lower case, imperative mood, no trailing period.
- Use the body to explain *why* when it is not obvious.
- One logical change per commit.

## Pull request checklist

Before you open a pull request:

- [ ] `cargo build --workspace --locked` passes.
- [ ] `cargo test --workspace --locked` passes.
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` passes.
- [ ] New backend logic has `FakeRunner` tests that assert exact commands.
- [ ] No test runs real system-changing commands or writes to system paths.
- [ ] The `Backend` contract holds (snapshot read-only, apply idempotent,
      restore exact).
- [ ] Docs are updated (`README.md`, `docs/platforms.md`, `docs/design.md`,
      `CHANGELOG.md` under `[Unreleased]`) when behaviour, flags, defaults or
      backend commands change.
- [ ] Commit messages follow Conventional Commits.

## Reporting bugs and security issues

- Bugs and feature requests: open an issue using the templates.
- Security issues: do **not** open a public issue. See
  [SECURITY.md](SECURITY.md).

## License

By contributing, you agree that your contributions are licensed under the
[Apache License 2.0](LICENSE).
