//! A session-scoped Windows Filtering Platform guard. The session owns all
//! filters, and commits them together so partial activation cannot cut access.

use std::ffi::{c_void, OsStr, OsString};
use std::io;
use std::net::SocketAddrV4;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::IpHelper::ConvertInterfaceIndexToLuid;
use windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::*;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_WINNT;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

// Fixed to refuse a second concurrent guard (duplicate sublayer).
const SUBLAYER: GUID = GUID::from_u128(0xe2a83f20_e5da_4ee8_9356_c112df75ab69);
const PERMIT_WEIGHT: u8 = 15;
const BLOCK_WEIGHT: u8 = 0;

pub struct Guard(HANDLE);

impl Drop for Guard {
    fn drop(&mut self) {
        // Dynamic-session objects are removed by BFE on close or process exit.
        unsafe { FwpmEngineClose0(self.0) };
    }
}

struct AppId(*mut FWP_BYTE_BLOB);

impl AppId {
    fn from_executable(path: &Path) -> io::Result<Self> {
        let mut path = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if path.contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "NUL in executable path"));
        }
        path.push(0);
        let mut value = null_mut();
        checked(unsafe { FwpmGetAppIdFromFileName0(path.as_ptr(), &mut value) }, "identify executable")?;
        Ok(Self(value))
    }

    fn condition(&self) -> FWPM_FILTER_CONDITION0 {
        condition(FWPM_CONDITION_ALE_APP_ID, FWP_BYTE_BLOB_TYPE, FWP_CONDITION_VALUE0_0 { byteBlob: self.0 })
    }
}

impl Drop for AppId {
    fn drop(&mut self) {
        let mut value = self.0.cast::<c_void>();
        unsafe { FwpmFreeMemory0(&mut value) };
    }
}

/// Resolve the adapter index as seen by netdev to the LUID required by WFP.
/// This operation does not modify firewall settings.
pub fn interface_luid(index: u32) -> io::Result<u64> {
    let mut luid = NET_LUID_LH::default();
    checked(unsafe { ConvertInterfaceIndexToLuid(index, &mut luid) }, "resolve tunnel interface")?;
    Ok(unsafe { luid.Value })
}

/// Keep the returned guard alive for as long as the filters should apply.
/// The VPN transport exception is bound to one executable, IPv4 address,
/// port, and transport protocol. No endpoint hostname is resolved here.
pub fn start(tunnel_luid: u64, client: &Path, peer: SocketAddrV4, protocol: u8) -> io::Result<Guard> {
    let client = AppId::from_executable(client)?;
    let dhcp = AppId::from_executable(&system_directory()?.join("svchost.exe"))?;
    let (_title, display) = display("Geoveil VPN protection");
    let session = FWPM_SESSION0 {
        displayData: display,
        flags: FWPM_SESSION_FLAG_DYNAMIC,
        txnWaitTimeoutInMSec: 3000,
        ..Default::default()
    };
    let mut handle = null_mut();
    checked(
        unsafe { FwpmEngineOpen0(null(), RPC_C_AUTHN_WINNT, null(), &session, &mut handle) },
        "open Windows Filtering Platform",
    )?;
    let guard = Guard(handle);
    checked(unsafe { FwpmTransactionBegin0(handle, 0) }, "begin firewall transaction")?;
    if let Err(error) = install_filters(&guard, tunnel_luid, &client, peer, protocol, &dhcp)
        .and_then(|()| checked(unsafe { FwpmTransactionCommit0(handle) }, "commit firewall transaction"))
    {
        unsafe { FwpmTransactionAbort0(handle) };
        return Err(error);
    }
    Ok(guard)
}

