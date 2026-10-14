//! Linux location backend via the geoclue static source (geoclue >= 2.7).
//!
//! Geoclue reads a fixed position from `/etc/geolocation` when its
//! `[static-source]` is enabled (geoclue 2.6+). We enable it through a drop-in
//! in `/etc/geoclue/conf.d/` (read since geoclue 2.7) and disable every other
//! source so the static position wins. A geoclue older than 2.7 would ignore
//! the drop-in and keep using Wi-Fi location, so snapshot reports
//! [`BackendError::Unsupported`] when it finds one (and the backend is then
//! never applied). When the version cannot be determined we proceed and say
//! so in the log. Only applications that ask geoclue for location (GNOME,
//! KDE, Firefox with the geoclue provider, ...) see the spoofed position.
//!
//! Files are written atomically (temp file in the same directory, fsync,
//! rename) and a symlink at a target path is never followed.

use super::check_snapshot_owner;
use crate::runner::CommandRunner;
use geoveil_core::{Backend, BackendError, BackendSnapshot, Profile, VerifyReport};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const NAME: &str = "location";
const GEOLOCATION_REL: &str = "etc/geolocation";
const GEOCLUE_DIR_REL: &str = "etc/geoclue";
const DROPIN_REL: &str = "etc/geoclue/conf.d/90-geoveil.conf";
/// Matching tolerance when reading back 6-decimal coordinates.
const COORD_EPSILON: f64 = 1e-6;
/// First geoclue release that reads `/etc/geoclue/conf.d/*.conf`.
const MIN_GEOCLUE: (u32, u32) = (2, 7);
/// Where distributions install the geoclue daemon.
const GEOCLUE_BINARIES: &[&str] = &[
    "/usr/libexec/geoclue",
    "/usr/lib/geoclue-2.0/geoclue",
    "/usr/libexec/geoclue-2.0/geoclue",
    "/usr/lib/geoclue",
];

/// Contents of the geoclue drop-in that enables only the static source.
const DROPIN_CONTENTS: &str = "\
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
";

/// Saved state: previous contents of both files, `None` if absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LocationSnapshot {
    geolocation: Option<String>,
    dropin: Option<String>,
}

/// Feeds a fixed position to geoclue through its static source.
pub struct LocationBackend {
    runner: Arc<dyn CommandRunner>,
    root: PathBuf,
}

impl LocationBackend {
    /// Backend operating on the real filesystem root `/`.
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self::with_root(runner, "/")
    }

    /// Backend operating below `root` instead of `/` (used by tests).
    pub fn with_root(runner: Arc<dyn CommandRunner>, root: impl Into<PathBuf>) -> Self {
        Self { runner, root: root.into() }
    }

    fn geolocation_path(&self) -> PathBuf {
        self.root.join(GEOLOCATION_REL)
    }

    fn dropin_path(&self) -> PathBuf {
        self.root.join(DROPIN_REL)
    }

    /// Restart geoclue so it re-reads its config. Failure is not fatal:
    /// geoclue is D-Bus activated and picks the files up on next start.
    fn restart_geoclue(&self) {
        match self.runner.run("systemctl", &["restart", "geoclue"]) {
            Ok(out) if out.success() => {}
            Ok(out) => tracing::warn!(
                status = out.status,
                stderr = out.stderr.trim(),
                "could not restart geoclue; it will use the new settings on next activation"
            ),
            Err(e) => tracing::warn!(error = %e, "could not run systemctl to restart geoclue"),
        }
    }

    /// Version of the installed geoclue daemon (`<binary> --version`), or
    /// `None` when no known binary answers with a version.
    fn geoclue_version(&self) -> Option<(u32, u32)> {
        GEOCLUE_BINARIES.iter().find_map(|bin| {
            let out = self.runner.run(bin, &["--version"]).ok()?;
            parse_version(&format!("{}\n{}", out.stdout, out.stderr))
        })
    }

    /// Fail for a geoclue that would ignore our drop-in.
    fn check_geoclue_version(&self) -> Result<(), BackendError> {
        match self.geoclue_version() {
            Some(v) if v < MIN_GEOCLUE => Err(BackendError::Unsupported {
                backend: NAME.to_string(),
                detail: format!(
                    "geoclue {}.{} ignores /etc/geoclue/conf.d drop-ins; version {}.{} or newer is required",
                    v.0, v.1, MIN_GEOCLUE.0, MIN_GEOCLUE.1
                ),
            }),
            Some(_) => Ok(()),
            None => {
                tracing::warn!(
                    "could not determine the geoclue version; the location drop-in needs geoclue {}.{}+",
                    MIN_GEOCLUE.0,
                    MIN_GEOCLUE.1
                );
                Ok(())
            }
        }
    }

    /// Put a file back to `saved` contents, or delete it if it was absent.
    fn restore_file(&self, path: &Path, saved: Option<&str>) -> Result<(), BackendError> {
        match saved {
            Some(contents) => atomic_write(path, contents).map_err(|e| io_error("restore", path, &e)),
            None => remove_if_present(path).map_err(|e| io_error("remove", path, &e)),
        }
    }
}

