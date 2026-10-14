//! Command-line definition (clap derive) and pure helpers derived from it.

use clap::{ArgAction, Args, Parser, Subcommand};
use std::net::IpAddr;
use std::path::PathBuf;

/// Accuracy reported for manually entered coordinates when `--accuracy` is not given.
pub const DEFAULT_MANUAL_ACCURACY_M: f64 = 100.0;

/// Geoveil: make the whole OS match your VPN exit location.
#[derive(Debug, Parser)]
#[command(name = "geoveil", version, about, long_about = None)]
pub struct Cli {
    /// Print machine-readable JSON instead of human-readable lines.
    #[arg(long, global = true)]
    pub json: bool,

    /// Also spoof the system locale / region (opt-in).
    #[arg(long, global = true)]
    pub locale: bool,

    /// Also drive the OS location service where supported.
    #[arg(long, global = true)]
    pub location: bool,

    /// Directory holding the snapshot file (default: <local data dir>/geoveil).
    #[arg(long, global = true, value_name = "PATH")]
    pub state_dir: Option<PathBuf>,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace). RUST_LOG overrides.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Timeout for each IP-geolocation provider request, in seconds.
    #[arg(long, global = true, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=120))]
    pub provider_timeout_secs: u64,

    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show network interfaces, VPN detection and the resolved profile. Changes nothing.
    Detect,
    /// Apply a location profile to the system once.
    Apply(ApplyArgs),
    /// Restore the original system settings saved by `apply` / `run`.
    Restore,
    /// Show whether a profile is engaged and verify the system state.
    Status,
    /// Daemon: follow the VPN up / down / exit changes automatically.
    Run(RunArgs),
}

/// Arguments for `apply`.
#[derive(Debug, Clone, Args)]
pub struct ApplyArgs {
    /// Latitude in degrees (-90..=90). Requires --lon.
    #[arg(long, requires = "lon", allow_negative_numbers = true, value_parser = parse_lat)]
    pub lat: Option<f64>,

    /// Longitude in degrees (-180..=180). Requires --lat.
    #[arg(long, requires = "lat", allow_negative_numbers = true, value_parser = parse_lon)]
    pub lon: Option<f64>,

    /// Reported accuracy in metres for manual coordinates. Requires --lat/--lon.
    #[arg(long, requires = "lat", value_parser = parse_accuracy)]
    pub accuracy: Option<f64>,

    /// Resolve this IP instead of the current public (VPN exit) IP.
    #[arg(long, conflicts_with_all = ["lat", "lon", "accuracy"])]
    pub ip: Option<IpAddr>,
}

/// Arguments for `run`.
#[derive(Debug, Clone, Args)]
pub struct RunArgs {
    /// Seconds between interface polls.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub interval_secs: u64,

    /// Keep the spoofed settings when the VPN goes down or on Ctrl-C (warn instead of restoring).
    #[arg(long)]
    pub sticky: bool,
}

/// How `apply` should obtain its profile.
#[derive(Debug, Clone, PartialEq)]
pub enum ProfileRequest {
    /// User-supplied coordinates.
    Manual { lat: f64, lon: f64, accuracy_m: f64 },
    /// Resolve a specific IP address.
    Ip(IpAddr),
    /// Resolve the current public IP (normally the VPN exit).
    PublicIp,
}

impl ApplyArgs {
    /// Decide where the profile comes from. Clap guarantees `lat` and `lon`
    /// appear together and never alongside `ip`.
    pub fn profile_request(&self) -> ProfileRequest {
        match (self.lat, self.lon, self.ip) {
            (Some(lat), Some(lon), _) => ProfileRequest::Manual {
                lat,
                lon,
                accuracy_m: self.accuracy.unwrap_or(DEFAULT_MANUAL_ACCURACY_M),
            },
            (_, _, Some(ip)) => ProfileRequest::Ip(ip),
            _ => ProfileRequest::PublicIp,
        }
    }
}

/// Tracing filter directive for a verbosity count. The daemon logs at least `info`.
pub fn log_filter(verbose: u8, daemon: bool) -> &'static str {
    match (verbose, daemon) {
        (0, false) => "warn",
        (0, true) | (1, _) => "info",
        (2, _) => "debug",
        _ => "trace",
    }
}

fn parse_ranged(s: &str, what: &str, min: f64, max: f64) -> Result<f64, String> {
    let v: f64 = s.trim().parse().map_err(|_| format!("`{s}` is not a number"))?;
    if !v.is_finite() || v < min || v > max {
        return Err(format!("{what} must be between {min} and {max}, got {v}"));
    }
    Ok(v)
}

fn parse_lat(s: &str) -> Result<f64, String> {
    parse_ranged(s, "latitude", -90.0, 90.0)
}

