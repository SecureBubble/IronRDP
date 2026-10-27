use bitflags::bitflags;
use ironrdp_core::{
    Decode, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor, ensure_fixed_part_size, invalid_field_err,
};

/// [2.2.1.1.1] Remote Programs Capability Set
///
/// [2.2.1.1.1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdperp
#[derive(Debug, PartialEq, Eq, Clone)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub struct Rail {
    pub support_level: RailSupportLevel,
}

impl Rail {
    const NAME: &'static str = "Rail";

    pub(crate) const FIXED_PART_SIZE: usize = 4 /* RailSupportLevel */;

    fn is_valid(&self) -> bool {
        self.support_level.bits() & !RailSupportLevel::all().bits() == 0
            && (self.support_level.contains(RailSupportLevel::SUPPORTED) || self.support_level.is_empty())
    }
}

impl Encode for Rail {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);

        if !self.is_valid() {
            return Err(invalid_field_err!(
                "railSupportLevel",
                "RAIL extension support requires remote programs support"
            ));
        }

        dst.write_u32(self.support_level.bits());

        Ok(())
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

impl<'de> Decode<'de> for Rail {
    fn decode(src: &mut ReadCursor<'de>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);

        // Be STRICT when encoding (see `Encode`, which still rejects a malformed value we build
        // ourselves) but LIBERAL when decoding a server's capability set.
        //
        // This used to `from_bits` -- rejecting any bit IronRDP does not model -- and then also
        // reject anything failing `is_valid()`, which rejects unknown bits a second time. Either
        // one aborts the ENTIRE connection at Capabilities Exchange. The sibling `wndSupportLevel`
        // had exactly this bug and a real Windows host tripped it, so do not leave the same
        // landmine here: an unrecognised RAIL support bit tells us nothing we need at decode time.
        let support_level = RailSupportLevel::from_bits_retain(src.read_u32());

        Ok(Self { support_level })
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
    pub struct RailSupportLevel: u32 {
        const SUPPORTED = 0x0000_0001;
        const DOCKED_LANGBAR_SUPPORTED = 0x0000_0002;
        const SHELL_INTEGRATION_SUPPORTED = 0x0000_0004;
        const LANGUAGE_IME_SYNC_SUPPORTED = 0x0000_0008;
        const SERVER_TO_CLIENT_IME_SYNC_SUPPORTED = 0x0000_0010;
        const HIDE_MINIMIZED_APPS_SUPPORTED = 0x0000_0020;
        const WINDOW_CLOAKING_SUPPORTED = 0x0000_0040;
        const HANDSHAKE_EX_SUPPORTED = 0x0000_0080;
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_core::{decode, encode_vec};

    use super::super::CapabilitySet;
    use super::*;

    const RAIL_PDU_BUFFER: [u8; 4] = [0xff, 0x00, 0x00, 0x00];

    fn rail() -> Rail {
        Rail {
            support_level: RailSupportLevel::SUPPORTED
                | RailSupportLevel::DOCKED_LANGBAR_SUPPORTED
                | RailSupportLevel::SHELL_INTEGRATION_SUPPORTED
                | RailSupportLevel::LANGUAGE_IME_SYNC_SUPPORTED
                | RailSupportLevel::SERVER_TO_CLIENT_IME_SYNC_SUPPORTED
                | RailSupportLevel::HIDE_MINIMIZED_APPS_SUPPORTED
                | RailSupportLevel::WINDOW_CLOAKING_SUPPORTED
                | RailSupportLevel::HANDSHAKE_EX_SUPPORTED,
        }
    }

    #[test]
    fn from_buffer_correctly_parses_rail() {
        assert_eq!(rail(), decode(RAIL_PDU_BUFFER.as_ref()).unwrap());
    }

    #[test]
    fn to_buffer_correctly_serializes_rail() {
        assert_eq!(RAIL_PDU_BUFFER, encode_vec(&rail()).unwrap().as_slice());
    }

    #[test]
    fn buffer_length_is_correct_for_rail() {
        assert_eq!(RAIL_PDU_BUFFER.len(), rail().size());
    }

    /// Strict on the way OUT, liberal on the way IN.
    ///
    /// Encoding a malformed value we constructed ourselves is still an error. DECODING a server's
    /// capability set is not: rejecting an unmodelled bit there kills the entire connection at
    /// Capabilities Exchange, which is precisely what the sibling `wndSupportLevel` did against a
    /// real Windows host.
    #[test]
    fn unknown_rail_support_level_decodes_but_does_not_encode() {
        // Unknown bit, and an extension bit without the base SUPPORTED bit: both decode fine now.
        assert!(decode::<Rail>(&[0x00, 0x01, 0x00, 0x00]).is_ok());
        assert!(decode::<Rail>(&[0x02, 0x00, 0x00, 0x00]).is_ok());
        // Encoding still refuses to emit something malformed of our own making.
        assert!(
            encode_vec(&Rail {
                support_level: RailSupportLevel::from_bits_retain(0x0000_0100),
            })
            .is_err()
        );
    }

    #[test]
    fn invalid_rail_capability_set_length_is_rejected() {
        assert!(decode::<CapabilitySet>(&[0x17, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).is_err());
    }
}
