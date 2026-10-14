//! Minimal STUN Binding codec (RFC 8489), enough to learn the public
//! ("server-reflexive") address a STUN server sees.
//!
//! Only what a leak probe needs is implemented: building a Binding request
//! and decoding a Binding success response's XOR-MAPPED-ADDRESS (falling
//! back to the legacy MAPPED-ADDRESS). The decoder checks the magic cookie,
//! the transaction ID, the length fields and, when present, the FINGERPRINT
//! CRC. MESSAGE-INTEGRITY is skipped: Binding requests to public servers are
//! unauthenticated. No I/O happens here; the platform crate sends the bytes.
//!
//! # Wire format (RFC 8489 section 5)
//!
//! A 20-byte header: message type (Binding request `0x0001`, success
//! `0x0101`, error `0x0111`), message length (attribute bytes only, always a
//! multiple of 4), magic cookie `0x2112A442` and a 96-bit transaction ID.
//! Attributes follow as type / length / value, each value padded to 4 bytes.
//!
//! The tests use the RFC 5769 sample messages.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The fixed magic cookie in every RFC 5389/8489 message.
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
/// Binding request message type.
pub const BINDING_REQUEST: u16 = 0x0001;
/// Binding success response message type.
pub const BINDING_SUCCESS: u16 = 0x0101;
/// Binding error response message type.
pub const BINDING_ERROR: u16 = 0x0111;

/// MAPPED-ADDRESS attribute (legacy, not XORed).
pub const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
/// ERROR-CODE attribute.
pub const ATTR_ERROR_CODE: u16 = 0x0009;
/// XOR-MAPPED-ADDRESS attribute.
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// SOFTWARE attribute.
pub const ATTR_SOFTWARE: u16 = 0x8022;
/// FINGERPRINT attribute.
pub const ATTR_FINGERPRINT: u16 = 0x8028;

/// Header length in bytes.
pub const HEADER_LEN: usize = 20;
/// Value XORed into the CRC-32 for FINGERPRINT ("STUN").
const FINGERPRINT_XOR: u32 = 0x5354_554E;

/// A 96-bit STUN transaction ID.
pub type TransactionId = [u8; 12];

/// Why a datagram is not a usable Binding success response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StunError {
    /// Shorter than a header, or a length field points past the end.
    Truncated,
    /// The first two bits are not zero, or the cookie is wrong: not STUN.
    NotStun,
    /// A STUN message for a different transaction (stale or spoofed).
    WrongTransaction,
    /// The FINGERPRINT attribute does not match the message.
    BadFingerprint,
    /// The server answered with a Binding error response.
    ErrorResponse { code: u16, reason: String },
    /// Some other message type than a Binding response.
    UnexpectedType(u16),
    /// A success response with no (valid) mapped address attribute.
    NoMappedAddress,
}

impl fmt::Display for StunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StunError::Truncated => write!(f, "truncated STUN message"),
            StunError::NotStun => write!(f, "not a STUN message"),
            StunError::WrongTransaction => write!(f, "transaction ID does not match"),
            StunError::BadFingerprint => write!(f, "FINGERPRINT does not match"),
            StunError::ErrorResponse { code, reason } => write!(f, "STUN error {code} {reason}"),
            StunError::UnexpectedType(t) => write!(f, "unexpected STUN message type 0x{t:04x}"),
            StunError::NoMappedAddress => write!(f, "response has no mapped address"),
        }
    }
}

impl std::error::Error for StunError {}

/// One raw attribute of a decoded message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAttribute<'a> {
    pub kind: u16,
    /// The value without padding.
    pub value: &'a [u8],
    /// Byte offset of the attribute header inside the message.
    pub offset: usize,
}

/// A structurally valid STUN message (header checked, attributes split).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message<'a> {
    pub msg_type: u16,
    pub transaction_id: TransactionId,
    pub attributes: Vec<RawAttribute<'a>>,
    /// The whole message (header plus attributes), without trailing bytes.
    pub bytes: &'a [u8],
}

/// A fresh random transaction ID.
pub fn random_transaction_id() -> TransactionId {
    super::net::random_bytes()
}

