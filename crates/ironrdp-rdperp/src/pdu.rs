//! RAIL control-channel PDUs (MS-RDPERP §2.2.2).
//!
//! Every PDU starts with a [`RailPduHeader`] (orderType + orderLength). The
//! variable strings in Client Execute are UTF-16LE and *not* null-terminated;
//! their byte lengths are carried in the fixed part.

use bitflags::bitflags;
use ironrdp_core::{
    Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor, cast_int, ensure_size, invalid_field_err,
};

const HEADER_SIZE: usize = 4;

/// `orderType` values for the RAIL PDU header (MS-RDPERP §2.2.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RailOrderType {
    Exec = 0x0001,
    Activate = 0x0002,
    SysParam = 0x0003,
    Handshake = 0x0005,
    NotifyEvent = 0x0006,
    WindowMove = 0x0008,
    LocalMoveSize = 0x0009,
    MinMaxInfo = 0x000A,
    ClientStatus = 0x000B,
    SysMenu = 0x000C,
    LangBarInfo = 0x000D,
    GetAppIdReq = 0x000E,
    GetAppIdResp = 0x000F,
    TaskbarInfo = 0x0010,
    LanguageImeInfo = 0x0011,
    CompartmentInfo = 0x0012,
    HandshakeEx = 0x0013,
    ZOrderSync = 0x0014,
    Cloak = 0x0015,
    PowerDisplayRequest = 0x0016,
    SnapArrange = 0x0017,
    GetAppIdRespEx = 0x0018,
    ExecResult = 0x0080,
}

impl RailOrderType {
    #[expect(
        clippy::as_conversions,
        reason = "guarantees discriminant layout, and as is the only way to cast enum -> primitive"
    )]
    pub fn as_u16(self) -> u16 {
        self as u16
    }

    pub fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            0x0001 => Self::Exec,
            0x0002 => Self::Activate,
            0x0003 => Self::SysParam,
            0x0005 => Self::Handshake,
            0x0006 => Self::NotifyEvent,
            0x0008 => Self::WindowMove,
            0x0009 => Self::LocalMoveSize,
            0x000A => Self::MinMaxInfo,
            0x000B => Self::ClientStatus,
            0x000C => Self::SysMenu,
            0x000D => Self::LangBarInfo,
            0x000E => Self::GetAppIdReq,
            0x000F => Self::GetAppIdResp,
            0x0010 => Self::TaskbarInfo,
            0x0011 => Self::LanguageImeInfo,
            0x0012 => Self::CompartmentInfo,
            0x0013 => Self::HandshakeEx,
            0x0014 => Self::ZOrderSync,
            0x0015 => Self::Cloak,
            0x0016 => Self::PowerDisplayRequest,
            0x0017 => Self::SnapArrange,
            0x0018 => Self::GetAppIdRespEx,
            0x0080 => Self::ExecResult,
            _ => return None,
        })
    }
}

/// `TS_RAIL_PDU_HEADER` (MS-RDPERP §2.2.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RailPduHeader {
    pub order_type: u16,
    /// Length of the whole PDU including this 4-byte header.
    pub order_length: u16,
}

impl RailPduHeader {
    pub const SIZE: usize = HEADER_SIZE;

    fn encode(order_type: RailOrderType, pdu_size: usize, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_size!(in: dst, size: HEADER_SIZE);
        dst.write_u16(order_type.as_u16());
        dst.write_u16(cast_int!("orderLength", pdu_size)?);
        Ok(())
    }

    pub fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        ensure_size!(in: src, size: HEADER_SIZE);
        Ok(Self {
            order_type: src.read_u16(),
            order_length: src.read_u16(),
        })
    }
}

/// Peek the `orderType` of the next PDU without consuming the cursor, so a
/// dispatcher can route to the right decoder.
pub fn peek_order_type(src: &[u8]) -> Option<RailOrderType> {
    if src.len() < 2 {
        return None;
    }
    RailOrderType::from_u16(u16::from_le_bytes([src[0], src[1]]))
}

