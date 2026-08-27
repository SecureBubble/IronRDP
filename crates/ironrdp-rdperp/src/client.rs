//! The client-side `rail` static virtual channel processor (MS-RDPERP §3.2).
//!
//! Drives the RAIL handshake and launches a RemoteApp. The flow, once the
//! server opens the channel and sends its Handshake:
//!
//! 1. reply with a client Handshake,
//! 2. send the Client Information PDU (Client Status),
//! 3. send Client Execute to launch the app — its `arguments` field carries the
//!    command line natively.
//!
//! Completing this handshake is what makes a RAIL-aware proxy start forwarding
//! the server's Window List orders to us.

use ironrdp_core::{AsAny, Encode as _, decode};
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_pdu::{PduResult, decode_err};
use ironrdp_svc::{SvcClientProcessor, SvcEncode, SvcMessage, SvcProcessor};
use tracing::{debug, info, warn};

use crate::pdu::{
    Activate, ClientExecute, ClientStatus, ClientStatusFlags, Handshake, RailPdu, ServerExecuteResult, ServerMoveSize,
    SysCommand, WindowMove,
};

/// Any RAIL PDU can be sent on the channel.
impl SvcEncode for RailPdu {}

/// A published application to launch over RAIL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteApp {
    /// The executable or file to run (`||<alias>` for a published app, or a path).
    pub exe_or_file: String,
    pub working_dir: String,
    /// The command-line argument(s). Empty for none.
    pub arguments: String,
}

/// Client build number reported in the Handshake. Mirrors what real clients
/// send; the exact value is not significant to the server.
const CLIENT_BUILD_NUMBER: u32 = 0x0000_1DB1; // 7601

#[derive(Debug)]
pub struct RailChannel {
    handshook: bool,
    client_build_number: u32,
    client_status_flags: ClientStatusFlags,
    launch: Option<ClientExecute>,
    /// Server Move/Size (§2.2.2.7.2) events received since the last drain. The SVC processor
    /// only decodes them; the run loop drives the actual local move (it owns the compositor +
    /// input) via [`RailChannel::take_move_size_events`].
    pending_move_size: Vec<ServerMoveSize>,
}

impl RailChannel {
    const CHANNEL_NAME: ChannelName = ChannelName::from_static(b"rail\0\0\0\0");

    /// A RAIL channel that completes the handshake but launches nothing (the app
    /// is launched some other way, or this is a diagnostic connection).
    pub fn new() -> Self {
        Self {
            handshook: false,
            client_build_number: CLIENT_BUILD_NUMBER,
            client_status_flags: ClientStatusFlags::ALLOWLOCALMOVESIZE | ClientStatusFlags::AUTORECONNECT,
            launch: None,
            pending_move_size: Vec::new(),
        }
    }

    /// A RAIL channel that launches `app` (with its command line) once the
    /// handshake completes.
    pub fn with_app(app: RemoteApp) -> Self {
        let mut this = Self::new();
        this.launch = Some(ClientExecute {
            flags: crate::pdu::ClientExecuteFlags::EXPAND_WORKING_DIRECTORY
                | crate::pdu::ClientExecuteFlags::EXPAND_ARGUMENTS,
            exe_or_file: app.exe_or_file,
            working_dir: app.working_dir,
            arguments: app.arguments,
        });
        this
    }

    /// Override the flags sent in the Client Information PDU.
    #[must_use]
    pub fn with_client_status_flags(mut self, flags: ClientStatusFlags) -> Self {
        self.client_status_flags = flags;
        self
    }

    /// Notify the server that a RAIL window gained/lost focus (§2.2.2.6.1).
    pub fn activate(&self, window_id: u32, enabled: bool) -> SvcMessage {
        SvcMessage::from(RailPdu::Activate(Activate { window_id, enabled }))
    }