/// Builds a Binding request with the given transaction ID and an optional
/// SOFTWARE attribute (truncated to 763 bytes, as RFC 8489 allows).
pub fn encode_binding_request(transaction_id: &TransactionId, software: Option<&str>) -> Vec<u8> {
    let mut attrs = Vec::new();
    if let Some(sw) = software {
        let mut value = sw.as_bytes();
        if value.len() > 763 {
            value = &value[..763];
        }
        push_attribute(&mut attrs, ATTR_SOFTWARE, value);
    }
    let mut out = Vec::with_capacity(HEADER_LEN + attrs.len());
    out.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    out.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out.extend_from_slice(transaction_id);
    out.extend_from_slice(&attrs);
    out
}

/// Appends one attribute, zero-padded to a multiple of 4 bytes.
fn push_attribute(buf: &mut Vec<u8>, kind: u16, value: &[u8]) {
    buf.extend_from_slice(&kind.to_be_bytes());
    buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
    buf.extend_from_slice(value);
    buf.resize(buf.len() + padding(value.len()), 0);
}

/// Padding needed after a value of `len` bytes.
fn padding(len: usize) -> usize {
    (4 - len % 4) % 4
}

/// Splits a datagram into a STUN message, checking the header and lengths.
pub fn parse_message(buf: &[u8]) -> Result<Message<'_>, StunError> {
    if buf.len() < HEADER_LEN {
        return Err(StunError::Truncated);
    }
    if buf[0] & 0xC0 != 0 || be_u32(&buf[4..8]) != MAGIC_COOKIE {
        return Err(StunError::NotStun);
    }
    let msg_type = be_u16(&buf[0..2]);
    let len = be_u16(&buf[2..4]) as usize;
    if !len.is_multiple_of(4) {
        return Err(StunError::NotStun);
    }
    if buf.len() < HEADER_LEN + len {
        return Err(StunError::Truncated);
    }
    let bytes = &buf[..HEADER_LEN + len];
    let mut transaction_id = [0u8; 12];
    transaction_id.copy_from_slice(&bytes[8..20]);

    let mut attributes = Vec::new();
    let mut pos = HEADER_LEN;
    while pos < bytes.len() {
        if pos + 4 > bytes.len() {
            return Err(StunError::Truncated);
        }
        let kind = be_u16(&bytes[pos..pos + 2]);
        let alen = be_u16(&bytes[pos + 2..pos + 4]) as usize;
        let start = pos + 4;
        let end = start + alen;
        if end > bytes.len() {
            return Err(StunError::Truncated);
        }
        attributes.push(RawAttribute { kind, value: &bytes[start..end], offset: pos });
        pos = end + padding(alen);
    }
    Ok(Message { msg_type, transaction_id, attributes, bytes })
}

/// Checks the FINGERPRINT attribute when present. Returns `Ok(false)` when
/// the message has no FINGERPRINT, `Ok(true)` when it matches.
pub fn check_fingerprint(msg: &Message<'_>) -> Result<bool, StunError> {
    let Some(fp) = msg.attributes.iter().find(|a| a.kind == ATTR_FINGERPRINT) else {
        return Ok(false);
    };
    if fp.value.len() != 4 {
        return Err(StunError::BadFingerprint);
    }
    // The CRC covers everything before the FINGERPRINT attribute, with the
    // header length already counting the FINGERPRINT attribute itself.
    let expected = crc32(&msg.bytes[..fp.offset]) ^ FINGERPRINT_XOR;
    if be_u32(fp.value) == expected {
        Ok(true)
    } else {
        Err(StunError::BadFingerprint)
    }
}

