//! Shared data types. These are the contracts between core, platform and CLI.

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// Where a profile came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileSource {
    /// Resolved from the VPN exit IP.
    VpnExit { ip: IpAddr },
    /// User gave coordinates or an IP manually.
    Manual,
}

/// The location identity applied to the whole system.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub lat: f64,
    pub lon: f64,
    /// Reported horizontal accuracy in metres.
    pub accuracy_m: f64,
    /// IANA zone, e.g. "Europe/Berlin".
    pub timezone: String,
    /// BCP-47 tag, e.g. "de-DE".
    pub locale: String,
    /// ISO-3166 alpha-2, upper case, e.g. "DE".
    pub country: String,
    pub source: ProfileSource,
}

/// Opaque per-backend saved state, used to restore original settings.
/// Each backend chooses its own JSON shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendSnapshot {
    pub backend: String,
    pub data: serde_json::Value,
}

/// Result of reading back what the OS reports after apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub backend: String,
    pub ok: bool,
    /// Human-readable: what the OS currently reports vs expected.
    pub detail: String,
}

/// One network interface as seen by the platform layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetInterface {
    pub name: String,
    /// Windows adapter description / friendly name; empty elsewhere if unknown.
    pub description: String,
    pub is_up: bool,
    pub addrs: Vec<IpAddr>,
}

/// VPN state transitions emitted by the detector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum VpnEvent {
    Up { exit_ip: IpAddr, interfaces: Vec<String> },
    Changed { exit_ip: IpAddr, interfaces: Vec<String> },
    Down,
}
