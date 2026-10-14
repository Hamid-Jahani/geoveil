//! Offline timezone helpers: coordinates -> IANA zone, and IANA <-> Windows zone ids.
//!
//! * [`tz_for_coords`] uses the `tzf-rs` polygon data compiled into the binary.
//! * [`iana_to_windows`] / [`windows_to_iana`] use the embedded CLDR
//!   `windowsZones.xml` table in [`windows_zones`].

mod windows_zones;

use std::collections::HashMap;
use std::sync::OnceLock;

use tzf_rs::DefaultFinder;

use windows_zones::{ALIASES, IANA_TO_WINDOWS, WINDOWS_TO_IANA};

/// Largest `N` accepted in `Etc/GMT+N` (UTC-12).
const ETC_GMT_PLUS_MAX: u8 = 12;
/// Largest `N` accepted in `Etc/GMT-N` (UTC+14).
const ETC_GMT_MINUS_MAX: u8 = 14;

/// Shared finder; building it parses the embedded polygon data, which is slow,
/// so it happens once on first use.
fn finder() -> &'static DefaultFinder {
    static FINDER: OnceLock<DefaultFinder> = OnceLock::new();
    FINDER.get_or_init(DefaultFinder::new)
}

/// IANA -> Windows lookup map built from the CLDR rows plus tzdata aliases.
fn iana_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut map = HashMap::with_capacity(IANA_TO_WINDOWS.len() + ALIASES.len());
        // Aliases first so that genuine CLDR rows win on any (unexpected) overlap.
        map.extend(ALIASES.iter().copied());
        map.extend(IANA_TO_WINDOWS.iter().copied());
        map
    })
}

/// Returns true when `lat`/`lon` are finite and inside the valid WGS-84 ranges.
fn coords_valid(lat: f64, lon: f64) -> bool {
    lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
}

/// Looks up the IANA timezone for a coordinate, fully offline.
///
/// Returns `None` for out-of-range or non-finite coordinates, or when the
/// dataset has no zone for the point. Points at sea may yield nautical zones
/// such as `"Etc/GMT+5"`; those are returned as-is.
pub fn tz_for_coords(lat: f64, lon: f64) -> Option<String> {
    if !coords_valid(lat, lon) {
        return None;
    }
    // tzf-rs takes (longitude, latitude).
    let name = finder().get_tz_name(lon, lat);
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// Maps an IANA zone name to its Windows timezone id (CLDR `windowsZones.xml`).
///
/// Every zone listed for any territory is covered, plus tzdata link names
/// (e.g. both `Asia/Kolkata` and `Asia/Calcutta`). Matching is exact first,
/// then ASCII case-insensitive.
pub fn iana_to_windows(iana: &str) -> Option<&'static str> {
    let iana = iana.trim();
    let map = iana_map();
    if let Some(&win) = map.get(iana) {
        return Some(win);
    }
    map.iter()
        .find(|(zone, _)| zone.eq_ignore_ascii_case(iana))
        .map(|(_, win)| *win)
}

/// Maps a Windows timezone id to its canonical IANA zone: the CLDR
/// territory `001` ("golden zone") entry, e.g. `"W. Europe Standard Time"` ->
/// `"Europe/Berlin"`. Matching is ASCII case-insensitive.
///
/// Note that CLDR golden zones sometimes use legacy-but-valid tzdata names,
/// e.g. `"India Standard Time"` -> `"Asia/Calcutta"`.
pub fn windows_to_iana(windows_id: &str) -> Option<&'static str> {
    let windows_id = windows_id.trim();
    WINDOWS_TO_IANA
        .iter()
        .find(|(win, _)| win.eq_ignore_ascii_case(windows_id))
        .map(|(_, zone)| *zone)
}

/// Returns true for names known to the embedded table (CLDR zones and tzdata
/// aliases), `UTC`, `Etc/UTC`, and in-range `Etc/GMT+N` (N <= 12) /
/// `Etc/GMT-N` (N <= 14). Matching is exact (IANA names are case-sensitive).
pub fn is_valid_iana(name: &str) -> bool {
    name == "UTC"
        || name == "Etc/UTC"
        || is_etc_gmt_offset(name)
        || iana_map().contains_key(name)
}

/// Longest zone name accepted by [`is_safe_zone_name`].
const MAX_ZONE_NAME_LEN: usize = 128;

/// Shape check for a zone name read back from the OS (e.g. an original
/// setting saved in a snapshot), as opposed to [`is_valid_iana`], which only
/// knows the embedded table.
///
/// Restoring must never be stricter than snapshotting: the system may be
/// configured with a perfectly valid tzdata name the table does not list
/// (`Antarctica/Troll`, `Factory`, new zones). This accepts
/// `[A-Za-z0-9/_+-]`, at most 128 characters, no leading `-` or `/`, no empty
/// path segment, and never `.` (so no `..`). Such a value is always passed to
/// the OS tool as a single argument (never through a shell), so it cannot
/// inject anything.
pub fn is_safe_zone_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_ZONE_NAME_LEN
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'+' | b'-'))
}

