use ironrdp_core::{
    Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor, ensure_fixed_part_size, invalid_field_err,
};

/// [2.2.1.1.2] Window List Capability Set.
///
/// [2.2.1.1.2]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdperp
#[derive(Debug, PartialEq, Eq, Clone)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct WindowList {
    pub support_level: WindowSupportLevel,
    pub num_icon_caches: u8,
    pub num_icon_cache_entries: u16,
}

impl WindowList {
    const NAME: &'static str = "WindowList";

    pub(crate) const FIXED_PART_SIZE: usize =
        4 /* WndSupportLevel */ + 1 /* NumIconCaches */ + 2 /* NumIconCacheEntries */;
}

impl Encode for WindowList {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);

        dst.write_u32(self.support_level.as_u32());
        dst.write_u8(self.num_icon_caches);
        dst.write_u16(self.num_icon_cache_entries);

        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

impl<'de> Decode<'de> for WindowList {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);

        let support_level = WindowSupportLevel::from_u32(src.read_u32())
            .ok_or_else(|| invalid_field_err!("wndSupportLevel", "invalid window support level"))?;
        let num_icon_caches = src.read_u8();
        let num_icon_cache_entries = src.read_u16();

        Ok(Self {
            support_level,
            num_icon_caches,
            num_icon_cache_entries,
        })
    }
}

/// `WndSupportLevel` ([MS-RDPERP] 2.2.1.1.2).
///
/// Deliberately a NEWTYPE, not an enum: this was an enum whose decode REJECTED any value outside
/// {0, 1, 2}, and a real Windows host sends a value outside that set, which aborted the whole
/// connection at Capabilities Exchange with "invalid `wndSupportLevel`". A capability level we do
/// not recognise must never be fatal -- we only need to know whether windowing is supported at
/// all. This mirrors the same remedy applied to `KeyboardType`, which had the identical problem.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct WindowSupportLevel(pub u32);

impl WindowSupportLevel {
    pub const NOT_SUPPORTED: Self = Self(0x0000_0000);
    pub const SUPPORTED: Self = Self(0x0000_0001);
    pub const SUPPORTED_EX: Self = Self(0x0000_0002);

    fn from_u32(value: u32) -> Option<Self> {
        Some(Self(value))
    }

    fn as_u32(self) -> u32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, encode_vec};

    use super::super::CapabilitySet;
    use super::*;

    const WINDOW_LIST_BUFFER: [u8; 7] = [0x02, 0x00, 0x00, 0x00, 0x03, 0x0c, 0x00];
    const WINDOW_LIST: WindowList = WindowList {
        support_level: WindowSupportLevel::SUPPORTED_EX,
        num_icon_caches: 3,
        num_icon_cache_entries: 12,
    };

    #[test]
    fn round_trips_window_list() {
        assert_eq!(WINDOW_LIST, decode(WINDOW_LIST_BUFFER.as_ref()).unwrap());
        assert_eq!(WINDOW_LIST_BUFFER, encode_vec(&WINDOW_LIST).unwrap().as_slice());
    }

    /// Was `rejects_invalid_window_support_level`, asserting that anything outside {0,1,2} is an
    /// error. That contract aborted the whole connection at Capabilities Exchange against a real
    /// Windows host, so it is inverted deliberately: an unrecognised level must decode, not fail.
    /// Note the value this test originally called invalid is 3 -- i.e. SUPPORTED | SUPPORTED_EX.
    #[test]
    fn unknown_window_support_level_is_accepted_not_rejected() {
        let decoded: WindowList = decode(&[3, 0, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(decoded.support_level, WindowSupportLevel(3));
        // And it still round-trips, so we never silently drop what the server told us.
        assert_eq!(encode_vec(&decoded).unwrap().as_slice(), &[3, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn rejects_invalid_window_list_capability_set_length() {
        assert!(decode::<CapabilitySet>(&[0x18, 0, 0x0c, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }
}