fn utf16_encoded_len(s: &str) -> usize {
    s.encode_utf16().count() * 2
}

fn write_utf16(dst: &mut WriteCursor<'_>, s: &str) {
    for unit in s.encode_utf16() {
        dst.write_u16(unit);
    }
}

fn read_utf16(src: &mut ReadCursor<'_>, byte_len: usize) -> DecodeResult<String> {
    ensure_size!(in: src, size: byte_len);
    let bytes = src.read_slice(byte_len);
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&units))
}

fn check_order_type(header: &RailPduHeader, expected: RailOrderType) -> DecodeResult<()> {
    if header.order_type != expected.as_u16() {
        return Err(invalid_field_err!("orderType", "unexpected RAIL orderType"));
    }
    Ok(())
}

// ── Handshake (§2.2.2.2.1) — sent by both peers ────────────────────────────

/// `TS_RAIL_ORDER_HANDSHAKE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    pub build_number: u32,
}

impl Handshake {
    const BODY_SIZE: usize = 4;
    const NAME: &'static str = "TS_RAIL_ORDER_HANDSHAKE";
}

impl Encode for Handshake {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        RailPduHeader::encode(RailOrderType::Handshake, self.size(), dst)?;
        ensure_size!(in: dst, size: Self::BODY_SIZE);
        dst.write_u32(self.build_number);
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        HEADER_SIZE + Self::BODY_SIZE
    }
}

impl Decode<'_> for Handshake {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        let header = RailPduHeader::decode(src)?;
        check_order_type(&header, RailOrderType::Handshake)?;
        ensure_size!(in: src, size: Self::BODY_SIZE);
        Ok(Self {
            build_number: src.read_u32(),
        })
    }
}

bitflags! {
    /// `railHandshakeFlags` in Handshake Ex (§2.2.2.2.2).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct HandshakeExFlags: u32 {
        const HIDEF = 0x0000_0001;
        const EXTENDED_SPI_SUPPORTED = 0x0000_0002;
        const SNAP_ARRANGE_SUPPORTED = 0x0000_0004;
        const TEXT_SCALE_SUPPORTED = 0x0000_0008;
        const CARET_BLINK_SUPPORTED = 0x0000_0010;
        const EXTENDED_SPI_2_SUPPORTED = 0x0000_0020;
    }
}

/// `TS_RAIL_ORDER_HANDSHAKE_EX` (§2.2.2.2.2) — server → client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeEx {
    pub build_number: u32,
    pub rail_handshake_flags: HandshakeExFlags,
}

impl HandshakeEx {
    const BODY_SIZE: usize = 8;
    const NAME: &'static str = "TS_RAIL_ORDER_HANDSHAKE_EX";
}

impl Encode for HandshakeEx {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        RailPduHeader::encode(RailOrderType::HandshakeEx, self.size(), dst)?;
        ensure_size!(in: dst, size: Self::BODY_SIZE);
        dst.write_u32(self.build_number);
        dst.write_u32(self.rail_handshake_flags.bits());
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        HEADER_SIZE + Self::BODY_SIZE
    }
}

impl Decode<'_> for HandshakeEx {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        let header = RailPduHeader::decode(src)?;
        check_order_type(&header, RailOrderType::HandshakeEx)?;
        ensure_size!(in: src, size: Self::BODY_SIZE);
        let build_number = src.read_u32();
        let rail_handshake_flags = HandshakeExFlags::from_bits_retain(src.read_u32());
        Ok(Self {
            build_number,
            rail_handshake_flags,
        })
    }
}

