use std::borrow::Cow;
use std::collections::HashSet;

use ironrdp_core::{Decode as _, EncodeResult, ReadCursor, cast_length, impl_as_any};
use ironrdp_dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_pdu::{PduResult, encode_err, pdu_other_err};
use ironrdp_svc::{CompressionCondition, SvcClientProcessor, SvcMessage, SvcProcessor};
use tracing::{debug, error};

use crate::pdu::{self, AudioFormat, ClientAudioOutputPdu, PitchPdu, ServerAudioFormatPdu, TrainingPdu, VolumePdu};

pub trait RdpsndClientHandler: Send + core::fmt::Debug {
    fn get_flags(&self) -> pdu::AudioFormatFlags {
        pdu::AudioFormatFlags::empty()
    }

    fn get_formats(&self) -> &[AudioFormat];

    fn wave(&mut self, format_no: usize, ts: u32, data: Cow<'_, [u8]>);

    fn set_volume(&mut self, volume: VolumePdu);

    fn set_pitch(&mut self, pitch: PitchPdu);

    fn close(&mut self);
}

#[derive(Debug)]
pub struct NoopRdpsndBackend;

impl RdpsndClientHandler for NoopRdpsndBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        &[]
    }

    fn wave(&mut self, _format_no: usize, _ts: u32, _data: Cow<'_, [u8]>) {}

    fn set_volume(&mut self, _volume: VolumePdu) {}

    fn set_pitch(&mut self, _pitch: PitchPdu) {}

    fn close(&mut self) {}
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum RdpsndState {
    Start,
    WaitingForTraining,
    Ready,
    Stop,
}

/// Required for rdpdr to work: [\[MS-RDPEFS\] Appendix A<1>]
///
/// [\[MS-RDPEFS\] Appendix A<1>]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpefs/fd28bfd9-dae2-4a78-abe1-b4efa208b7aa#Appendix_A_1
#[derive(Debug)]
pub struct Rdpsnd {
    handler: Box<dyn RdpsndClientHandler>,
    state: RdpsndState,
    server_format: Option<ServerAudioFormatPdu>,
}

impl Rdpsnd {
    pub const NAME: ChannelName = ChannelName::from_static(b"rdpsnd\0\0");

    pub fn new(handler: Box<dyn RdpsndClientHandler>) -> Self {
        Self {
            handler,
            state: RdpsndState::Start,
            server_format: None,
        }
    }

    pub fn get_format(&self, format_no: u16) -> PduResult<&AudioFormat> {
        let server_format = self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no format"))?;

        server_format
            .formats
            .get(usize::from(format_no))
            .ok_or_else(|| pdu_other_err!("invalid format"))
    }

    pub fn version(&self) -> PduResult<pdu::Version> {
        let server_format = self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no version"))?;

        Ok(server_format.version)
    }

    pub fn client_formats(&mut self) -> PduResult<Vec<ClientAudioOutputPdu>> {
        // Windows seems to be confused if the client replies with more formats, or unknown formats (e.g.: opus).
        // We ensure to only send supported formats in common with the server.
        let server_format: HashSet<_> = self
            .server_format
            .as_ref()
            .ok_or_else(|| pdu_other_err!("invalid state - no server format"))?
            .formats
            .iter()
            .collect();
        let formats: HashSet<_> = self.handler.get_formats().iter().collect();
        let formats = formats.intersection(&server_format).map(|&x| x.clone()).collect();

        let pdu = pdu::ClientAudioFormatPdu {
            version: self.version()?,
            flags: self.handler.get_flags() | pdu::AudioFormatFlags::ALIVE,
            formats,
            volume_left: 0xFFFF,
            volume_right: 0xFFFF,
            pitch: 0x00010000,
            dgram_port: 0,
        };
        Ok(vec![ClientAudioOutputPdu::AudioFormat(pdu)])
    }

    pub fn quality_mode(&mut self) -> PduResult<Vec<ClientAudioOutputPdu>> {
        let pdu = pdu::QualityModePdu {
            quality_mode: pdu::QualityMode::High,
        };
        Ok(vec![ClientAudioOutputPdu::QualityMode(pdu)])
    }

    pub fn training_confirm(&mut self, pdu: &TrainingPdu) -> PduResult<Vec<ClientAudioOutputPdu>> {
        let pack_size: EncodeResult<_> = cast_length!("wPackSize", pdu.data.len());
        let pack_size = pack_size.map_err(|e| encode_err!(e))?;
        let pdu = pdu::TrainingConfirmPdu {
            timestamp: pdu.timestamp,
            pack_size,
        };
        Ok(vec![ClientAudioOutputPdu::TrainingConfirm(pdu)])
    }

    pub fn wave_confirm(&mut self, timestamp: u16, block_no: u8) -> PduResult<Vec<ClientAudioOutputPdu>> {
        let pdu = pdu::WaveConfirmPdu { timestamp, block_no };
        Ok(vec![ClientAudioOutputPdu::WaveConfirm(pdu)])
    }

    /// Transport-agnostic core of the RDPSND client state machine.
    ///
    /// Decodes one server→client [`pdu::ServerAudioOutputPdu`] and returns the
    /// client PDUs to send back. The same PDUs flow over the static "rdpsnd" SVC
    /// (see [`SvcProcessor`] impl) and the "AUDIO_PLAYBACK_DVC" dynamic virtual
    /// channel (see [`RdpsndDvcClient`]); only the framing differs, so this logic
    /// is shared verbatim between the two transports.
    fn process_server_pdu(&mut self, payload: &[u8]) -> PduResult<Vec<ClientAudioOutputPdu>> {
        let pdu = match pdu::ServerAudioOutputPdu::decode(&mut ReadCursor::new(payload)) {
            Ok(pdu) => pdu,
            Err(error) => {
                error!(?error, "Ignoring malformed RDPSND PDU");
                return Ok(vec![]);
            }
        };

        debug!(?pdu, ?self.state);
        let msg = match self.state {
            RdpsndState::Start => {
                let pdu::ServerAudioOutputPdu::AudioFormat(af) = pdu else {
                    error!("Invalid pdu");
                    self.state = RdpsndState::Stop;
                    return Ok(vec![]);
                };
                self.server_format = Some(af);
                self.state = RdpsndState::WaitingForTraining;
                let mut msgs = self.client_formats()?;
                if self.version()? >= pdu::Version::V6 {
                    msgs.append(&mut self.quality_mode()?);
                }
                msgs
            }
            RdpsndState::WaitingForTraining => {
                let pdu::ServerAudioOutputPdu::Training(pdu) = pdu else {
                    error!("Invalid PDU");
                    self.state = RdpsndState::Stop;
                    return Ok(vec![]);
                };
                self.state = RdpsndState::Ready;
                self.training_confirm(&pdu)?
            }
            RdpsndState::Ready => {
                match pdu {
                    // TODO: handle WaveInfo for < v8
                    pdu::ServerAudioOutputPdu::Wave2(pdu) => {
                        let format_no = usize::from(pdu.format_no);
                        let ts = pdu.audio_timestamp;
                        self.handler.wave(format_no, ts, pdu.data);
                        return self.wave_confirm(pdu.timestamp, pdu.block_no);
                    }
                    pdu::ServerAudioOutputPdu::Volume(pdu) => {
                        self.handler.set_volume(pdu);
                    }
                    pdu::ServerAudioOutputPdu::Pitch(pdu) => {
                        self.handler.set_pitch(pdu);
                    }
                    pdu::ServerAudioOutputPdu::Close => {
                        self.handler.close();
                    }
                    pdu::ServerAudioOutputPdu::Training(pdu) => return self.training_confirm(&pdu),
                    pdu::ServerAudioOutputPdu::AudioFormat(af) => {
                        self.handler.close();
                        self.server_format = Some(af);
                        self.state = RdpsndState::WaitingForTraining;
                        let mut msgs = self.client_formats()?;
                        if self.version()? >= pdu::Version::V6 {
                            msgs.append(&mut self.quality_mode()?);
                        }
                        return Ok(msgs);
                    }
                    _ => {
                        error!("Invalid PDU");
                        self.state = RdpsndState::Stop;
                        return Ok(vec![]);
                    }
                }
                vec![]
            }
            state => {
                error!(?state, "Invalid state");
                vec![]
            }
        };

        Ok(msg)
    }
}

impl_as_any!(Rdpsnd);

impl SvcProcessor for Rdpsnd {
    fn channel_name(&self) -> ChannelName {
        Self::NAME
    }

    fn compression_condition(&self) -> CompressionCondition {
        CompressionCondition::Never
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        // Static "rdpsnd" SVC framing: wrap each client PDU in an SVC message.
        Ok(self
            .process_server_pdu(payload)?
            .into_iter()
            .map(SvcMessage::from)
            .collect())
    }
}

impl Drop for Rdpsnd {
    fn drop(&mut self) {
        self.handler.close();
    }
}

impl SvcClientProcessor for Rdpsnd {}

/// RDPSND audio-output client over the `AUDIO_PLAYBACK_DVC` dynamic virtual
/// channel (MS-RDPEA).
///
/// Modern hosts — Windows 10/11 and Azure Virtual Desktop in particular — prefer
/// to carry server→client audio over a dynamic virtual channel named
/// `AUDIO_PLAYBACK_DVC` rather than the legacy static "rdpsnd" channel. The
/// protocol is otherwise identical: the exact same `SNDPROLOG`-prefixed PDUs
/// (Server Audio Formats/Version, Training, Wave/Wave2, Volume, Pitch, Close)
/// flow in the same order; only the transport framing differs (a DVC opened by
/// the server via DRDYNVC instead of a preallocated MCS channel). FreeRDP models
/// this the same way — one plugin, one PDU state machine, two transports
/// (`rdpsnd_VirtualChannelEntryEx` vs `rdpsnd_DVCPluginEntry`).
///
/// This wraps the transport-agnostic [`Rdpsnd`] state machine and re-frames its
/// client PDUs as [`DvcMessage`]s. Register it alongside the static [`Rdpsnd`]
/// SVC (both share one [`RdpsndClientHandler`] sink or a clone of it) so the
/// server can pick whichever transport it prefers; only one will actually carry
/// audio in a given session.
#[derive(Debug)]
pub struct RdpsndDvcClient {
    inner: Rdpsnd,
}

impl RdpsndDvcClient {
    /// The MS-RDPEA dynamic virtual channel name for server→client audio output.
    pub const NAME: &'static str = "AUDIO_PLAYBACK_DVC";

    pub fn new(handler: Box<dyn RdpsndClientHandler>) -> Self {
        Self {
            inner: Rdpsnd::new(handler),
        }
    }
}

impl_as_any!(RdpsndDvcClient);

impl DvcProcessor for RdpsndDvcClient {
    fn channel_name(&self) -> &str {
        Self::NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // Per MS-RDPEA the server speaks first (Server Audio Formats and Version
        // PDU), exactly as on the static channel, so there is nothing to send on
        // channel creation. The state machine stays in `Start` until that PDU
        // arrives via `process`.
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // DVC framing: box each client PDU as a DvcMessage. The DRDYNVC layer
        // splits it across DATA_FIRST/DATA PDUs as needed.
        Ok(self
            .inner
            .process_server_pdu(payload)?
            .into_iter()
            .map(|pdu| Box::new(pdu) as DvcMessage)
            .collect())
    }

    // `close` intentionally left as the default no-op: the wrapped `Rdpsnd`'s
    // `Drop` invokes `handler.close()` exactly once when this processor is
    // dropped (channel teardown), matching the static-channel lifecycle.
}

impl DvcClientProcessor for RdpsndDvcClient {}
