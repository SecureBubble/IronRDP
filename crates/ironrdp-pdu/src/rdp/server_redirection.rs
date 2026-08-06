//! RDP Server Redirection Packet (MS-RDPBCGR 2.2.13.1).
//!
//! Carried inside an Enhanced Security Server Redirection PDU (share-control PDU
//! type `PDUTYPE_SERVER_REDIR_PKT`, 0xA), whose body is:
//! `pad2Octets(2) | Flags(2) | Length(2) | SessionID(4) | RedirFlags(4) | fields`.
//!
//! Framing note: [`crate::rdp::headers::ShareControlHeader`] reads a 4-byte
//! `shareId` for every PDU type, which here overlaps `pad2Octets(2)` + `Flags(2)`.
//! So this type is decoded/encoded starting at the packet `Length` field — the
//! `pad2Octets`/`Flags` live in the enclosing header's `shareId`.

use ironrdp_core::{
    Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor, cast_length, ensure_size,
};

// Redirection flags (`RedirectionFlags`, MS-RDPBCGR 2.2.13.1).
const LB_TARGET_NET_ADDRESS: u32 = 0x0000_0001;
const LB_LOAD_BALANCE_INFO: u32 = 0x0000_0002;
const LB_USERNAME: u32 = 0x0000_0004;
const LB_DOMAIN: u32 = 0x0000_0008;
const LB_PASSWORD: u32 = 0x0000_0010;
const LB_TARGET_FQDN: u32 = 0x0000_0100;
const LB_TARGET_NETBIOS_NAME: u32 = 0x0000_0200;
const LB_TARGET_NET_ADDRESSES: u32 = 0x0000_0800;
const LB_CLIENT_TSV_URL: u32 = 0x0000_1000;
const LB_REDIRECTION_GUID: u32 = 0x0000_8000;
const LB_TARGET_CERTIFICATE: u32 = 0x0001_0000;

/// Server Redirection Packet (from `SessionID` onward; see module docs).
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct ServerRedirectionPdu {
    /// The packet `Length` field (bytes from `Flags` onward). Retained so the PDU
    /// round-trips; not otherwise interpreted.
    pub redir_packet_length: u16,
    pub session_id: u32,
    pub redir_flags: u32,
    pub target_net_address: Option<String>,
    /// Routing token / cookie (also used by the SecureBubble proxy to carry a
    /// `QTERR\t<code>\t<title>\t<body>` sentinel on a failed sign-in).
    pub load_balance_info: Option<Vec<u8>>,
    pub username: Option<String>,
    pub domain: Option<String>,
    pub password: Option<Vec<u8>>,
    pub target_fqdn: Option<String>,
    pub target_netbios_name: Option<String>,
}

impl core::fmt::Debug for ServerRedirectionPdu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ServerRedirectionPdu")
            .field("session_id", &self.session_id)
            .field("redir_flags", &format_args!("0x{:08X}", self.redir_flags))
            .field("target_fqdn", &self.target_fqdn)
            .field("username", &self.username)
            .field("domain", &self.domain)
            .field("load_balance_info_len", &self.load_balance_info.as_ref().map(Vec::len))
            .field("password_len", &self.password.as_ref().map(Vec::len))
            .finish()
    }
}

impl ServerRedirectionPdu {
    const NAME: &'static str = "ServerRedirectionPdu";
    const FIXED_PART_SIZE: usize = 2 /* Length */ + 4 /* SessionID */ + 4 /* RedirFlags */;

    /// If [`Self::load_balance_info`] holds the SecureBubble proxy's error
    /// sentinel `QTERR\t<code>\t<title>\t<body>`, returns `(title, body)`.
    ///
    /// FreeRDP wraps `LB_LOAD_BALANCE_INFO` on the wire as
    /// `Cookie: msts=<value>\r\n`, so the prefix/suffix are stripped first.
    pub fn qterr_message(&self) -> Option<(String, String)> {
        let lbi = self.load_balance_info.as_ref()?;
        let text = core::str::from_utf8(lbi).ok()?;
        let text = text.strip_prefix("Cookie: msts=").unwrap_or(text);
        let text = text.trim_end_matches(['\r', '\n']);
        let mut parts = text.split('\t');
        if parts.next()? != "QTERR" {
            return None;
        }
        let _code = parts.next()?;
        let title = parts.next()?.to_owned();
        let body = parts.next().unwrap_or("").to_owned();
        Some((title, body))
    }
}