/// First `major.minor` version number in `text` (e.g. "2.7.1" -> (2, 7)).
fn parse_version(text: &str) -> Option<(u32, u32)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.')).find_map(|token| {
        let mut parts = token.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some((major, minor))
    })
}

/// Validate the parts of a profile geoclue needs.
fn validate(profile: &Profile) -> Result<(), BackendError> {
    let problem = if !profile.lat.is_finite() || !(-90.0..=90.0).contains(&profile.lat) {
        Some(format!("latitude {} out of range", profile.lat))
    } else if !profile.lon.is_finite() || !(-180.0..=180.0).contains(&profile.lon) {
        Some(format!("longitude {} out of range", profile.lon))
    } else if !profile.accuracy_m.is_finite() || profile.accuracy_m <= 0.0 {
        Some(format!("accuracy {} m must be a positive number", profile.accuracy_m))
    } else {
        None
    };
    match problem {
        Some(detail) => Err(BackendError::Other { backend: NAME.to_string(), detail }),
        None => Ok(()),
    }
}

/// Render `/etc/geolocation`: latitude, longitude, altitude (m), accuracy (m).
fn render_geolocation(lat: f64, lon: f64, accuracy_m: f64) -> String {
    format!(
        "# Written by geoveil. Restored when geoveil disengages.\n{lat:.6}\n{lon:.6}\n{alt:.6}\n{accuracy_m:.6}\n",
        alt = 0.0
    )
}

/// Numbers of a geolocation file, skipping comments and blank lines.
fn parse_geolocation(contents: &str) -> Option<Vec<f64>> {
    contents
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.parse::<f64>().ok())
        .collect()
}

/// Map a filesystem error to a [`BackendError`] with path context.
fn io_error(action: &str, path: &Path, err: &io::Error) -> BackendError {
    if err.kind() == io::ErrorKind::PermissionDenied {
        BackendError::NeedsPrivilege {
            backend: NAME.to_string(),
            detail: format!("cannot {action} {}: {err}", path.display()),
        }
    } else {
        BackendError::Other { backend: NAME.to_string(), detail: format!("cannot {action} {}: {err}", path.display()) }
    }
}

/// Fail if `path` is a symlink; absent is fine.
fn refuse_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(io::Error::other("refusing to follow a symlink at the target path"))
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read a file, `None` if absent. Refuses symlinks.
fn read_optional(path: &Path) -> io::Result<Option<String>> {
    refuse_symlink(path)?;
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Delete a file; absent is fine. Never follows a symlink (removes the link itself).
fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Atomically replace `path` with `contents` (mode 0644 on Unix).
///
/// Writes `.<name>.geoveil-tmp` in the same directory with `O_EXCL`
/// (so a planted symlink is not followed), fsyncs, then renames over the
/// target. Parent directories are created as needed.
fn atomic_write(path: &Path, contents: &str) -> io::Result<()> {
    refuse_symlink(path)?;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target path has no parent directory"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target path has no file name"))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.geoveil-tmp", file_name.to_string_lossy()));
    remove_if_present(&tmp)?;
    let result = write_new_file(&tmp, contents).and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    sync_dir(dir);
    Ok(())
}

/// Create a new file (failing if anything exists there), write and fsync it.
fn write_new_file(path: &Path, contents: &str) -> io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o644);
    }
    let mut file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Explicit chmod: the open() mode is filtered by the umask.
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

/// Best-effort fsync of a directory so the rename is durable.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Err(e) = fs::File::open(dir).and_then(|d| d.sync_all()) {
            tracing::debug!(error = %e, dir = %dir.display(), "directory fsync failed");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