/// Decodes a Binding success response to `transaction_id` and returns the
/// server-reflexive address (XOR-MAPPED-ADDRESS, else MAPPED-ADDRESS).
pub fn decode_binding_response(buf: &[u8], transaction_id: &TransactionId) -> Result<SocketAddr, StunError> {
    let msg = parse_message(buf)?;
    if &msg.transaction_id != transaction_id {
        return Err(StunError::WrongTransaction);
    }
    check_fingerprint(&msg)?;
    match msg.msg_type {
        BINDING_SUCCESS => {}
        BINDING_ERROR => {
            let (code, reason) = msg
                .attributes
                .iter()
                .find(|a| a.kind == ATTR_ERROR_CODE)
                .and_then(|a| parse_error_code(a.value))
                .unwrap_or((0, String::new()));
            return Err(StunError::ErrorResponse { code, reason });
        }
        other => return Err(StunError::UnexpectedType(other)),
    }
    let xor = msg
        .attributes
        .iter()
        .filter(|a| a.kind == ATTR_XOR_MAPPED_ADDRESS)
        .find_map(|a| decode_address(a.value, Some(&msg.transaction_id)));
    let plain = || {
        msg.attributes
            .iter()
            .filter(|a| a.kind == ATTR_MAPPED_ADDRESS)
            .find_map(|a| decode_address(a.value, None))
    };
    xor.or_else(plain).ok_or(StunError::NoMappedAddress)
}