    /// Launch an ADDITIONAL RemoteApp on the live session (§2.2.2.3.1), without reconnecting.
    ///
    /// Same PDU as the initial launch, just sent later. The initial one rides `with_app` at
    /// handshake because there is nothing to send it on before that; nothing in MS-RDPERP limits a
    /// session to one Execute, and the Microsoft AVD web client relies on that -- a captured
    /// session shows three different app GUIDs launched over ONE connection.
    ///
    /// On AVD `exe_or_file` is the published-app resource id in the `||<guid>` form, not a path,
    /// so the caller passes what the workspace API returned.
    pub fn launch_app(&self, app: RemoteApp) -> SvcMessage {
        SvcMessage::from(RailPdu::ClientExecute(ClientExecute {
            flags: crate::pdu::ClientExecuteFlags::EXPAND_WORKING_DIRECTORY
                | crate::pdu::ClientExecuteFlags::EXPAND_ARGUMENTS,
            exe_or_file: app.exe_or_file,
            working_dir: app.working_dir,
            arguments: app.arguments,
        }))
    }

    /// Build a `TS_RAIL_ORDER_SYSCOMMAND` (§2.2.2.6.2): minimise / maximise / restore / close.
    ///
    /// `command` is one of the `SC_*` constants. Note `SC_RESTORE` is NOT safe to send blindly —
    /// on a maximised window it un-maximises it — so callers must check the window is actually
    /// minimised first.
    pub fn sys_command(&self, window_id: u32, command: u16) -> SvcMessage {
        SvcMessage::from(RailPdu::SysCommand(SysCommand { window_id, command }))
    }

    /// Drain the Server Move/Size events (§2.2.2.7.2) received since the last call. The run loop
    /// polls this each iteration to begin/end a local window drag.
    pub fn take_move_size_events(&mut self) -> Vec<ServerMoveSize> {
        core::mem::take(&mut self.pending_move_size)
    }

    /// Build a client Window Move PDU (§2.2.2.7.4) reporting a window's final rectangle after a
    /// local move. `right`/`bottom` are exclusive. Send it on this channel when the drag ends.
    pub fn window_move(&self, window_id: u32, left: i16, top: i16, right: i16, bottom: i16) -> SvcMessage {
        SvcMessage::from(RailPdu::WindowMove(WindowMove {
            window_id,
            left,
            top,
            right,
            bottom,
        }))
    }

    /// Build the client's response to a server Handshake: reply + client status
    /// + (optionally) launch the app. Fired only once.
    fn on_server_handshake(&mut self) -> Vec<SvcMessage> {
        if self.handshook {
            debug!("ignoring duplicate RAIL server handshake");
            return Vec::new();
        }
        self.handshook = true;

        let mut messages = vec![
            SvcMessage::from(RailPdu::Handshake(Handshake {
                build_number: self.client_build_number,
            })),
            SvcMessage::from(RailPdu::ClientStatus(ClientStatus {
                flags: self.client_status_flags,
            })),
        ];

        if let Some(exec) = self.launch.clone() {
            info!(exe = %exec.exe_or_file, args = %exec.arguments, "RAIL: launching RemoteApp");
            messages.push(SvcMessage::from(RailPdu::ClientExecute(exec)));
        }

        messages
    }

    fn on_exec_result(result: &ServerExecuteResult) {
        if result.exec_result == 0 {
            info!(exe = %result.exe_or_file, "RAIL: RemoteApp launch acknowledged");
        } else {
            warn!(
                exe = %result.exe_or_file,
                exec_result = result.exec_result,
                raw_result = result.raw_result,
                "RAIL: RemoteApp launch failed"
            );
        }
    }
}

impl Default for RailChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl AsAny for RailChannel {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl SvcProcessor for RailChannel {
    fn channel_name(&self) -> ChannelName {
        Self::CHANNEL_NAME
    }