fn parse_lon(s: &str) -> Result<f64, String> {
    parse_ranged(s, "longitude", -180.0, 180.0)
}

fn parse_accuracy(s: &str) -> Result<f64, String> {
    let v = parse_ranged(s, "accuracy", 0.0, 1_000_000.0)?;
    if v == 0.0 {
        return Err("accuracy must be greater than 0".to_string());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("geoveil").chain(args.iter().copied()))
    }

    fn apply_args(args: &[&str]) -> ApplyArgs {
        match parse(args).expect("should parse").command {
            Command::Apply(a) => a,
            other => panic!("expected apply, got {other:?}"),
        }
    }

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_flags_defaults() {
        let cli = parse(&["status"]).unwrap();
        assert!(!cli.json && !cli.locale && !cli.location);
        assert_eq!(cli.verbose, 0);
        assert_eq!(cli.provider_timeout_secs, 5);
        assert!(cli.state_dir.is_none());
    }

    #[test]
    fn global_flags_after_subcommand() {
        let cli = parse(&["detect", "--json", "--locale", "--location", "-vv", "--state-dir", "x/y"]).unwrap();
        assert!(cli.json && cli.locale && cli.location);
        assert_eq!(cli.verbose, 2);
        assert_eq!(cli.state_dir, Some(PathBuf::from("x/y")));
    }

    #[test]
    fn lat_without_lon_rejected() {
        assert!(parse(&["apply", "--lat", "10"]).is_err());
        assert!(parse(&["apply", "--lon", "10"]).is_err());
    }

    #[test]
    fn accuracy_without_coords_rejected() {
        assert!(parse(&["apply", "--accuracy", "50"]).is_err());
    }

    #[test]
    fn ip_conflicts_with_coords() {
        assert!(parse(&["apply", "--lat", "1", "--lon", "2", "--ip", "1.2.3.4"]).is_err());
    }

    #[test]
    fn out_of_range_coords_rejected() {
        assert!(parse(&["apply", "--lat", "91", "--lon", "0"]).is_err());
        assert!(parse(&["apply", "--lat", "0", "--lon", "-181"]).is_err());
        assert!(parse(&["apply", "--lat", "NaN", "--lon", "0"]).is_err());
        assert!(parse(&["apply", "--lat", "0", "--lon", "0", "--accuracy", "0"]).is_err());
    }

    #[test]
    fn negative_coords_accepted() {
        let a = apply_args(&["apply", "--lat", "-33.86", "--lon", "-70.65"]);
        assert_eq!(a.lat, Some(-33.86));
        assert_eq!(a.lon, Some(-70.65));
    }

    #[test]
    fn manual_request_with_default_accuracy() {
        let a = apply_args(&["apply", "--lat", "52.52", "--lon", "13.405"]);
        assert_eq!(
            a.profile_request(),
            ProfileRequest::Manual { lat: 52.52, lon: 13.405, accuracy_m: DEFAULT_MANUAL_ACCURACY_M }
        );
    }

    #[test]
    fn manual_request_with_accuracy() {
        let a = apply_args(&["apply", "--lat", "1", "--lon", "2", "--accuracy", "25"]);
        assert_eq!(a.profile_request(), ProfileRequest::Manual { lat: 1.0, lon: 2.0, accuracy_m: 25.0 });
    }

    #[test]
    fn ip_request() {
        let a = apply_args(&["apply", "--ip", "203.0.113.7"]);
        assert_eq!(a.profile_request(), ProfileRequest::Ip("203.0.113.7".parse().unwrap()));
    }

    #[test]
    fn public_ip_request_by_default() {
        assert_eq!(apply_args(&["apply"]).profile_request(), ProfileRequest::PublicIp);
    }

    #[test]
    fn run_defaults_and_sticky() {
        match parse(&["run"]).unwrap().command {
            Command::Run(r) => {
                assert_eq!(r.interval_secs, 5);
                assert!(!r.sticky);
            }
            other => panic!("expected run, got {other:?}"),
        }
        match parse(&["run", "--interval-secs", "10", "--sticky"]).unwrap().command {
            Command::Run(r) => {
                assert_eq!(r.interval_secs, 10);
                assert!(r.sticky);
            }
            other => panic!("expected run, got {other:?}"),
        }
        assert!(parse(&["run", "--interval-secs", "0"]).is_err());
    }

    #[test]
    fn verbosity_mapping() {
        assert_eq!(log_filter(0, false), "warn");
        assert_eq!(log_filter(0, true), "info");
        assert_eq!(log_filter(1, false), "info");
        assert_eq!(log_filter(2, true), "debug");
        assert_eq!(log_filter(7, false), "trace");
    }
}