/// Decodes a (XOR-)MAPPED-ADDRESS value. `xor_tid` is `Some` for the XOR form.
fn decode_address(value: &[u8], xor_tid: Option<&TransactionId>) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let family = value[1];
    let mut port = be_u16(&value[2..4]);
    if xor_tid.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip = match (family, value.len()) {
        (0x01, 8) => {
            let mut raw = be_u32(&value[4..8]);
            if xor_tid.is_some() {
                raw ^= MAGIC_COOKIE;
            }
            IpAddr::V4(Ipv4Addr::from(raw))
        }
        (0x02, 20) => {
            let mut raw = [0u8; 16];
            raw.copy_from_slice(&value[4..20]);
            if let Some(tid) = xor_tid {
                let mut key = [0u8; 16];
                key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                key[4..].copy_from_slice(tid);
                for (b, k) in raw.iter_mut().zip(key) {
                    *b ^= k;
                }
            }
            IpAddr::V6(Ipv6Addr::from(raw))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Decodes an ERROR-CODE value into (code, reason phrase).
fn parse_error_code(value: &[u8]) -> Option<(u16, String)> {
    if value.len() < 4 {
        return None;
    }
    let code = u16::from(value[2] & 0x07) * 100 + u16::from(value[3]);
    Some((code, String::from_utf8_lossy(&value[4..]).into_owned()))
}

fn be_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// CRC-32 (ISO-HDLC / IEEE 802.3, the zlib one) as FINGERPRINT requires.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 5769 section 2.1, sample request.
    const RFC5769_REQUEST: [u8; 108] = [
        0x00, 0x01, 0x00, 0x58, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87,
        0xdf, 0xae, 0x80, 0x22, 0x00, 0x10, 0x53, 0x54, 0x55, 0x4e, 0x20, 0x74, 0x65, 0x73, 0x74, 0x20, 0x63, 0x6c,
        0x69, 0x65, 0x6e, 0x74, 0x00, 0x24, 0x00, 0x04, 0x6e, 0x00, 0x01, 0xff, 0x80, 0x29, 0x00, 0x08, 0x93, 0x2f,
        0xf9, 0xb1, 0x51, 0x26, 0x3b, 0x36, 0x00, 0x06, 0x00, 0x09, 0x65, 0x76, 0x74, 0x6a, 0x3a, 0x68, 0x36, 0x76,
        0x59, 0x20, 0x20, 0x20, 0x00, 0x08, 0x00, 0x14, 0x9a, 0xea, 0xa7, 0x0c, 0xbf, 0xd8, 0xcb, 0x56, 0x78, 0x1e,
        0xf2, 0xb5, 0xb2, 0xd3, 0xf2, 0x49, 0xc1, 0xb5, 0x71, 0xa2, 0x80, 0x28, 0x00, 0x04, 0xe5, 0x7a, 0x3b, 0xcf,
    ];

    /// RFC 5769 section 2.2, sample IPv4 response (192.0.2.1:32853).
    const RFC5769_IPV4_RESPONSE: [u8; 80] = [
        0x01, 0x01, 0x00, 0x3c, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87,
        0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20,
        0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43, 0x00, 0x08, 0x00, 0x14, 0x2b, 0x91,
        0xf5, 0x99, 0xfd, 0x9e, 0x90, 0xc3, 0x8c, 0x74, 0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53, 0xf0, 0x6b, 0xe7, 0xd7,
        0x80, 0x28, 0x00, 0x04, 0xc0, 0x7d, 0x4c, 0x96,
    ];

    /// RFC 5769 section 2.3, sample IPv6 response
    /// ([2001:db8:1234:5678:11:2233:4455:6677]:32853).
    const RFC5769_IPV6_RESPONSE: [u8; 92] = [
        0x01, 0x01, 0x00, 0x48, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87,
        0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20,
        0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47, 0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3, 0xf1, 0x79, 0xbc, 0x25,
        0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9, 0x00, 0x08, 0x00, 0x14, 0xa3, 0x82, 0x95, 0x4e, 0x4b, 0xe6, 0x7b, 0xf1,
        0x17, 0x84, 0xc9, 0x7c, 0x82, 0x92, 0xc2, 0x75, 0xbf, 0xe3, 0xed, 0x41, 0x80, 0x28, 0x00, 0x04, 0xc8, 0xfb,
        0x0b, 0x4c,
    ];

    const RFC_TID: TransactionId = [0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae];

    #[test]
    fn crc32_check_value() {
        // The standard CRC-32 check value.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn rfc5769_ipv4_response() {
        let addr = decode_binding_response(&RFC5769_IPV4_RESPONSE, &RFC_TID).unwrap();
        assert_eq!(addr, "192.0.2.1:32853".parse().unwrap());
        let msg = parse_message(&RFC5769_IPV4_RESPONSE).unwrap();
        assert_eq!(check_fingerprint(&msg), Ok(true));
        let sw = msg.attributes.iter().find(|a| a.kind == ATTR_SOFTWARE).unwrap();
        assert_eq!(sw.value, b"test vector");
    }

    #[test]
    fn rfc5769_ipv6_response() {
        let addr = decode_binding_response(&RFC5769_IPV6_RESPONSE, &RFC_TID).unwrap();
        assert_eq!(addr, "[2001:db8:1234:5678:11:2233:4455:6677]:32853".parse().unwrap());
    }

    #[test]
    fn rfc5769_request_parses_with_fingerprint() {
        let msg = parse_message(&RFC5769_REQUEST).unwrap();
        assert_eq!(msg.msg_type, BINDING_REQUEST);
        assert_eq!(msg.transaction_id, RFC_TID);
        let kinds: Vec<u16> = msg.attributes.iter().map(|a| a.kind).collect();
        assert_eq!(kinds, vec![ATTR_SOFTWARE, 0x0024, 0x8029, 0x0006, 0x0008, ATTR_FINGERPRINT]);
        assert_eq!(msg.attributes[3].value, b"evtj:h6vY");
        assert_eq!(check_fingerprint(&msg), Ok(true));
        // A request is not a response.
        assert_eq!(
            decode_binding_response(&RFC5769_REQUEST, &RFC_TID),
            Err(StunError::UnexpectedType(BINDING_REQUEST))
        );
    }

    #[test]
    fn encoder_matches_rfc5769_header_and_software() {
        let req = encode_binding_request(&RFC_TID, Some("STUN test client"));
        // Same type, cookie, transaction ID and SOFTWARE TLV as the sample;
        // only the length differs because we send SOFTWARE alone.
        assert_eq!(&req[0..2], &RFC5769_REQUEST[0..2]);
        assert_eq!(&req[2..4], &[0x00, 0x14]);
        assert_eq!(&req[4..40], &RFC5769_REQUEST[4..40]);
        assert_eq!(req.len(), 40);
    }

    #[test]
    fn encoder_pads_and_round_trips() {
        let tid = [7u8; 12];
        let req = encode_binding_request(&tid, Some("geoveil"));
        assert_eq!(req.len(), HEADER_LEN + 4 + 8);
        assert_eq!(req[req.len() - 1], 0, "zero padding");
        let msg = parse_message(&req).unwrap();
        assert_eq!(msg.msg_type, BINDING_REQUEST);
        assert_eq!(msg.transaction_id, tid);
        assert_eq!(msg.attributes[0].value, b"geoveil");
        let bare = encode_binding_request(&tid, None);
        assert_eq!(bare.len(), HEADER_LEN);
        assert_eq!(&bare[2..4], &[0, 0]);
    }

    #[test]
    fn rejects_wrong_transaction() {
        let mut other = RFC_TID;
        other[0] ^= 1;
        assert_eq!(decode_binding_response(&RFC5769_IPV4_RESPONSE, &other), Err(StunError::WrongTransaction));
    }

    #[test]
    fn rejects_corrupted_message_via_fingerprint() {
        let mut bad = RFC5769_IPV4_RESPONSE;
        bad[45] ^= 0xff; // inside the XOR-MAPPED-ADDRESS value
        assert_eq!(decode_binding_response(&bad, &RFC_TID), Err(StunError::BadFingerprint));
    }

    #[test]
    fn rejects_non_stun_and_truncated() {
        assert_eq!(parse_message(&[0u8; 10]), Err(StunError::Truncated));
        let mut bad_cookie = RFC5769_IPV4_RESPONSE;
        bad_cookie[4] = 0;
        assert_eq!(parse_message(&bad_cookie), Err(StunError::NotStun));
        let mut high_bits = RFC5769_IPV4_RESPONSE;
        high_bits[0] = 0x81;
        assert_eq!(parse_message(&high_bits), Err(StunError::NotStun));
        assert_eq!(parse_message(&RFC5769_IPV4_RESPONSE[..60]), Err(StunError::Truncated));
    }

    /// A success response with only a plain MAPPED-ADDRESS (old servers).
    #[test]
    fn falls_back_to_mapped_address() {
        let tid = [1u8; 12];
        let mut attrs = Vec::new();
        push_attribute(&mut attrs, ATTR_MAPPED_ADDRESS, &[0, 1, 0x1f, 0x90, 203, 0, 113, 9]);
        let mut msg = Vec::new();
        msg.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        msg.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&tid);
        msg.extend_from_slice(&attrs);
        assert_eq!(decode_binding_response(&msg, &tid).unwrap(), "203.0.113.9:8080".parse().unwrap());
    }

    #[test]
    fn error_response_is_reported() {
        let tid = [2u8; 12];
        let mut attrs = Vec::new();
        push_attribute(&mut attrs, ATTR_ERROR_CODE, &[0, 0, 4, 20, b'B', b'a', b'd']);
        let mut msg = Vec::new();
        msg.extend_from_slice(&BINDING_ERROR.to_be_bytes());
        msg.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&tid);
        msg.extend_from_slice(&attrs);
        assert_eq!(
            decode_binding_response(&msg, &tid),
            Err(StunError::ErrorResponse { code: 420, reason: "Bad".into() })
        );
    }

    #[test]
    fn success_without_address_is_an_error() {
        let tid = [3u8; 12];
        let msg = {
            let mut m = encode_binding_request(&tid, Some("x"));
            m[0..2].copy_from_slice(&BINDING_SUCCESS.to_be_bytes());
            m
        };
        assert_eq!(decode_binding_response(&msg, &tid), Err(StunError::NoMappedAddress));
    }

    /// The decoder never panics on arbitrary input (cheap deterministic fuzz).
    #[test]
    fn decoder_survives_garbage() {
        let mut state = 0x1234_5678u32;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for _ in 0..5000 {
            let len = (next() % 120) as usize;
            let mut buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if buf.len() >= 20 && next() % 2 == 0 {
                // Make many inputs look like STUN so the attribute walk runs.
                buf[0] = 0x01;
                buf[1] = 0x01;
                let alen = ((buf.len() - 20) / 4 * 4) as u16;
                buf[2..4].copy_from_slice(&alen.to_be_bytes());
                buf[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
            }
            let _ = decode_binding_response(&buf, &RFC_TID);
            let tid: TransactionId = buf.get(8..20).and_then(|s| s.try_into().ok()).unwrap_or(RFC_TID);
            let _ = decode_binding_response(&buf, &tid);
        }
    }
}