bitflags! {
    /// `Flags` of the Client Information PDU (§2.2.2.2.3).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ClientStatusFlags: u32 {
        const ALLOWLOCALMOVESIZE = 0x0000_0001;
        const AUTORECONNECT = 0x0000_0002;
        const ZORDER_SYNC = 0x0000_0004;
        const WINDOW_RESIZE_MARGIN_SUPPORTED = 0x0000_0010;
        const HIGH_DPI_ICONS_SUPPORTED = 0x0000_0020;
        const APPBAR_STATE_SUPPORTED = 0x0000_0040;
        const BIDIRECTIONAL_CLOAK_SUPPORTED = 0x0000_0080;
        const SUPPRESS_ICON_ORDERS = 0x0000_0100;
    }
}

/// `TS_RAIL_ORDER_CLIENTSTATUS` (§2.2.2.2.3) — client → server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientStatus {
    pub flags: ClientStatusFlags,
}

impl ClientStatus {
    const BODY_SIZE: usize = 4;
    const NAME: &'static str = "TS_RAIL_ORDER_CLIENTSTATUS";
}

impl Encode for ClientStatus {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        RailPduHeader::encode(RailOrderType::ClientStatus, self.size(), dst)?;
        ensure_size!(in: dst, size: Self::BODY_SIZE);
        dst.write_u32(self.flags.bits());
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        HEADER_SIZE + Self::BODY_SIZE
    }
}

impl Decode<'_> for ClientStatus {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        let header = RailPduHeader::decode(src)?;
        check_order_type(&header, RailOrderType::ClientStatus)?;
        ensure_size!(in: src, size: Self::BODY_SIZE);
        Ok(Self {
            flags: ClientStatusFlags::from_bits_retain(src.read_u32()),
        })
    }
}

bitflags! {
    /// `Flags` of the Client Execute PDU (§2.2.2.3.1).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ClientExecuteFlags: u16 {
        /// Expand environment variables in `working_dir`.
        const EXPAND_WORKING_DIRECTORY = 0x0001;
        const TRANSLATE_FILES = 0x0002;
        /// `exe_or_file` refers to a document/file rather than an executable.
        const FILE = 0x0004;
        /// Expand environment variables in `arguments`.
        const EXPAND_ARGUMENTS = 0x0008;
        /// `exe_or_file` is an Application User Model ID, not a path — do NOT set
        /// this when launching by path, or the server fails with RAIL_EXEC_E_FAIL.
        const APP_USER_MODEL_ID = 0x0010;
    }
}

/// `TS_RAIL_ORDER_EXEC` (§2.2.2.3.1) — client → server. Launches a RemoteApp.
///
/// `arguments` carries the command line natively (this supersedes packing the
/// argument into the Client Info AlternateShell for the browser client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientExecute {
    pub flags: ClientExecuteFlags,
    pub exe_or_file: String,
    pub working_dir: String,
    pub arguments: String,
}

impl ClientExecute {
    const FIXED_BODY_SIZE: usize = 2 + 2 + 2 + 2; // flags + 3 length fields
    const NAME: &'static str = "TS_RAIL_ORDER_EXEC";

    fn variable_size(&self) -> usize {
        utf16_encoded_len(&self.exe_or_file) + utf16_encoded_len(&self.working_dir) + utf16_encoded_len(&self.arguments)
    }
}

impl Encode for ClientExecute {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        RailPduHeader::encode(RailOrderType::Exec, self.size(), dst)?;
        ensure_size!(in: dst, size: Self::FIXED_BODY_SIZE + self.variable_size());
        dst.write_u16(self.flags.bits());
        dst.write_u16(cast_int!("ExeOrFileLength", utf16_encoded_len(&self.exe_or_file))?);
        dst.write_u16(cast_int!("WorkingDirLength", utf16_encoded_len(&self.working_dir))?);
        dst.write_u16(cast_int!("ArgumentsLen", utf16_encoded_len(&self.arguments))?);
        write_utf16(dst, &self.exe_or_file);
        write_utf16(dst, &self.working_dir);
        write_utf16(dst, &self.arguments);
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        HEADER_SIZE + Self::FIXED_BODY_SIZE + self.variable_size()
    }
}