impl Backend for LocationBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn snapshot(&self) -> Result<BackendSnapshot, BackendError> {
        let geoclue_dir = self.root.join(GEOCLUE_DIR_REL);
        if !geoclue_dir.is_dir() {
            return Err(BackendError::Unsupported {
                backend: NAME.to_string(),
                detail: format!("geoclue does not appear to be installed ({} missing)", geoclue_dir.display()),
            });
        }
        self.check_geoclue_version()?;
        let geo_path = self.geolocation_path();
        let dropin_path = self.dropin_path();
        let saved = LocationSnapshot {
            geolocation: read_optional(&geo_path).map_err(|e| io_error("read", &geo_path, &e))?,
            dropin: read_optional(&dropin_path).map_err(|e| io_error("read", &dropin_path, &e))?,
        };
        let data = serde_json::to_value(saved)
            .map_err(|e| BackendError::Other { backend: NAME.to_string(), detail: format!("serialize snapshot: {e}") })?;
        Ok(BackendSnapshot { backend: NAME.to_string(), data })
    }

    fn apply(&self, profile: &Profile) -> Result<(), BackendError> {
        validate(profile)?;
        let geo_path = self.geolocation_path();
        let dropin_path = self.dropin_path();
        atomic_write(&geo_path, &render_geolocation(profile.lat, profile.lon, profile.accuracy_m))
            .map_err(|e| io_error("write", &geo_path, &e))?;
        atomic_write(&dropin_path, DROPIN_CONTENTS).map_err(|e| io_error("write", &dropin_path, &e))?;
        self.restart_geoclue();
        Ok(())
    }

    fn restore(&self, snapshot: &BackendSnapshot) -> Result<(), BackendError> {
        check_snapshot_owner(NAME, &snapshot.backend)?;
        let saved: LocationSnapshot = serde_json::from_value(snapshot.data.clone())
            .map_err(|e| BackendError::Other { backend: NAME.to_string(), detail: format!("invalid snapshot data: {e}") })?;
        // Attempt both files even if the first fails, then report the first error.
        let geo = self.restore_file(&self.geolocation_path(), saved.geolocation.as_deref());
        let dropin = self.restore_file(&self.dropin_path(), saved.dropin.as_deref());
        self.restart_geoclue();
        geo.and(dropin)
    }

    fn verify(&self, profile: &Profile) -> Result<VerifyReport, BackendError> {
        const NOTE: &str =
            "checks geoveil's files only; geoclue applies them when it restarts; only apps that use geoclue see this location";
        let geo_path = self.geolocation_path();
        let dropin_path = self.dropin_path();
        let geo = read_optional(&geo_path).map_err(|e| io_error("read", &geo_path, &e))?;
        let dropin = read_optional(&dropin_path).map_err(|e| io_error("read", &dropin_path, &e))?;

        let dropin_ok = dropin.as_deref() == Some(DROPIN_CONTENTS);
        let (coords_ok, coords_detail) = match geo.as_deref().and_then(parse_geolocation) {
            Some(v) if v.len() >= 4 => {
                let ok = (v[0] - profile.lat).abs() <= COORD_EPSILON
                    && (v[1] - profile.lon).abs() <= COORD_EPSILON
                    && (v[3] - profile.accuracy_m).abs() <= COORD_EPSILON;
                (ok, format!("{} reports {:.6},{:.6} ±{} m", geo_path.display(), v[0], v[1], v[3]))
            }
            Some(_) => (false, format!("{} is incomplete", geo_path.display())),
            None if geo.is_none() => (false, format!("{} is missing", geo_path.display())),
            None => (false, format!("{} is not parseable", geo_path.display())),
        };
        let dropin_detail = if dropin_ok { "static source enabled" } else { "geoveil drop-in missing or modified" };
        Ok(VerifyReport {
            backend: NAME.to_string(),
            ok: coords_ok && dropin_ok,
            detail: format!(
                "{coords_detail}, expected {:.6},{:.6} ±{} m; {dropin_detail} ({NOTE})",
                profile.lat, profile.lon, profile.accuracy_m
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CommandOutput, FakeRunner};
    use geoveil_core::ProfileSource;

    fn profile() -> Profile {
        Profile {
            lat: 52.52,
            lon: 13.405,
            accuracy_m: 5000.0,
            timezone: "Europe/Berlin".to_string(),
            locale: "de-DE".to_string(),
            country: "DE".to_string(),
            source: ProfileSource::Manual,
        }
    }

    /// Temp root with `etc/geoclue` present (geoclue "installed").
    fn setup() -> (tempfile::TempDir, Arc<FakeRunner>, LocationBackend) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("etc/geoclue")).unwrap();
        let fake = Arc::new(FakeRunner::new());
        let backend = LocationBackend::with_root(fake.clone(), dir.path());
        (dir, fake, backend)
    }

    #[test]
    fn renders_six_decimals() {
        let s = render_geolocation(52.52, -13.4, 5000.0);
        let numbers: Vec<&str> = s.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(numbers, vec!["52.520000", "-13.400000", "0.000000", "5000.000000"]);
        assert_eq!(parse_geolocation(&s), Some(vec![52.52, -13.4, 0.0, 5000.0]));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(parse_geolocation("abc\n1\n"), None);
        assert_eq!(parse_geolocation("# only comment\n"), Some(vec![]));
    }

    /// Calls other than the geoclue version probe.
    fn non_probe_calls(fake: &FakeRunner) -> Vec<String> {
        fake.calls().into_iter().filter(|c| !c.ends_with("--version")).collect()
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("2.7.1\n"), Some((2, 7)));
        assert_eq!(parse_version("geoclue 2.6.0"), Some((2, 6)));
        assert_eq!(parse_version("2.10"), Some((2, 10)));
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version("7"), None);
    }

    #[test]
    fn old_geoclue_is_unsupported() {
        let (_dir, fake, b) = setup();
        fake.push_stdout("2.6.0\n");
        match b.snapshot() {
            Err(BackendError::Unsupported { detail, .. }) => assert!(detail.contains("2.6"), "{detail}"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(fake.calls(), vec!["/usr/libexec/geoclue --version"]);
    }

    #[test]
    fn new_geoclue_found_at_later_path_is_accepted() {
        let (_dir, fake, b) = setup();
        fake.push_err(io::ErrorKind::NotFound);
        fake.push_stdout("2.7.2\n");
        assert!(b.snapshot().is_ok());
        assert_eq!(
            fake.calls(),
            vec!["/usr/libexec/geoclue --version", "/usr/lib/geoclue-2.0/geoclue --version"]
        );
    }

    #[test]
    fn unknown_geoclue_version_proceeds() {
        let (_dir, fake, b) = setup();
        for _ in GEOCLUE_BINARIES {
            fake.push_err(io::ErrorKind::NotFound);
        }
        assert!(b.snapshot().is_ok());
    }

    #[test]
    fn snapshot_without_geoclue_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let b = LocationBackend::with_root(Arc::new(FakeRunner::new()), dir.path());
        assert!(matches!(b.snapshot(), Err(BackendError::Unsupported { .. })));
    }

    #[test]
    fn snapshot_records_absence() {
        let (_dir, _fake, b) = setup();
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.backend, "location");
        assert_eq!(snap.data, serde_json::json!({ "geolocation": null, "dropin": null }));
    }

    #[test]
    fn snapshot_records_contents() {
        let (dir, _fake, b) = setup();
        fs::write(dir.path().join(GEOLOCATION_REL), "1\n2\n3\n4\n").unwrap();
        let snap = b.snapshot().unwrap();
        assert_eq!(snap.data, serde_json::json!({ "geolocation": "1\n2\n3\n4\n", "dropin": null }));
    }

    #[test]
    fn apply_writes_files_and_restarts_geoclue() {
        let (dir, fake, b) = setup();
        b.apply(&profile()).unwrap();
        let geo = fs::read_to_string(dir.path().join(GEOLOCATION_REL)).unwrap();
        assert_eq!(parse_geolocation(&geo), Some(vec![52.52, 13.405, 0.0, 5000.0]));
        assert!(geo.contains("\n52.520000\n13.405000\n0.000000\n5000.000000\n"));
        let dropin = fs::read_to_string(dir.path().join(DROPIN_REL)).unwrap();
        assert!(dropin.contains("[static-source]\nenable=true"));
        for section in ["wifi", "3g", "cdma", "modem-gps", "network-nmea", "compass"] {
            assert!(dropin.contains(&format!("[{section}]\nenable=false")), "{section}");
        }
        assert_eq!(fake.calls(), vec!["systemctl restart geoclue"]);
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(dir.path().join("etc"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".geoveil-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn apply_tolerates_restart_failure() {
        let (_dir, fake, b) = setup();
        fake.push(CommandOutput { status: 5, stdout: String::new(), stderr: "Unit geoclue.service not found.".into() });
        assert!(b.apply(&profile()).is_ok());
        fake.push_err(io::ErrorKind::NotFound);
        assert!(b.apply(&profile()).is_ok());
    }

    #[test]
    fn apply_is_idempotent() {
        let (dir, _fake, b) = setup();
        b.apply(&profile()).unwrap();
        b.apply(&profile()).unwrap();
        let geo = fs::read_to_string(dir.path().join(GEOLOCATION_REL)).unwrap();
        assert_eq!(geo, render_geolocation(52.52, 13.405, 5000.0));
    }

    #[test]
    fn apply_rejects_invalid_profile() {
        let (dir, fake, b) = setup();
        for (lat, lon, acc) in [(91.0, 0.0, 10.0), (0.0, 181.0, 10.0), (f64::NAN, 0.0, 10.0), (0.0, 0.0, 0.0)] {
            let p = Profile { lat, lon, accuracy_m: acc, ..profile() };
            assert!(b.apply(&p).is_err());
        }
        assert!(!dir.path().join(GEOLOCATION_REL).exists());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn restore_to_absent_deletes_files() {
        let (dir, fake, b) = setup();
        let snap = b.snapshot().unwrap();
        b.apply(&profile()).unwrap();
        b.restore(&snap).unwrap();
        assert!(!dir.path().join(GEOLOCATION_REL).exists());
        assert!(!dir.path().join(DROPIN_REL).exists());
        assert_eq!(non_probe_calls(&fake), vec!["systemctl restart geoclue", "systemctl restart geoclue"]);
    }

    #[test]
    fn restore_to_previous_contents() {
        let (dir, _fake, b) = setup();
        let original_geo = "# admin's own\n10.0\n20.0\n5\n100\n";
        let original_dropin = "[static-source]\nenable=false\n";
        fs::write(dir.path().join(GEOLOCATION_REL), original_geo).unwrap();
        fs::create_dir_all(dir.path().join("etc/geoclue/conf.d")).unwrap();
        fs::write(dir.path().join(DROPIN_REL), original_dropin).unwrap();

        let snap = b.snapshot().unwrap();
        b.apply(&profile()).unwrap();
        assert_ne!(fs::read_to_string(dir.path().join(GEOLOCATION_REL)).unwrap(), original_geo);
        b.restore(&snap).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join(GEOLOCATION_REL)).unwrap(), original_geo);
        assert_eq!(fs::read_to_string(dir.path().join(DROPIN_REL)).unwrap(), original_dropin);
    }

    #[test]
    fn restore_survives_json_roundtrip() {
        let (dir, _fake, b) = setup();
        fs::write(dir.path().join(GEOLOCATION_REL), "1\n2\n0\n9\n").unwrap();
        let snap = b.snapshot().unwrap();
        let json = serde_json::to_string(&snap).unwrap();
        let back: BackendSnapshot = serde_json::from_str(&json).unwrap();
        b.apply(&profile()).unwrap();
        b.restore(&back).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join(GEOLOCATION_REL)).unwrap(), "1\n2\n0\n9\n");
        assert!(!dir.path().join(DROPIN_REL).exists());
    }

    #[test]
    fn restore_rejects_bad_snapshot() {
        let (_dir, _fake, b) = setup();
        let wrong = BackendSnapshot { backend: "timezone".into(), data: serde_json::json!({}) };
        assert!(b.restore(&wrong).is_err());
        let bad = BackendSnapshot { backend: "location".into(), data: serde_json::json!({ "geolocation": 5 }) };
        assert!(b.restore(&bad).is_err());
    }

    #[test]
    fn verify_before_and_after_apply() {
        let (_dir, _fake, b) = setup();
        let before = b.verify(&profile()).unwrap();
        assert!(!before.ok);
        assert!(before.detail.contains("missing"));
        b.apply(&profile()).unwrap();
        let after = b.verify(&profile()).unwrap();
        assert!(after.ok, "{}", after.detail);
        assert!(after.detail.contains("geoclue"));
        let other = Profile { lat: 48.85, lon: 2.35, ..profile() };
        assert!(!b.verify(&other).unwrap().ok);
    }

    #[test]
    fn verify_detects_tampered_dropin() {
        let (dir, _fake, b) = setup();
        b.apply(&profile()).unwrap();
        fs::write(dir.path().join(DROPIN_REL), "[static-source]\nenable=false\n").unwrap();
        assert!(!b.verify(&profile()).unwrap().ok);
    }

    #[cfg(unix)]
    #[test]
    fn written_files_are_0644() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _fake, b) = setup();
        b.apply(&profile()).unwrap();
        for rel in [GEOLOCATION_REL, DROPIN_REL] {
            let mode = fs::metadata(dir.path().join(rel)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "{rel}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_target() {
        let (dir, _fake, b) = setup();
        let victim = dir.path().join("victim");
        fs::write(&victim, "precious").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join(GEOLOCATION_REL)).unwrap();
        assert!(b.snapshot().is_err());
        assert!(b.apply(&profile()).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
    }
}
