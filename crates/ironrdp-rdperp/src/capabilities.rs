//! RAIL capability sets (MS-RDPERP §2.2.1.1).
//!
//! These are advertised by the client inside the Confirm Active PDU. IronRDP
//! models them as opaque buffers (`CapabilitySet::Rail` / `CapabilitySet::WindowList`),
//! where the buffer is the capability *body* (the enclosing type + length are
//! written by the capability-set layer). The helpers here build and parse those
//! bodies.

use bitflags::bitflags;

bitflags! {
    /// `RailSupportLevel` of the Remote Programs Capability Set (§2.2.1.1.1).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RailSupportLevel: u32 {
        const SUPPORTED = 0x0000_0001;
        const DOCKED_LANGBAR = 0x0000_0002;
        const SHELL_INTEGRATION = 0x0000_0004;
        const LANGUAGE_IME_SYNC = 0x0000_0008;
        const SERVER_TO_CLIENT_IME_SYNC = 0x0000_0010;
        const HIDE_MINIMIZED_APPS = 0x0000_0020;
        const WINDOW_CLOAKING = 0x0000_0040;
        const HANDSHAKE_EX = 0x0000_0080;
    }
}

/// Remote Programs Capability Set (§2.2.1.1.1). Body is a single `RailSupportLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RailCapabilitySet {
    pub support_level: RailSupportLevel,
}

impl RailCapabilitySet {
    /// Size of the capability body in bytes.
    pub const BUFFER_SIZE: usize = 4;

    pub fn to_buffer(self) -> Vec<u8> {
        self.support_level.bits().to_le_bytes().to_vec()
    }

    pub fn from_buffer(buf: &[u8]) -> Option<Self> {
        let raw = buf.get(0..4)?;
        Some(Self {
            support_level: RailSupportLevel::from_bits_retain(u32::from_le_bytes(raw.try_into().ok()?)),
        })
    }
}

/// `WndSupportLevel` of the Window List Capability Set (§2.2.1.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WindowSupportLevel {
    NotSupported = 0x0000_0000,
    Supported = 0x0000_0001,
    SupportedEx = 0x0000_0002,
}

impl WindowSupportLevel {
    #[expect(
        clippy::as_conversions,
        reason = "guarantees discriminant layout, and as is the only way to cast enum -> primitive"
    )]
    pub fn as_u32(self) -> u32 {
        self as u32
    }

    pub fn from_u32(value: u32) -> Option<Self> {
        Some(match value {
            0x0000_0000 => Self::NotSupported,
            0x0000_0001 => Self::Supported,
            0x0000_0002 => Self::SupportedEx,
            _ => return None,
        })
    }
}

/// Window List Capability Set (§2.2.1.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowListCapabilitySet {
    pub support_level: WindowSupportLevel,
    pub num_icon_caches: u8,
    pub num_icon_cache_entries: u16,
}

impl WindowListCapabilitySet {
    /// Size of the capability body in bytes: WndSupportLevel(4) + NumIconCaches(1) + NumIconCacheEntries(2).
    pub const BUFFER_SIZE: usize = 7;

    pub fn to_buffer(self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::BUFFER_SIZE);
        buf.extend_from_slice(&self.support_level.as_u32().to_le_bytes());
        buf.push(self.num_icon_caches);
        buf.extend_from_slice(&self.num_icon_cache_entries.to_le_bytes());
        buf
    }

    pub fn from_buffer(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::BUFFER_SIZE {
            return None;
        }
        Some(Self {
            support_level: WindowSupportLevel::from_u32(u32::from_le_bytes(buf[0..4].try_into().ok()?))?,
            num_icon_caches: buf[4],
            num_icon_cache_entries: u16::from_le_bytes(buf[5..7].try_into().ok()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_capability_round_trip() {
        let cap = RailCapabilitySet {
            support_level: RailSupportLevel::SUPPORTED | RailSupportLevel::HANDSHAKE_EX,
        };
        let buf = cap.to_buffer();
        assert_eq!(buf.len(), RailCapabilitySet::BUFFER_SIZE);
        assert_eq!(RailCapabilitySet::from_buffer(&buf), Some(cap));
    }

    #[test]
    fn window_list_capability_round_trip() {
        let cap = WindowListCapabilitySet {
            support_level: WindowSupportLevel::SupportedEx,
            num_icon_caches: 3,
            num_icon_cache_entries: 12,
        };
        let buf = cap.to_buffer();
        assert_eq!(buf.len(), WindowListCapabilitySet::BUFFER_SIZE);
        assert_eq!(WindowListCapabilitySet::from_buffer(&buf), Some(cap));
    }
}