impl Decode<'_> for ClientExecute {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        let header = RailPduHeader::decode(src)?;
        check_order_type(&header, RailOrderType::Exec)?;
        ensure_size!(in: src, size: Self::FIXED_BODY_SIZE);
        let flags = ClientExecuteFlags::from_bits_retain(src.read_u16());
        let exe_len = usize::from(src.read_u16());
        let wd_len = usize::from(src.read_u16());
        let args_len = usize::from(src.read_u16());
        let exe_or_file = read_utf16(src, exe_len)?;
        let working_dir = read_utf16(src, wd_len)?;
        let arguments = read_utf16(src, args_len)?;
        Ok(Self {
            flags,
            exe_or_file,
            working_dir,
            arguments,
        })
    }
}

/// `TS_RAIL_ORDER_ACTIVATE` (§2.2.2.6.1) — client → server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activate {
    pub window_id: u32,
    pub enabled: bool,
}

impl Activate {
    const BODY_SIZE: usize = 5;
    const NAME: &'static str = "TS_RAIL_ORDER_ACTIVATE";
}

impl Encode for Activate {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        RailPduHeader::encode(RailOrderType::Activate, self.size(), dst)?;
        ensure_size!(in: dst, size: Self::BODY_SIZE);
        dst.write_u32(self.window_id);
        dst.write_u8(u8::from(self.enabled));
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        HEADER_SIZE + Self::BODY_SIZE
    }
}

impl Decode<'_> for Activate {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        let header = RailPduHeader::decode(src)?;
        check_order_type(&header, RailOrderType::Activate)?;
        ensure_size!(in: src, size: Self::BODY_SIZE);
        Ok(Self {
            window_id: src.read_u32(),
            enabled: src.read_u8() != 0,
        })
    }
}

bitflags! {
    /// `Flags` of the Server Execute Result PDU (§2.2.2.3.2). Echoes the flags
    /// from the client's Execute request.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ExecResultFlags: u16 {
        const EXPAND_WORKING_DIRECTORY = 0x0001;
        const TRANSLATE_FILES = 0x0002;
        const FILE = 0x0004;
        const EXPAND_ARGUMENTS = 0x0008;
        const APP_USER_MODEL_ID = 0x0010;
    }
}

/// `TS_RAIL_ORDER_EXEC_RESULT` (§2.2.2.3.2) — server → client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerExecuteResult {
    pub flags: ExecResultFlags,
    /// `RAIL_EXEC_S_OK` (0) on success; other values map to `RAIL_EXEC_E_*`.
    pub exec_result: u16,
    pub raw_result: u32,
    pub exe_or_file: String,
}

impl ServerExecuteResult {
    const FIXED_BODY_SIZE: usize = 2 + 2 + 4 + 2 + 2; // flags + result + raw + padding + exeLen
    const NAME: &'static str = "TS_RAIL_ORDER_EXEC_RESULT";
}

impl Encode for ServerExecuteResult {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        RailPduHeader::encode(RailOrderType::ExecResult, self.size(), dst)?;
        ensure_size!(in: dst, size: Self::FIXED_BODY_SIZE + utf16_encoded_len(&self.exe_or_file));
        dst.write_u16(self.flags.bits());
        dst.write_u16(self.exec_result);
        dst.write_u32(self.raw_result);
        dst.write_u16(0); // padding
        dst.write_u16(cast_int!("ExeOrFileLength", utf16_encoded_len(&self.exe_or_file))?);
        write_utf16(dst, &self.exe_or_file);
        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        HEADER_SIZE + Self::FIXED_BODY_SIZE + utf16_encoded_len(&self.exe_or_file)
    }
}