fn install_filters(
    guard: &Guard,
    mut tunnel_luid: u64,
    client: &AppId,
    peer: SocketAddrV4,
    protocol: u8,
    dhcp: &AppId,
) -> io::Result<()> {
    let (_title, display) = display("Geoveil VPN protection");
    let sublayer = FWPM_SUBLAYER0 {
        subLayerKey: SUBLAYER,
        displayData: display,
        weight: u16::MAX,
        ..Default::default()
    };
    checked(unsafe { FwpmSubLayerAdd0(guard.0, &sublayer, null_mut()) }, "add Geoveil filter sublayer")?;

    let layers = [
        (FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWPM_CONDITION_IP_NEXTHOP_INTERFACE, true),
        (FWPM_LAYER_ALE_AUTH_CONNECT_V6, FWPM_CONDITION_IP_NEXTHOP_INTERFACE, false),
        (FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4, FWPM_CONDITION_IP_ARRIVAL_INTERFACE, true),
        (FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6, FWPM_CONDITION_IP_ARRIVAL_INTERFACE, false),
    ];
    for (layer, interface_field, ipv4) in layers {
        let loopback = FWPM_FILTER_CONDITION0 {
            matchType: FWP_MATCH_FLAGS_ALL_SET,
            ..condition(
                FWPM_CONDITION_FLAGS,
                FWP_UINT32,
                FWP_CONDITION_VALUE0_0 { uint32: FWP_CONDITION_FLAG_IS_LOOPBACK },
            )
        };
        guard.filter(layer, "Allow loopback", true, &mut [loopback])?;
        let tunnel = condition(
            interface_field,
            FWP_UINT64,
            FWP_CONDITION_VALUE0_0 { uint64: &mut tunnel_luid },
        );
        guard.filter(layer, "Allow VPN adapter", true, &mut [tunnel])?;
        if ipv4 {
            guard.filter(layer, "Allow VPN transport", true, &mut [
                client.condition(),
                condition(FWPM_CONDITION_IP_PROTOCOL, FWP_UINT8, FWP_CONDITION_VALUE0_0 { uint8: protocol }),
                condition(
                    FWPM_CONDITION_IP_REMOTE_ADDRESS,
                    FWP_UINT32,
                    FWP_CONDITION_VALUE0_0 { uint32: u32::from(*peer.ip()) },
                ),
                condition(FWPM_CONDITION_IP_REMOTE_PORT, FWP_UINT16, FWP_CONDITION_VALUE0_0 { uint16: peer.port() }),
            ])?;
            guard.filter(layer, "Allow Windows DHCPv4", true, &mut [
                dhcp.condition(),
                condition(FWPM_CONDITION_IP_PROTOCOL, FWP_UINT8, FWP_CONDITION_VALUE0_0 { uint8: 17 }),
                condition(FWPM_CONDITION_IP_LOCAL_PORT, FWP_UINT16, FWP_CONDITION_VALUE0_0 { uint16: 68 }),
                condition(FWPM_CONDITION_IP_REMOTE_PORT, FWP_UINT16, FWP_CONDITION_VALUE0_0 { uint16: 67 }),
            ])?;
        }
        guard.filter(layer, "Block traffic outside the VPN", false, &mut [])?;
    }
    Ok(())
}

impl Guard {
    fn filter(
        &self,
        layer: GUID,
        name: &str,
        permit: bool,
        conditions: &mut [FWPM_FILTER_CONDITION0],
    ) -> io::Result<()> {
        let (_title, display) = display(name);
        let filter = FWPM_FILTER0 {
            displayData: display,
            layerKey: layer,
            subLayerKey: SUBLAYER,
            weight: FWP_VALUE0 {
                r#type: FWP_UINT8,
                Anonymous: FWP_VALUE0_0 { uint8: if permit { PERMIT_WEIGHT } else { BLOCK_WEIGHT } },
            },
            numFilterConditions: conditions.len() as u32,
            filterCondition: if conditions.is_empty() { null_mut() } else { conditions.as_mut_ptr() },
            action: FWPM_ACTION0 { r#type: if permit { FWP_ACTION_PERMIT } else { FWP_ACTION_BLOCK }, ..Default::default() },
            ..Default::default()
        };
        checked(unsafe { FwpmFilterAdd0(self.0, &filter, null_mut(), null_mut()) }, name)
    }
}

fn condition(field: GUID, kind: FWP_DATA_TYPE, value: FWP_CONDITION_VALUE0_0) -> FWPM_FILTER_CONDITION0 {
    FWPM_FILTER_CONDITION0 {
        fieldKey: field,
        matchType: FWP_MATCH_EQUAL,
        conditionValue: FWP_CONDITION_VALUE0 { r#type: kind, Anonymous: value },
    }
}

fn system_directory() -> io::Result<PathBuf> {
    let mut buffer = vec![0_u16; 32_768];
    let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    if len >= buffer.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Windows system directory is too long"));
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..len])))
}

fn display(title: &str) -> (Vec<u16>, FWPM_DISPLAY_DATA0) {
    let mut title = OsStr::new(title).encode_wide().chain([0]).collect::<Vec<_>>();
    let data = FWPM_DISPLAY_DATA0 { name: title.as_mut_ptr(), description: null_mut() };
    (title, data)
}

fn checked(code: u32, action: &str) -> io::Result<()> {
    if code == 0 {
        return Ok(());
    }
    let cause = io::Error::from_raw_os_error(code as i32);
    let detail = if cause.kind() == io::ErrorKind::PermissionDenied {
        " Run Geoveil from an Administrator terminal."
    } else {
        ""
    };
    Err(io::Error::new(cause.kind(), format!("{action}: {cause} (0x{code:08x}).{detail}")))
}