    fn start(&mut self) -> PduResult<Vec<SvcMessage>> {
        // The server drives the handshake; we respond in `process`.
        Ok(Vec::new())
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        // DIAG: the raw orderType (first 2 bytes) of every RAIL SVC PDU, so a live console shows
        // exactly what the host sends during a drag (LocalMoveSize is 0x0009).
        let raw_order_type = if payload.len() >= 2 {
            u16::from_le_bytes([payload[0], payload[1]])
        } else {
            0
        };
        let pdu = decode::<RailPdu>(payload).map_err(|e| decode_err!(e))?;
        debug!(target: "rail_diag", order_type = format!("{raw_order_type:#06x}"), pdu = pdu.name(), "RAIL SVC recv");

        match pdu {
            // HiDef is negotiated primarily via the `INFO_HIDEF_RAIL_SUPPORTED`
            // Client Info PDU flag (set in ironrdp-connector). The server's
            // HandshakeEx carries `HandshakeExFlags::HIDEF` as a secondary,
            // informational advertisement; per MS-RDPERP the client replies to
            // both Handshake and HandshakeEx with a plain Handshake PDU, so we
            // do not echo a separate HandshakeEx. TODO: if the host requires the
            // client to reflect specific HandshakeEx flags, store and honor
            // `HandshakeEx::rail_handshake_flags` here.
            RailPdu::Handshake(_) | RailPdu::HandshakeEx(_) => Ok(self.on_server_handshake()),
            RailPdu::ServerExecuteResult(result) => {
                Self::on_exec_result(&result);
                Ok(Vec::new())
            }
            RailPdu::ServerMoveSize(ms) => {
                // Queue for the run loop, which owns the compositor + input and runs the actual
                // local move. No SVC response is emitted from here.
                debug!(
                    target: "rail_diag",
                    window_id = format!("{:#x}", ms.window_id),
                    start = ms.is_move_size_start,
                    move_size_type = ms.move_size_type,
                    pos_x = ms.pos_x,
                    pos_y = ms.pos_y,
                    "RAIL: server move/size"
                );
                self.pending_move_size.push(ms);
                Ok(Vec::new())
            }
            other => {
                debug!(pdu = other.name(), "RAIL: ignoring unhandled server PDU");
                Ok(Vec::new())
            }
        }
    }
}

impl SvcClientProcessor for RailChannel {}

#[cfg(test)]
mod tests {
    use ironrdp_core::encode_vec;

    use super::*;
    use crate::pdu::RailOrderType;

    fn server_handshake_bytes() -> Vec<u8> {
        encode_vec(&Handshake { build_number: 7601 }).unwrap()
    }

    #[test]
    fn handshake_triggers_reply_status_and_exec() {
        let mut chan = RailChannel::with_app(RemoteApp {
            exe_or_file: r"C:\Windows\explorer.exe".to_owned(),
            working_dir: String::new(),
            arguments: r"C:\Sales".to_owned(),
        });

        let out = chan.process(&server_handshake_bytes()).unwrap();
        assert_eq!(out.len(), 3, "expected Handshake + ClientStatus + Execute");

        let order_types: Vec<_> = out
            .iter()
            .map(|m| {
                let bytes = m.encode_unframed_pdu().unwrap();
                crate::pdu::peek_order_type(&bytes).unwrap()
            })
            .collect();
        assert_eq!(
            order_types,
            vec![
                RailOrderType::Handshake,
                RailOrderType::ClientStatus,
                RailOrderType::Exec
            ]
        );

        // The Execute PDU must carry the command line.
        let exec_bytes = out[2].encode_unframed_pdu().unwrap();
        let RailPdu::ClientExecute(exec) = decode::<RailPdu>(&exec_bytes).unwrap() else {
            panic!("third message must be Client Execute");
        };
        assert_eq!(exec.arguments, r"C:\Sales");
    }

    #[test]
    fn handshake_reply_fires_only_once() {
        let mut chan = RailChannel::new();
        assert_eq!(chan.process(&server_handshake_bytes()).unwrap().len(), 2); // no app -> reply + status
        assert!(chan.process(&server_handshake_bytes()).unwrap().is_empty());
    }
}