/// Parses `Etc/GMT`, `Etc/GMT+N`, `Etc/GMT-N` with `N` in the tzdata range.
fn is_etc_gmt_offset(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("Etc/GMT") else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let (max, digits) = if let Some(d) = rest.strip_prefix('+') {
        (ETC_GMT_PLUS_MAX, d)
    } else if let Some(d) = rest.strip_prefix('-') {
        (ETC_GMT_MINUS_MAX, d)
    } else {
        return false;
    };
    let well_formed = !digits.is_empty()
        && digits.len() <= 2
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !(digits.len() == 2 && digits.starts_with('0'));
    well_formed && digits.parse::<u8>().is_ok_and(|n| n <= max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coords_to_zone_major_cities() {
        let cases = [
            ((52.52, 13.405), "Europe/Berlin"),
            ((40.7128, -74.0060), "America/New_York"),
            ((35.6892, 51.3890), "Asia/Tehran"),
            ((35.6762, 139.6503), "Asia/Tokyo"),
            ((-33.8688, 151.2093), "Australia/Sydney"),
        ];
        for ((lat, lon), want) in cases {
            assert_eq!(tz_for_coords(lat, lon).as_deref(), Some(want), "{lat},{lon}");
        }
    }

    #[test]
    fn invalid_coords_are_none() {
        assert_eq!(tz_for_coords(91.0, 0.0), None);
        assert_eq!(tz_for_coords(-90.5, 0.0), None);
        assert_eq!(tz_for_coords(0.0, 180.5), None);
        assert_eq!(tz_for_coords(0.0, -181.0), None);
        assert_eq!(tz_for_coords(f64::NAN, 0.0), None);
        assert_eq!(tz_for_coords(0.0, f64::INFINITY), None);
    }

    #[test]
    fn iana_to_windows_samples() {
        let cases = [
            ("Europe/Berlin", "W. Europe Standard Time"),
            ("Europe/Amsterdam", "W. Europe Standard Time"),
            ("America/New_York", "Eastern Standard Time"),
            ("America/Indiana/Indianapolis", "US Eastern Standard Time"),
            ("America/Indianapolis", "US Eastern Standard Time"),
            ("Asia/Tehran", "Iran Standard Time"),
            ("Asia/Tokyo", "Tokyo Standard Time"),
            ("Australia/Sydney", "AUS Eastern Standard Time"),
            ("Etc/UTC", "UTC"),
            ("UTC", "UTC"),
            ("Etc/GMT+5", "SA Pacific Standard Time"),
        ];
        for (iana, win) in cases {
            assert_eq!(iana_to_windows(iana), Some(win), "{iana}");
        }
    }

    #[test]
    fn iana_to_windows_aliases() {
        let cases = [
            ("Asia/Kolkata", "India Standard Time"),
            ("Asia/Calcutta", "India Standard Time"),
            ("Europe/Kiev", "FLE Standard Time"),
            ("Europe/Kyiv", "FLE Standard Time"),
            ("America/Buenos_Aires", "Argentina Standard Time"),
            ("America/Argentina/Buenos_Aires", "Argentina Standard Time"),
            ("Asia/Saigon", "SE Asia Standard Time"),
            ("Asia/Ho_Chi_Minh", "SE Asia Standard Time"),
            ("US/Eastern", "Eastern Standard Time"),
        ];
        for (iana, win) in cases {
            assert_eq!(iana_to_windows(iana), Some(win), "{iana}");
        }
    }

    #[test]
    fn iana_to_windows_unknown_and_case() {
        assert_eq!(iana_to_windows("Mars/Olympus_Mons"), None);
        assert_eq!(iana_to_windows(""), None);
        assert_eq!(iana_to_windows("europe/berlin"), Some("W. Europe Standard Time"));
    }

    #[test]
    fn windows_to_iana_samples() {
        assert_eq!(windows_to_iana("W. Europe Standard Time"), Some("Europe/Berlin"));
        assert_eq!(windows_to_iana("Eastern Standard Time"), Some("America/New_York"));
        assert_eq!(windows_to_iana("Iran Standard Time"), Some("Asia/Tehran"));
        assert_eq!(windows_to_iana("Tokyo Standard Time"), Some("Asia/Tokyo"));
        assert_eq!(windows_to_iana("India Standard Time"), Some("Asia/Calcutta"));
        assert_eq!(windows_to_iana("UTC"), Some("Etc/UTC"));
        assert_eq!(windows_to_iana("tokyo standard time"), Some("Asia/Tokyo"));
        assert_eq!(windows_to_iana("Nowhere Standard Time"), None);
    }

    #[test]
    fn every_golden_zone_round_trips() {
        assert!(WINDOWS_TO_IANA.len() > 100);
        for (win, zone) in WINDOWS_TO_IANA {
            assert_eq!(windows_to_iana(win), Some(*zone), "{win}");
            assert_eq!(iana_to_windows(zone), Some(*win), "{zone}");
            assert!(is_valid_iana(zone), "{zone}");
        }
    }

    #[test]
    fn windows_ids_are_unique_in_golden_table() {
        let mut seen = std::collections::HashSet::new();
        for (win, _) in WINDOWS_TO_IANA {
            assert!(seen.insert(*win), "duplicate {win}");
        }
    }

    /// Diagnostic: lists tzf-rs zones without a Windows mapping (data lag
    /// between tzf and CLDR). Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn every_tzf_zone_has_windows_mapping() {
        let missing: Vec<&str> = finder()
            .timezonenames()
            .into_iter()
            .filter(|z| iana_to_windows(z).is_none())
            .collect();
        assert!(missing.is_empty(), "no Windows id for: {missing:?}");
    }

    /// Windows ids captured from a real Windows 11 registry.
    fn real_windows_ids() -> std::collections::HashSet<&'static str> {
        include_str!("windows_tz_ids.txt")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect()
    }

    #[test]
    fn every_windows_id_exists_on_windows() {
        let real = real_windows_ids();
        assert!(real.len() > 130);
        let tables = [IANA_TO_WINDOWS, ALIASES];
        for (zone, win) in tables.iter().flat_map(|t| t.iter()) {
            assert!(real.contains(win), "{zone} -> `{win}` is not a Windows time zone id");
        }
        for (win, _) in WINDOWS_TO_IANA {
            assert!(real.contains(win), "golden row `{win}` is not a Windows time zone id");
        }
    }

    #[test]
    fn canadian_zones_use_standard_windows_ids() {
        for (zone, win) in [
            ("America/Vancouver", "Pacific Standard Time"),
            ("Canada/Pacific", "Pacific Standard Time"),
            ("America/Edmonton", "Mountain Standard Time"),
            ("America/Yellowknife", "Mountain Standard Time"),
            ("America/Inuvik", "Mountain Standard Time"),
            ("America/Winnipeg", "Central Standard Time"),
            ("America/Rainy_River", "Central Standard Time"),
        ] {
            assert_eq!(iana_to_windows(zone), Some(win), "{zone}");
        }
    }

    #[test]
    fn legacy_and_backward_names_are_known() {
        for zone in [
            "GMT",
            "EST5EDT",
            "CST6CDT",
            "MST7MDT",
            "PST8PDT",
            "Europe/Uzhgorod",
            "Europe/Zaporozhye",
            "America/Fort_Wayne",
            "US/East-Indiana",
            "America/Rosario",
        ] {
            assert!(is_valid_iana(zone), "{zone}");
            assert!(iana_to_windows(zone).is_some(), "{zone}");
        }
    }

    #[test]
    fn safe_zone_name_shape() {
        for ok in [
            "GMT",
            "Factory",
            "Antarctica/Troll",
            "America/Argentina/Buenos_Aires",
            "Etc/GMT+5",
            "Etc/GMT-14",
            "America/Port-au-Prince",
            "PST8PDT",
        ] {
            assert!(is_safe_zone_name(ok), "{ok}");
        }
        let too_long = "A".repeat(129);
        for bad in [
            "",
            "-rf",
            "/etc/passwd",
            "../../etc/passwd",
            "Europe/../x",
            "Europe//Berlin",
            "Europe/",
            "Europe/Berlin; rm",
            "Europe/Berlin\n",
            "Europe Berlin",
            "Europe/Berlín",
            too_long.as_str(),
        ] {
            assert!(!is_safe_zone_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn validity() {
        for ok in [
            "Europe/Berlin",
            "Asia/Kolkata",
            "Asia/Calcutta",
            "UTC",
            "Etc/UTC",
            "Etc/GMT",
            "Etc/GMT+12",
            "Etc/GMT-14",
            "Etc/GMT+0",
            "Etc/GMT-3",
        ] {
            assert!(is_valid_iana(ok), "{ok}");
        }
        for bad in [
            "",
            "Mars/Base",
            "Etc/GMT+13",
            "Etc/GMT-15",
            "Etc/GMT+",
            "Etc/GMT+05",
            "Etc/GMT+1a",
            "Etc/GMTx",
            "europe/berlin",
        ] {
            assert!(!is_valid_iana(bad), "{bad}");
        }
    }
}