/// Read a redirection data field: `u32` length followed by that many bytes.
fn read_data(src: &mut ReadCursor<'_>) -> DecodeResult<Vec<u8>> {
    ensure_size!(in: src, size: 4);
    let len = cast_length!("redirDataLen", src.read_u32())?;
    ensure_size!(in: src, size: len);
    Ok(src.read_slice(len).to_vec())
}

/// Read a redirection UTF-16LE string: `u32` byte length then the bytes.
/// Trailing NUL code units are trimmed.
fn read_unicode_string(src: &mut ReadCursor<'_>) -> DecodeResult<String> {
    let bytes = read_data(src)?;
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let trimmed = units.split(|&u| u == 0).next().unwrap_or(&units);
    Ok(String::from_utf16_lossy(trimmed))
}

fn write_data(dst: &mut WriteCursor<'_>, data: &[u8]) -> EncodeResult<()> {
    dst.write_u32(cast_length!("redirDataLen", data.len())?);
    dst.write_slice(data);
    Ok(())
}

fn write_unicode_string(dst: &mut WriteCursor<'_>, s: &str) -> EncodeResult<()> {
    let mut units: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    units.extend_from_slice(&[0, 0]); // NUL terminator
    write_data(dst, &units)
}

fn unicode_size(s: &str) -> usize {
    4 + (s.encode_utf16().count() + 1) * 2
}

impl<'de> Decode<'de> for ServerRedirectionPdu {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        ensure_size!(in: src, size: Self::FIXED_PART_SIZE);
        let redir_packet_length = src.read_u16();
        let session_id = src.read_u32();
        let redir_flags = src.read_u32();

        // Fields appear in the fixed order defined by MS-RDPBCGR 2.2.13.1.
        let target_net_address = (redir_flags & LB_TARGET_NET_ADDRESS != 0)
            .then(|| read_unicode_string(src))
            .transpose()?;
        let load_balance_info = (redir_flags & LB_LOAD_BALANCE_INFO != 0)
            .then(|| read_data(src))
            .transpose()?;
        let username = (redir_flags & LB_USERNAME != 0)
            .then(|| read_unicode_string(src))
            .transpose()?;
        let domain = (redir_flags & LB_DOMAIN != 0)
            .then(|| read_unicode_string(src))
            .transpose()?;
        let password = (redir_flags & LB_PASSWORD != 0).then(|| read_data(src)).transpose()?;
        let target_fqdn = (redir_flags & LB_TARGET_FQDN != 0)
            .then(|| read_unicode_string(src))
            .transpose()?;
        let target_netbios_name = (redir_flags & LB_TARGET_NETBIOS_NAME != 0)
            .then(|| read_unicode_string(src))
            .transpose()?;

        // Remaining optional fields are consumed but not retained (not used by the
        // client). They must still be read in order so the cursor stays aligned.
        if redir_flags & LB_CLIENT_TSV_URL != 0 {
            let _ = read_data(src)?;
        }
        if redir_flags & LB_REDIRECTION_GUID != 0 {
            let _ = read_data(src)?;
        }
        if redir_flags & LB_TARGET_CERTIFICATE != 0 {
            let _ = read_data(src)?;
        }
        if redir_flags & LB_TARGET_NET_ADDRESSES != 0 {
            let _ = read_data(src)?;
        }

        Ok(Self {
            redir_packet_length,
            session_id,
            redir_flags,
            target_net_address,
            load_balance_info,
            username,
            domain,
            password,
            target_fqdn,
            target_netbios_name,
        })
    }
}