impl Decode<'_> for ServerExecuteResult {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        let header = RailPduHeader::decode(src)?;
        check_order_type(&header, RailOrderType::ExecResult)?;
        ensure_size!(in: src, size: Self::FIXED_BODY_SIZE);
        let flags = ExecResultFlags::from_bits_retain(src.read_u16());
        let exec_result = src.read_u16();
        let raw_result = src.read_u32();
        let _padding = src.read_u16();
        let exe_len = usize::from(src.read_u16());
        let exe_or_file = read_utf16(src, exe_len)?;
        Ok(Self {
            flags,
            exec_result,
            raw_result,
            exe_or_file,
        })
    }
}

/// Any RAIL control PDU, for dispatching decode and building outbound messages.
///
/// Unmodeled order types (e.g. server system-parameter updates we don't act on
/// yet) round-trip through the `Other` variant instead of failing to decode, so
/// the channel processor never chokes on a PDU it doesn't handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailPdu {
    Handshake(Handshake),
    HandshakeEx(HandshakeEx),
    ClientStatus(ClientStatus),
    ClientExecute(ClientExecute),
    Activate(Activate),
    ServerExecuteResult(ServerExecuteResult),
    Other { order_type: u16, data: Vec<u8> },
}

impl Encode for RailPdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        match self {
            Self::Handshake(p) => p.encode(dst),
            Self::HandshakeEx(p) => p.encode(dst),
            Self::ClientStatus(p) => p.encode(dst),
            Self::ClientExecute(p) => p.encode(dst),
            Self::Activate(p) => p.encode(dst),
            Self::ServerExecuteResult(p) => p.encode(dst),
            Self::Other { order_type, data } => {
                ensure_size!(in: dst, size: HEADER_SIZE + data.len());
                dst.write_u16(*order_type);
                dst.write_u16(cast_int!("orderLength", HEADER_SIZE + data.len())?);
                dst.write_slice(data);
                Ok(())
            }
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Handshake(p) => p.name(),
            Self::HandshakeEx(p) => p.name(),
            Self::ClientStatus(p) => p.name(),
            Self::ClientExecute(p) => p.name(),
            Self::Activate(p) => p.name(),
            Self::ServerExecuteResult(p) => p.name(),
            Self::Other { .. } => "TS_RAIL_ORDER_UNKNOWN",
        }
    }

    fn size(&self) -> usize {
        match self {
            Self::Handshake(p) => p.size(),
            Self::HandshakeEx(p) => p.size(),
            Self::ClientStatus(p) => p.size(),
            Self::ClientExecute(p) => p.size(),
            Self::Activate(p) => p.size(),
            Self::ServerExecuteResult(p) => p.size(),
            Self::Other { data, .. } => HEADER_SIZE + data.len(),
        }
    }
}

impl Decode<'_> for RailPdu {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        match peek_order_type(src.remaining()) {
            Some(RailOrderType::Handshake) => Ok(Self::Handshake(Handshake::decode(src)?)),
            Some(RailOrderType::HandshakeEx) => Ok(Self::HandshakeEx(HandshakeEx::decode(src)?)),
            Some(RailOrderType::ClientStatus) => Ok(Self::ClientStatus(ClientStatus::decode(src)?)),
            Some(RailOrderType::Exec) => Ok(Self::ClientExecute(ClientExecute::decode(src)?)),
            Some(RailOrderType::Activate) => Ok(Self::Activate(Activate::decode(src)?)),
            Some(RailOrderType::ExecResult) => Ok(Self::ServerExecuteResult(ServerExecuteResult::decode(src)?)),
            _ => {
                let header = RailPduHeader::decode(src)?;
                let body_len = usize::from(header.order_length).saturating_sub(HEADER_SIZE);
                ensure_size!(in: src, size: body_len);
                Ok(Self::Other {
                    order_type: header.order_type,
                    data: src.read_slice(body_len).to_vec(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, encode_vec};

    use super::*;

    fn round_trip<T>(pdu: &T) -> T
    where
        T: Encode + for<'de> Decode<'de> + PartialEq + core::fmt::Debug,
    {
        let bytes = encode_vec(pdu).unwrap();
        assert_eq!(bytes.len(), pdu.size(), "encoded length must match size()");
        // orderLength (bytes 2..4) must equal the whole PDU length.
        let order_length = u16::from_le_bytes([bytes[2], bytes[3]]);
        assert_eq!(
            usize::from(order_length),
            pdu.size(),
            "orderLength must be total PDU size"
        );
        decode::<T>(&bytes).unwrap()
    }

    #[test]
    fn handshake_round_trip() {
        let pdu = Handshake {
            build_number: 0x1234_5678,
        };
        assert_eq!(round_trip(&pdu), pdu);
    }

    #[test]
    fn handshake_ex_round_trip() {
        let pdu = HandshakeEx {
            build_number: 7601,
            rail_handshake_flags: HandshakeExFlags::HIDEF | HandshakeExFlags::EXTENDED_SPI_SUPPORTED,
        };
        assert_eq!(round_trip(&pdu), pdu);
    }

    #[test]
    fn client_status_round_trip() {
        let pdu = ClientStatus {
            flags: ClientStatusFlags::ALLOWLOCALMOVESIZE | ClientStatusFlags::ZORDER_SYNC,
        };
        assert_eq!(round_trip(&pdu), pdu);
    }

    #[test]
    fn client_execute_round_trip_with_arguments() {
        let pdu = ClientExecute {
            flags: ClientExecuteFlags::EXPAND_WORKING_DIRECTORY | ClientExecuteFlags::EXPAND_ARGUMENTS,
            exe_or_file: r"C:\Windows\explorer.exe".to_owned(),
            working_dir: String::new(),
            arguments: r"C:\Sales".to_owned(),
        };
        assert_eq!(round_trip(&pdu), pdu);
    }

    #[test]
    fn activate_round_trip() {
        let pdu = Activate {
            window_id: 0x000A_00B2,
            enabled: true,
        };
        assert_eq!(round_trip(&pdu), pdu);
    }

    #[test]
    fn server_execute_result_round_trip() {
        let pdu = ServerExecuteResult {
            flags: ExecResultFlags::empty(),
            exec_result: 0,
            raw_result: 0,
            exe_or_file: "||File Explorer".to_owned(),
        };
        assert_eq!(round_trip(&pdu), pdu);
    }

    #[test]
    fn peek_routes_by_order_type() {
        let bytes = encode_vec(&Activate {
            window_id: 1,
            enabled: false,
        })
        .unwrap();
        assert_eq!(peek_order_type(&bytes), Some(RailOrderType::Activate));
    }

    #[test]
    fn rail_pdu_dispatch_known() {
        let inner = Handshake { build_number: 42 };
        let bytes = encode_vec(&inner).unwrap();
        assert_eq!(decode::<RailPdu>(&bytes).unwrap(), RailPdu::Handshake(inner));
    }

    #[test]
    fn rail_pdu_dispatch_unknown_preserved() {
        // A SysParam order (0x0003) we don't model yet must survive as `Other`.
        let body = [0xAA, 0xBB, 0xCC, 0xDD];
        let mut raw = Vec::new();
        raw.extend_from_slice(&RailOrderType::SysParam.as_u16().to_le_bytes());
        raw.extend_from_slice(&u16::try_from(HEADER_SIZE + body.len()).unwrap().to_le_bytes());
        raw.extend_from_slice(&body);

        let decoded = decode::<RailPdu>(&raw).unwrap();
        assert_eq!(
            decoded,
            RailPdu::Other {
                order_type: RailOrderType::SysParam.as_u16(),
                data: body.to_vec(),
            }
        );
        // And it re-encodes byte-identically.
        assert_eq!(encode_vec(&decoded).unwrap(), raw);
    }
}