impl Encode for ServerRedirectionPdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_size!(in: dst, size: self.size());
        dst.write_u16(self.redir_packet_length);
        dst.write_u32(self.session_id);
        dst.write_u32(self.redir_flags);
        if let Some(s) = &self.target_net_address {
            write_unicode_string(dst, s)?;
        }
        if let Some(d) = &self.load_balance_info {
            write_data(dst, d)?;
        }
        if let Some(s) = &self.username {
            write_unicode_string(dst, s)?;
        }
        if let Some(s) = &self.domain {
            write_unicode_string(dst, s)?;
        }
        if let Some(d) = &self.password {
            write_data(dst, d)?;
        }
        if let Some(s) = &self.target_fqdn {
            write_unicode_string(dst, s)?;
        }
        if let Some(s) = &self.target_netbios_name {
            write_unicode_string(dst, s)?;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
            + self.target_net_address.as_deref().map_or(0, unicode_size)
            + self.load_balance_info.as_ref().map_or(0, |d| 4 + d.len())
            + self.username.as_deref().map_or(0, unicode_size)
            + self.domain.as_deref().map_or(0, unicode_size)
            + self.password.as_ref().map_or(0, |d| 4 + d.len())
            + self.target_fqdn.as_deref().map_or(0, unicode_size)
            + self.target_netbios_name.as_deref().map_or(0, unicode_size)
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, encode_vec};

    use super::*;

    /// The proxy's failed-sign-in redirect exactly as it appears on the wire after
    /// `ShareControlHeader` has consumed pad2Octets+Flags as `shareId`: the packet
    /// `Length`, then SessionID, RedirFlags, and the `LB_*` fields. FreeRDP wraps
    /// `LB_LOAD_BALANCE_INFO` as `Cookie: msts=<value>\r\n`.
    fn proxy_qterr_bytes() -> Vec<u8> {
        let sentinel = b"QTERR\t00020014\tSign-in failed\tUsername or password is incorrect.";
        let mut lbi = Vec::new();
        lbi.extend_from_slice(b"Cookie: msts=");
        lbi.extend_from_slice(sentinel);
        lbi.extend_from_slice(b"\r\n");
        let fqdn: Vec<u8> = "proxy.host"
            .encode_utf16()
            .chain(core::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut b = Vec::new();
        b.extend_from_slice(&0x00cdu16.to_le_bytes()); // redir packet Length
        b.extend_from_slice(&0x1234u32.to_le_bytes()); // session_id
        b.extend_from_slice(&(LB_LOAD_BALANCE_INFO | LB_TARGET_FQDN).to_le_bytes());
        b.extend_from_slice(&(lbi.len() as u32).to_le_bytes());
        b.extend_from_slice(&lbi);
        b.extend_from_slice(&(fqdn.len() as u32).to_le_bytes());
        b.extend_from_slice(&fqdn);
        b
    }

    #[test]
    fn decodes_proxy_qterr_redirect() {
        let bytes = proxy_qterr_bytes();
        let pdu: ServerRedirectionPdu = decode(&bytes).expect("decode");
        assert_eq!(pdu.session_id, 0x1234);
        assert_eq!(pdu.target_fqdn.as_deref(), Some("proxy.host"));
        let (title, body) = pdu.qterr_message().expect("qterr");
        assert_eq!(title, "Sign-in failed");
        assert_eq!(body, "Username or password is incorrect.");
    }

    #[test]
    fn round_trips() {
        let bytes = proxy_qterr_bytes();
        let pdu: ServerRedirectionPdu = decode(&bytes).expect("decode");
        assert_eq!(encode_vec(&pdu).expect("encode"), bytes);
    }

    #[test]
    fn non_qterr_lbi_is_not_a_message() {
        let token = b"Cookie: msts=1.2.3\r\n";
        let mut b = Vec::new();
        b.extend_from_slice(&0u16.to_le_bytes()); // Length
        b.extend_from_slice(&0u32.to_le_bytes()); // session_id
        b.extend_from_slice(&LB_LOAD_BALANCE_INFO.to_le_bytes());
        b.extend_from_slice(&(token.len() as u32).to_le_bytes());
        b.extend_from_slice(token);
        let pdu: ServerRedirectionPdu = decode(&b).expect("decode");
        assert!(pdu.qterr_message().is_none());
        assert_eq!(pdu.load_balance_info.as_deref(), Some(&token[..]));
    }
}
