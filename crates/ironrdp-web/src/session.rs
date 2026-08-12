use core::cell::RefCell;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::num::NonZeroU32;
use core::time::Duration;
use std::borrow::Cow;
use std::rc::Rc;

use anyhow::Context as _;
use base64::Engine as _;
use futures_channel::mpsc;
use futures_util::io::{ReadHalf, WriteHalf};
use futures_util::{AsyncWriteExt as _, FutureExt as _, StreamExt as _, select};
use gloo_net::websocket;
use gloo_net::websocket::futures::WebSocket;
use gloo_timers::future::IntervalStream;
use iron_remote_desktop::{CursorStyle, DesktopSize, Extension, IronErrorKind};
use ironrdp::cliprdr::CliprdrClient;
use ironrdp::cliprdr::backend::ClipboardMessage;
use ironrdp::cliprdr::pdu::{FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor};
use ironrdp::connector::connection_activation::ConnectionActivationState;
use ironrdp::connector::credssp::KerberosConfig;
use ironrdp::connector::{self, ClientConnector, Credentials};
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::gcc::{Monitor as GccMonitor, MonitorFlags};
use ironrdp::pdu::input::fast_path::FastPathInputEvent;
use ironrdp::pdu::rdp::capability_sets::client_codecs_capabilities;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::rdpdr::Rdpdr;
use ironrdp::rdpdr::pdu::efs::{DEFAULT_PRINTER_DRIVER_NAME, MICROSOFT_PRINT_TO_PDF_DRIVER_NAME};
use ironrdp::rdperp::client::{RailChannel, RemoteApp};
use ironrdp::rdperp::orders::WindowOrder;
use ironrdp::rdpsnd::client::{NoopRdpsndBackend, Rdpsnd, RdpsndDvcClient};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput, GracefulDisconnectReason};
use ironrdp_core::WriteBuf;
use ironrdp_egfx::client::GraphicsPipelineClient;
use ironrdp_futures::{FramedWrite, single_sequence_step_read};
use rgb::AsPixels as _;
use tap::prelude::*;
use tracing::{debug, error, info, trace, warn};
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlCanvasElement;

use crate::canvas::Canvas;
use crate::clipboard;
use crate::clipboard::{ClipboardData, FileMetadata, WasmClipboard, WasmClipboardBackend, WasmClipboardBackendMessage};
use crate::error::IronError;
use crate::graphics::{blend_watermark_into, WasmGraphicsHandler, WasmGraphicsMessageProxy, Watermark};
use crate::image::extract_partial_image;
use crate::input::InputTransaction;
use crate::network_client::WasmNetworkClient;
use crate::printer::{JsPrinterStreamCallbacks, WasmPrinter, WasmPrinterBackend, wasm_printer_pair};
use crate::sound::{JsSoundCallbacks, WasmSound, WasmSoundBackend, wasm_sound_pair};

const DEFAULT_WIDTH: u16 = 1280;
const DEFAULT_HEIGHT: u16 = 720;

#[derive(Clone, Default)]
pub(crate) struct SessionBuilder(Rc<RefCell<SessionBuilderInner>>);

struct SessionBuilderInner {
    username: Option<String>,
    destination: Option<String>,
    server_domain: Option<String>,
    password: Option<String>,
    proxy_address: Option<String>,
    auth_token: Option<String>,
    pcb: Option<String>,
    load_balance_info: Option<String>,
    kdc_proxy_url: Option<String>,
    // RemoteApp-style "published app": program to run as the session shell instead
    // of the full desktop (RDP alternate shell). Empty/None => normal desktop.
    alternate_shell: Option<String>,
    // RAIL (Remote Programs) app to launch over the `rail` static channel, set via
    // the `remote_app` extension. When present the client negotiates RAIL and
    // launches this app (its command line travels natively). Use ONLY for non-AVD
    // targets: AVD does RAIL over DVC + eGFX, which this classic path must not
    // shadow — so it is opt-in and never enabled unless a caller sets it.
    remote_app: Option<connector::RailConfig>,
    client_name: String,
    desktop_size: DesktopSize,
    /// Multi-monitor layout (Phase 0) parsed from the `monitors` extension. Empty
    /// means a single implicit monitor (legacy behavior). When non-empty, it is
    /// advertised at connect time as GCC Client Monitor Data and `desktop_size` is
    /// overridden with the bounding box of all monitors (the spanning virtual
    /// desktop). Rectangles use inclusive right/bottom (`right = left + width - 1`).
    monitors: Vec<GccMonitor>,

    render_canvas: Option<HtmlCanvasElement>,
    set_cursor_style_callback: Option<js_sys::Function>,
    set_cursor_style_callback_context: Option<JsValue>,
    remote_clipboard_changed_callback: Option<js_sys::Function>,
    force_clipboard_update_callback: Option<js_sys::Function>,
    // File transfer callbacks
    files_available_callback: Option<js_sys::Function>,
    file_contents_request_callback: Option<js_sys::Function>,
    file_contents_response_callback: Option<js_sys::Function>,
    lock_callback: Option<js_sys::Function>,
    unlock_callback: Option<js_sys::Function>,
    locks_expired_callback: Option<js_sys::Function>,
    format_list_response_callback: Option<js_sys::Function>,

    /// WebCodecs AVC decode callback (extension `avc_decode_callback`). The run loop
    /// calls it with a compressed H.264 main sub-stream; JS decodes it and returns
    /// RGBA via the `on_avc_decoded` extension. Absent = AVC frames dropped (logged).
    avc_decode_callback: Option<js_sys::Function>,
    /// AVC watermark callback (extension `avc_watermark_callback`). The run loop calls
    /// it when the session watermark changes so the GPU direct-draw path can overdraw
    /// it onto each AVC frame (the CPU path re-blends it in Rust instead).
    avc_watermark_callback: Option<js_sys::Function>,
    /// Render-canvas update notification (extension `canvas_updated_callback`). Fired
    /// once per rendered NON-AVC region (eGFX blit + CPU AVC readback) with the updated
    /// rect in source-canvas pixel coords, so an external multi-monitor presenter can
    /// redraw only the affected area on a real pixel change instead of a blind timer.
    /// Passive: it never touches frame-ack / present flow. (The AVC GPU direct-draw path
    /// notifies JS-side from `AvcDecoder`, since it never re-enters this run loop.)
    canvas_updated_callback: Option<js_sys::Function>,

    // Setting printer stream callbacks activates the virtual printer.
    invalid_print_job_stream_callbacks: bool,
    print_job_stream_callbacks: Option<JsPrinterStreamCallbacks>,
    printer_name: Option<String>,
    printer_device_id: Option<u32>,
    printer_driver_name: Option<String>,

    // Setting sound callbacks activates RDPSND audio playback (server → client).
    invalid_sound_callbacks: bool,
    sound_callbacks: Option<JsSoundCallbacks>,

    use_display_control: bool,
    enable_credssp: bool,
    // Advertise AVC420/AVC444 eGFX caps so the server sends H.264 (decoded by the
    // browser WebCodecs path). Controlled per-connection via the `advertise_avc`
    // extension so the web UI's "Enhanced graphics" toggle can turn it off (off =>
    // server falls back to ClearCodec / RFX-Progressive). Defaults to true to
    // preserve behavior for callers that don't set it.
    advertise_avc: bool,
    outbound_message_size_limit: Option<usize>,
}

impl Default for SessionBuilderInner {
    fn default() -> Self {
        Self {
            username: None,
            destination: None,
            server_domain: None,
            password: None,
            proxy_address: None,
            auth_token: None,
            pcb: None,
            load_balance_info: None,
            kdc_proxy_url: None,
            alternate_shell: None,
            remote_app: None,
            client_name: "ironrdp-web".to_owned(),
            desktop_size: DesktopSize {
                width: DEFAULT_WIDTH,
                height: DEFAULT_HEIGHT,
            },
            monitors: Vec::new(),

            render_canvas: None,
            set_cursor_style_callback: None,
            set_cursor_style_callback_context: None,
            remote_clipboard_changed_callback: None,
            force_clipboard_update_callback: None,
            files_available_callback: None,
            file_contents_request_callback: None,
            file_contents_response_callback: None,
            lock_callback: None,
            unlock_callback: None,
            locks_expired_callback: None,
            format_list_response_callback: None,
            avc_decode_callback: None,
            avc_watermark_callback: None,
            canvas_updated_callback: None,

            invalid_print_job_stream_callbacks: false,
            print_job_stream_callbacks: None,
            printer_name: None,
            printer_device_id: None,
            printer_driver_name: None,

            invalid_sound_callbacks: false,
            sound_callbacks: None,

            use_display_control: false,
            enable_credssp: true,
            advertise_avc: true,
            outbound_message_size_limit: None,
        }
    }
}

impl iron_remote_desktop::SessionBuilder for SessionBuilder {
    type Session = Session;
    type Error = IronError;

    fn create() -> Self {
        Self(Rc::new(RefCell::new(SessionBuilderInner::default())))
    }

    /// Required
    fn username(&self, username: String) -> Self {
        self.0.borrow_mut().username = Some(username);
        self.clone()
    }

    /// Required
    fn destination(&self, destination: String) -> Self {
        self.0.borrow_mut().destination = Some(destination);
        self.clone()
    }

    /// Optional
    fn server_domain(&self, server_domain: String) -> Self {
        self.0.borrow_mut().server_domain = if server_domain.is_empty() {
            None
        } else {
            Some(server_domain)
        };
        self.clone()
    }

    /// Required
    fn password(&self, password: String) -> Self {
        self.0.borrow_mut().password = Some(password);
        self.clone()
    }

    /// Required
    fn proxy_address(&self, address: String) -> Self {
        self.0.borrow_mut().proxy_address = Some(address);
        self.clone()
    }

    /// Required
    fn auth_token(&self, token: String) -> Self {
        self.0.borrow_mut().auth_token = Some(token);
        self.clone()
    }

    /// Optional
    fn desktop_size(&self, desktop_size: DesktopSize) -> Self {
        self.0.borrow_mut().desktop_size = desktop_size;
        self.clone()
    }

    /// Optional
    fn render_canvas(&self, canvas: HtmlCanvasElement) -> Self {
        self.0.borrow_mut().render_canvas = Some(canvas);
        self.clone()
    }

    /// Required.
    ///
    /// # Callback signature:
    /// ```typescript
    /// function callback(
    ///     cursor_kind: string,
    ///     cursor_data: string | undefined,
    ///     hotspot_x: number | undefined,
    ///     hotspot_y: number | undefined
    /// ): void
    /// ```
    ///
    /// # Cursor kinds:
    /// - `default` (default system cursor); other arguments are `UNDEFINED`
    /// - `none` (hide cursor); other arguments are `UNDEFINED`
    /// - `url` (custom cursor data URL); `cursor_data` contains the data URL with Base64-encoded
    ///   cursor bitmap; `hotspot_x` and `hotspot_y` are set to the cursor hotspot coordinates.
    fn set_cursor_style_callback(&self, callback: js_sys::Function) -> Self {
        self.0.borrow_mut().set_cursor_style_callback = Some(callback);
        self.clone()
    }

    /// Required.
    fn set_cursor_style_callback_context(&self, context: JsValue) -> Self {
        self.0.borrow_mut().set_cursor_style_callback_context = Some(context);
        self.clone()
    }

    /// Optional
    fn remote_clipboard_changed_callback(&self, callback: js_sys::Function) -> Self {
        self.0.borrow_mut().remote_clipboard_changed_callback = Some(callback);
        self.clone()
    }

    /// Optional
    fn force_clipboard_update_callback(&self, callback: js_sys::Function) -> Self {
        self.0.borrow_mut().force_clipboard_update_callback = Some(callback);
        self.clone()
    }

    /// Because the server does not resize the framebuffer in the RDP protocol, this feature is unused in IronRDP.
    fn canvas_resized_callback(&self, _callback: js_sys::Function) -> Self {
        self.clone()
    }

    fn extension(&self, ext: Extension) -> Self {
        iron_remote_desktop::extension_match! {
            match ext;
            |pcb: String| { self.0.borrow_mut().pcb = Some(pcb) };
            |load_balance_info: String| { self.0.borrow_mut().load_balance_info = Some(load_balance_info) };
            |kdc_proxy_url: String| { self.0.borrow_mut().kdc_proxy_url = Some(kdc_proxy_url) };
            |alternate_shell: String| { self.0.borrow_mut().alternate_shell = Some(alternate_shell) };
            |remote_app: JsValue| { self.0.borrow_mut().remote_app = parse_remote_app(&remote_app) };
            |display_control: bool| { self.0.borrow_mut().use_display_control = display_control };
            |enable_credssp: bool| { self.0.borrow_mut().enable_credssp = enable_credssp };
            |advertise_avc: bool| { self.0.borrow_mut().advertise_avc = advertise_avc };
            // Multi-monitor layout (Phase 0). Accepts a JSON array of
            // `{ left, top, width, height, primary? }` in virtual-desktop pixel
            // coordinates; the primary should be at (0,0). When set, the client
            // advertises >1 monitor at connect time (GCC Client Monitor Data) so the
            // remote host produces one spanning desktop (bounding box), rendered into
            // the single canvas. An empty/invalid layout falls back to single-monitor.
            |monitors: JsValue| { self.0.borrow_mut().monitors = parse_monitors(&monitors) };
            |outbound_message_size_limit: f64| {
                let limit = if outbound_message_size_limit >= 0.0 && outbound_message_size_limit <= f64::from(u32::MAX) {
                    #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    { outbound_message_size_limit as usize }
                } else {
                    warn!(outbound_message_size_limit, "Invalid outbound message size limit; fallback to unlimited");
                    0 // Fallback to no limit for invalid values.
                };
                self.0.borrow_mut().outbound_message_size_limit = if limit > 0 { Some(limit) } else { None };
            };
            // File transfer callbacks - protocol-specific, routed through extension()
            // rather than dedicated trait methods to keep iron-remote-desktop protocol-agnostic.
            |files_available_callback: JsValue| {
                self.0.borrow_mut().files_available_callback = files_available_callback.dyn_into::<js_sys::Function>().ok();
            };
            |file_contents_request_callback: JsValue| {
                self.0.borrow_mut().file_contents_request_callback = file_contents_request_callback.dyn_into::<js_sys::Function>().ok();
            };
            |file_contents_response_callback: JsValue| {
                self.0.borrow_mut().file_contents_response_callback = file_contents_response_callback.dyn_into::<js_sys::Function>().ok();
            };
            |lock_callback: JsValue| {
                self.0.borrow_mut().lock_callback = lock_callback.dyn_into::<js_sys::Function>().ok();
            };
            |unlock_callback: JsValue| {
                self.0.borrow_mut().unlock_callback = unlock_callback.dyn_into::<js_sys::Function>().ok();
            };
            |locks_expired_callback: JsValue| {
                self.0.borrow_mut().locks_expired_callback = locks_expired_callback.dyn_into::<js_sys::Function>().ok();
            };
            |format_list_response_callback: JsValue| {
                self.0.borrow_mut().format_list_response_callback = format_list_response_callback.dyn_into::<js_sys::Function>().ok();
            };
            |print_job_stream_callbacks: JsValue| {
                let mut inner = self.0.borrow_mut();
                match parse_print_job_stream_callbacks(print_job_stream_callbacks) {
                    Ok(callbacks) => {
                        inner.invalid_print_job_stream_callbacks = false;
                        inner.print_job_stream_callbacks = Some(callbacks);
                    }
                    Err(error) => {
                        inner.invalid_print_job_stream_callbacks = true;
                        inner.print_job_stream_callbacks = None;
                        warn!(%error, "Invalid print_job_stream_callbacks; printer streaming requires onJobData and onJobComplete functions");
                    }
                }
            };
            |printer_name: String| {
                let mut inner = self.0.borrow_mut();
                inner.printer_name = if printer_name.is_empty() { None } else { Some(printer_name) };
            };
            |printer_device_id: f64| {
                let id = if printer_device_id >= 0.0 && printer_device_id <= f64::from(u32::MAX) {
                    #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    { printer_device_id as u32 }
                } else {
                    warn!(printer_device_id, "Invalid printer_device_id; falling back to default");
                    0
                };
                let mut inner = self.0.borrow_mut();
                inner.printer_device_id = if id > 0 { Some(id) } else { None };
            };
            |printer_driver_name: String| {
                let mut inner = self.0.borrow_mut();
                inner.printer_driver_name = if printer_driver_name.is_empty() {
                    None
                } else {
                    Some(printer_driver_name)
                };
            };
            // Registering sound callbacks activates RDPSND server→client audio.
            |sound_callbacks: JsValue| {
                let mut inner = self.0.borrow_mut();
                match parse_sound_callbacks(sound_callbacks) {
                    Ok(callbacks) => {
                        inner.invalid_sound_callbacks = false;
                        inner.sound_callbacks = Some(callbacks);
                    }
                    Err(error) => {
                        inner.invalid_sound_callbacks = true;
                        inner.sound_callbacks = None;
                        warn!(%error, "Invalid sound_callbacks; audio playback requires an onWave function");
                    }
                }
            };
            // WebCodecs H.264 decode callback. Registering it lets the run loop hand
            // AVC main sub-streams to the browser decoder (see the `Avc` run-loop arm).
            |avc_decode_callback: JsValue| {
                self.0.borrow_mut().avc_decode_callback = avc_decode_callback.dyn_into::<js_sys::Function>().ok();
            };
            |avc_watermark_callback: JsValue| {
                self.0.borrow_mut().avc_watermark_callback = avc_watermark_callback.dyn_into::<js_sys::Function>().ok();
            };
            // Passive render-canvas update notification for the multi-monitor presenter
            // (see field docs). Fires once per drawn NON-AVC region; the AVC GPU path
            // notifies JS-side. Never affects protocol / frame-ack state.
            |canvas_updated_callback: JsValue| {
                self.0.borrow_mut().canvas_updated_callback = canvas_updated_callback.dyn_into::<js_sys::Function>().ok();
            };
        }

        self.clone()
    }

    async fn connect(&self) -> Result<Self::Session, Self::Error> {
        let (
            username,
            destination,
            server_domain,
            password,
            proxy_address,
            auth_token,
            pcb,
            kdc_proxy_url,
            client_name,
            desktop_size,
            render_canvas,
            set_cursor_style_callback,
            set_cursor_style_callback_context,
            remote_clipboard_changed_callback,
            force_clipboard_update_callback,
            files_available_callback,
            file_contents_request_callback,
            file_contents_response_callback,
            lock_callback,
            unlock_callback,
            locks_expired_callback,
            format_list_response_callback,
            avc_decode_callback,
            avc_watermark_callback,
            canvas_updated_callback,
            invalid_print_job_stream_callbacks,
            print_job_stream_callbacks,
            printer_name,
            printer_device_id,
            printer_driver_name,
            invalid_sound_callbacks,
            sound_callbacks,
            outbound_message_size_limit,
        );

        {
            let inner = self.0.borrow();

            username = inner.username.clone().context("username missing")?;
            destination = inner.destination.clone().context("destination missing")?;
            server_domain = inner.server_domain.clone();
            password = inner.password.clone().context("password missing")?;
            proxy_address = inner.proxy_address.clone().context("proxy_address missing")?;
            auth_token = inner.auth_token.clone().context("auth_token missing")?;
            pcb = inner.pcb.clone();
            kdc_proxy_url = inner.kdc_proxy_url.clone();
            client_name = inner.client_name.clone();
            desktop_size = inner.desktop_size;

            render_canvas = inner.render_canvas.clone().context("render_canvas missing")?;

            set_cursor_style_callback = inner
                .set_cursor_style_callback
                .clone()
                .context("set_cursor_style_callback missing")?;
            set_cursor_style_callback_context = inner
                .set_cursor_style_callback_context
                .clone()
                .context("set_cursor_style_callback_context missing")?;
            remote_clipboard_changed_callback = inner.remote_clipboard_changed_callback.clone();
            force_clipboard_update_callback = inner.force_clipboard_update_callback.clone();
            files_available_callback = inner.files_available_callback.clone();
            file_contents_request_callback = inner.file_contents_request_callback.clone();
            file_contents_response_callback = inner.file_contents_response_callback.clone();
            lock_callback = inner.lock_callback.clone();
            unlock_callback = inner.unlock_callback.clone();
            locks_expired_callback = inner.locks_expired_callback.clone();
            format_list_response_callback = inner.format_list_response_callback.clone();
            avc_decode_callback = inner.avc_decode_callback.clone();
            avc_watermark_callback = inner.avc_watermark_callback.clone();
            canvas_updated_callback = inner.canvas_updated_callback.clone();
            invalid_print_job_stream_callbacks = inner.invalid_print_job_stream_callbacks;
            print_job_stream_callbacks = inner.print_job_stream_callbacks.clone();
            printer_name = inner.printer_name.clone();
            printer_device_id = inner.printer_device_id;
            printer_driver_name = inner.printer_driver_name.clone();
            invalid_sound_callbacks = inner.invalid_sound_callbacks;
            sound_callbacks = inner.sound_callbacks.clone();
            outbound_message_size_limit = inner.outbound_message_size_limit;
        }

        info!("Connect to RDP host");

        let mut config = build_config(username, password, server_domain, client_name.clone(), desktop_size);

        let enable_credssp = self.0.borrow().enable_credssp;
        config.enable_credssp = enable_credssp;

        // Multi-monitor (Phase 0): if the caller supplied a monitor layout via the
        // `monitors` extension, advertise it as GCC Client Monitor Data and size the
        // requested desktop to the bounding box of all monitors. The server then
        // produces one spanning virtual desktop; the negotiated size flows back as
        // `connection_result.desktop_size`, which already drives the canvas /
        // DecodedImage sizing, and the eGFX compositor blits each output-mapped
        // surface at its virtual-desktop origin into that single framebuffer.
        let monitors = self.0.borrow().monitors.clone();
        if !monitors.is_empty() {
            if let Some((bbox_width, bbox_height)) = monitors_bounding_box(&monitors) {
                config.desktop_size = connector::DesktopSize {
                    width: bbox_width,
                    height: bbox_height,
                };
                info!(
                    monitor_count = monitors.len(),
                    bounding_box = format!("{bbox_width}x{bbox_height}"),
                    "Advertising multi-monitor layout (spanning virtual desktop)"
                );
                config.monitors = monitors;
            } else {
                warn!("Ignoring multi-monitor layout: could not compute a valid bounding box");
            }
        }

        // RemoteApp-style published app: run a single program as the session shell
        // (RDP "alternate shell") instead of the full desktop. The API supplies the
        // program path per app; the Bubble proxy forwards this alternate shell to the
        // target on its back leg, so clicking an app opens a full session running just
        // that app (session ends when the app closes). Empty => normal desktop.
        let alternate_shell = self.0.borrow().alternate_shell.clone();
        if let Some(alternate_shell) = alternate_shell {
            config.alternate_shell = alternate_shell;
        }

        // RAIL (Remote Programs): when a `remote_app` was set, enable RAIL mode —
        // the connector advertises RAIL + Window List caps and sets INFO_RAIL, and
        // the `rail` static channel launches the app (command line carried natively).
        // Only ever set for non-AVD targets; AVD uses RAIL over DVC + eGFX.
        config.rail = self.0.borrow().remote_app.clone();

        // RDP load-balance info / routing token. When set, it becomes the X.224
        // Connection Request routing token (`Cookie: msts=<value>\r\n`) instead of the
        // default `mstshash=<username>` cookie, so a broker/proxy can route the session.
        // IronRDP's `routing_token()` re-adds the `Cookie: msts=` prefix, so strip it
        // here to tolerate callers passing either the bare value or the full cookie form.
        let load_balance_info = self.0.borrow().load_balance_info.clone();
        if let Some(load_balance_info) = load_balance_info {
            let value = load_balance_info
                .strip_prefix("Cookie: msts=")
                .unwrap_or(&load_balance_info)
                .to_owned();
            config.request_data = Some(ironrdp::pdu::nego::NegoRequestData::routing_token(value));
        }

        let (input_events_tx, input_events_rx) = mpsc::unbounded();

        let clipboard = remote_clipboard_changed_callback.clone().map(|callback| {
            WasmClipboard::new(
                clipboard::WasmClipboardMessageProxy::new(input_events_tx.clone()),
                clipboard::JsClipboardCallbacks {
                    on_remote_clipboard_changed: callback,
                    on_force_clipboard_update: force_clipboard_update_callback,
                    on_files_available: files_available_callback,
                    on_file_contents_request: file_contents_request_callback,
                    on_file_contents_response: file_contents_response_callback,
                    on_lock: lock_callback,
                    on_unlock: unlock_callback,
                    on_locks_expired: locks_expired_callback,
                    on_format_list_response: format_list_response_callback,
                },
            )
        });

        if invalid_print_job_stream_callbacks {
            return Err(IronError::from(anyhow::anyhow!(
                "printer redirection requires valid print_job_stream_callbacks"
            )));
        }

        if invalid_sound_callbacks {
            return Err(IronError::from(anyhow::anyhow!(
                "audio playback requires valid sound_callbacks"
            )));
        }

        // Build the RDPSND audio pair when JS sound callbacks were registered.
        // Enabling audio also clears the NO_AUDIO_PLAYBACK client-info flag so the
        // server redirects sound to us instead of playing it on the host.
        let (sound_backend, sound) = match sound_callbacks {
            Some(callbacks) => {
                let (backend, sound) = wasm_sound_pair(input_events_tx.clone(), callbacks);
                config.enable_audio_playback = true;
                (Some(backend), Some(sound))
            }
            None => (None, None),
        };

        // Build the virtual-printer pair when JS printer callbacks were
        // registered via extension(). Backend is Send (holds the mpsc proxy
        // only) and goes into the SVC processor below; the front-end
        // `WasmPrinter` owns the JS callbacks and lives on `Session`.
        let (printer_backend, printer) = match print_job_stream_callbacks {
            Some(callbacks) => {
                let (backend, printer) = wasm_printer_pair(input_events_tx.clone(), callbacks);
                (Some(backend), Some(printer))
            }
            None => (None, None),
        };

        // Default to 2 to avoid a potential collision if drive redirection is
        // enabled in the same session.
        let printer_device_id = printer_device_id.unwrap_or(2);
        let printer_name = printer_name.unwrap_or_else(|| "IronRDP Virtual Printer".to_owned());
        let printer_driver_name = printer_driver_name.unwrap_or_else(default_printer_driver_name);

        let ws = WebSocket::open(&proxy_address).context("couldn't open WebSocket")?;

        // NOTE: ideally, when the WebSocket can't be opened, the above call should fail with details on why is that
        // (e.g., the proxy hostname could not be resolved, proxy service is not running), but errors are neved
        // bubbled up in practice, so instead we poll the WebSocket state until we know its connected (i.e., the
        // WebSocket handshake is a success and user data can be exchanged).
        loop {
            match ws.state() {
                websocket::State::Closing | websocket::State::Closed => {
                    return Err(IronError::from(anyhow::anyhow!(
                        "failed to connect to {proxy_address} (WebSocket is `{:?}`)",
                        ws.state()
                    ))
                    .with_kind(IronErrorKind::ProxyConnect));
                }
                websocket::State::Connecting => {
                    trace!("WebSocket is connecting to proxy at {proxy_address}...");
                    gloo_timers::future::sleep(Duration::from_millis(50)).await;
                }
                websocket::State::Open => {
                    debug!("WebSocket connected to {proxy_address} with success");
                    break;
                }
            }
        }

        let use_display_control = self.0.borrow().use_display_control;
        let advertise_avc = self.0.borrow().advertise_avc;

        // EGFX (MS-RDPEGFX) graphics pipeline is ENABLED (see `build_config`
        // support_graphics_pipeline = true). Attaching the handler makes the client
        // accept the server's `Microsoft::Windows::RDS::Graphics` DVC and decode the
        // full multi-codec eGFX stream (ClearCodec text/UI + RFX Progressive photo +
        // AVC/uncompressed) in the client core, compositing to the canvas. The pair
        // (flag + handler) must move together; both off falls back to bitmap/Surface-Bits.
        let graphics_handler = Some(WasmGraphicsHandler::new(WasmGraphicsMessageProxy::new(
            input_events_tx.clone(),
        )));

        let (connection_result, ws) = connect(ConnectParams {
            ws,
            config,
            proxy_auth_token: auth_token,
            destination,
            pcb,
            kdc_proxy_url,
            clipboard_backend: clipboard.as_ref().map(|clip| clip.backend()),
            printer_backend,
            printer_device_id,
            printer_name,
            printer_driver_name,
            sound_backend,
            computer_name: client_name.clone(),
            use_display_control,
            advertise_avc,
            graphics_handler,
        })
        .await?;

        info!("Connected!");

        let (rdp_reader, rdp_writer) = futures_util::AsyncReadExt::split(ws);

        let (writer_tx, writer_rx) = mpsc::unbounded();

        spawn_local(writer_task(writer_rx, rdp_writer, outbound_message_size_limit));

        Ok(Session {
            desktop_size: connection_result.desktop_size,
            input_database: RefCell::new(ironrdp::input::Database::new()),
            writer_tx,
            input_events_tx,

            render_canvas,
            set_cursor_style_callback,
            set_cursor_style_callback_context,
            avc_decode_callback,
            avc_watermark_callback,
            canvas_updated_callback,

            input_events_rx: RefCell::new(Some(input_events_rx)),
            rdp_reader: RefCell::new(Some(rdp_reader)),
            connection_result: RefCell::new(Some(connection_result)),
            clipboard: RefCell::new(Some(clipboard)),
            printer: RefCell::new(Some(printer)),
            sound: RefCell::new(Some(sound)),
        })
    }
}

pub(crate) type FastPathInputEvents = smallvec::SmallVec<[FastPathInputEvent; 2]>;

#[derive(Debug)]
pub(crate) enum RdpInputEvent {
    Cliprdr(ClipboardMessage),
    ClipboardBackend(WasmClipboardBackendMessage),
    /// Printer backend → event loop: a print job finished and its bytes are
    /// ready for delivery to JS. See [`crate::printer::PrinterBackendMessage`].
    Printer(crate::printer::PrinterBackendMessage),
    /// Sound backend → event loop: a chunk of PCM audio is ready for delivery to
    /// JS (Web Audio). See [`crate::sound::SoundBackendMessage`].
    Sound(crate::sound::SoundBackendMessage),
    FastPath(FastPathInputEvents),
    /// An EGFX-decoded output region, ready to blit to the canvas. Sent by
    /// [`crate::graphics::WasmGraphicsHandler`] (which is `Send` and cannot touch
    /// the `!Send` canvas) so the run loop can draw it. Also the return path for
    /// out-of-band AVC decode: JS hands back RGBA as one of these.
    Graphics(GraphicsRegion),
    /// A raw AVC (H.264) main sub-stream that must be decoded out-of-band by the
    /// browser (WebCodecs `VideoDecoder`). The graphics handler is `Send` and has no
    /// JS access, so it forwards the compressed frame here; the run loop hands it to
    /// the JS decode callback, which later returns RGBA via [`RdpInputEvent::AvcRegion`].
    Avc(AvcFrameEvent),
    /// An out-of-band-decoded AVC region (RGBA), returned by the JS WebCodecs decoder,
    /// with the eGFX `frame_id` its pixels belong to. Unlike [`RdpInputEvent::Graphics`],
    /// this bypassed the handler's surface buffer, so the run loop re-blends the watermark
    /// before drawing. This is the CPU-readback fallback's PIXEL-delivery path only; the
    /// FrameAcknowledge for `frame_id` was already sent at decode via [`RdpInputEvent::AvcAck`].
    AvcRegion(GraphicsRegion, u32),
    /// The WebCodecs decoder produced a frame for eGFX `frame_id`: send its deferred
    /// FrameAcknowledge. Fired at DECODE-completion (not present) so the server is paced to
    /// real decode throughput rather than a present round-trip — the fix for the
    /// two-monitor + AVC stutter. No drawing here; presentation happens independently on the
    /// JS side (GPU direct-draw or the CPU [`RdpInputEvent::AvcRegion`] pixel path).
    AvcAck(u32),
    /// The current session watermark, forwarded by the graphics handler so the run
    /// loop can re-blend it onto out-of-band AVC regions.
    Watermark(Watermark),
    Resize {
        width: u32,
        height: u32,
        scale_factor: Option<u32>,
        physical_size: Option<(u32, u32)>,
    },
    TerminateSession,
    /// The server marked a surface capture-protected (proxy `PROTECT_SURFACE`).
    /// A browser cannot enforce capture protection, so the session is refused
    /// fail-closed rather than shown unprotected. See [`PROTECTED_SESSION_REFUSAL`].
    ProtectedSessionRefused,
}

/// User-facing reason shown when a capture-protected session is refused in the
/// browser. Fail-closed: we never display protected content in a client that
/// cannot honor `SetWindowDisplayAffinity`-style screen-capture exclusion.
const PROTECTED_SESSION_REFUSAL: &str =
    "This session requires screen-capture protection, which isn't available in the browser — please use the native client.";

/// A decoded RGBA region positioned in output (desktop) coordinates.
#[derive(Debug)]
pub(crate) struct GraphicsRegion {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Row-major RGBA8888, `width * height * 4` bytes.
    pub(crate) data: Vec<u8>,
}

/// A raw AVC (H.264) main sub-stream awaiting out-of-band (WebCodecs) decode, with
/// its destination already resolved to output (desktop) coordinates.
#[derive(Debug)]
pub(crate) struct AvcFrameEvent {
    pub(crate) surface_id: u16,
    /// eGFX frame this picture belongs to; echoed back on present so the run loop can
    /// send the deferred FrameAcknowledge (flow control).
    pub(crate) frame_id: u32,
    /// Destination origin + size in output (desktop) coordinates.
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Main (`stream1`) H.264 bitstream (Annex B).
    pub(crate) main_stream: Vec<u8>,
}

pub(crate) struct SessionTerminationInfo {
    reason: GracefulDisconnectReason,
}

impl iron_remote_desktop::SessionTerminationInfo for SessionTerminationInfo {
    fn reason(&self) -> String {
        self.reason.to_string()
    }
}

pub(crate) struct Session {
    desktop_size: connector::DesktopSize,
    input_database: RefCell<ironrdp::input::Database>,
    writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    input_events_tx: mpsc::UnboundedSender<RdpInputEvent>,

    render_canvas: HtmlCanvasElement,
    set_cursor_style_callback: js_sys::Function,
    set_cursor_style_callback_context: JsValue,
    /// WebCodecs AVC decode callback; `None` if no browser decoder was registered.
    avc_decode_callback: Option<js_sys::Function>,
    /// AVC watermark callback; forwards the session watermark to the GPU draw path.
    avc_watermark_callback: Option<js_sys::Function>,
    /// Passive render-canvas update notification; `None` if no presenter registered.
    canvas_updated_callback: Option<js_sys::Function>,

    // Consumed when `run` is called
    input_events_rx: RefCell<Option<mpsc::UnboundedReceiver<RdpInputEvent>>>,
    connection_result: RefCell<Option<connector::ConnectionResult>>,
    rdp_reader: RefCell<Option<ReadHalf<WebSocket>>>,
    clipboard: RefCell<Option<Option<WasmClipboard>>>,
    printer: RefCell<Option<Option<WasmPrinter>>>,
    sound: RefCell<Option<Option<WasmSound>>>,
}

impl Session {
    fn h_send_inputs(&self, inputs: smallvec::SmallVec<[FastPathInputEvent; 2]>) -> Result<(), IronError> {
        if !inputs.is_empty() {
            trace!("Inputs: {inputs:?}");

            self.input_events_tx
                .unbounded_send(RdpInputEvent::FastPath(inputs))
                .context("Send input events to writer task")?;
        }

        Ok(())
    }

    fn set_cursor_style(&self, style: CursorStyle) -> Result<(), IronError> {
        let (kind, data, hotspot_x, hotspot_y) = match style {
            CursorStyle::Default => ("default", None, None, None),
            CursorStyle::Hidden => ("hidden", None, None, None),
            CursorStyle::Url {
                data,
                hotspot_x,
                hotspot_y,
            } => ("url", Some(data), Some(hotspot_x), Some(hotspot_y)),
        };

        let args = js_sys::Array::from_iter([
            JsValue::from_str(kind),
            JsValue::from(data),
            JsValue::from_f64(hotspot_x.unwrap_or_default().into()),
            JsValue::from_f64(hotspot_y.unwrap_or_default().into()),
        ]);

        let _ret = self
            .set_cursor_style_callback
            .apply(&self.set_cursor_style_callback_context, &args)
            .map_err(|e| anyhow::Error::msg(format!("set cursor style callback failed: {e:?}")))?;

        Ok(())
    }

    /// Passive notification that the render canvas changed in `(x, y, width, height)`
    /// (source-canvas pixel coords), used by the multi-monitor presenter to redraw only
    /// the affected area on a real pixel change. Best-effort: a throwing callback is
    /// logged and ignored, and it never influences protocol / frame-ack state.
    fn notify_canvas_updated(&self, x: u32, y: u32, width: u32, height: u32) {
        if let Some(cb) = &self.canvas_updated_callback {
            let args = js_sys::Array::from_iter([
                JsValue::from_f64(f64::from(x)),
                JsValue::from_f64(f64::from(y)),
                JsValue::from_f64(f64::from(width)),
                JsValue::from_f64(f64::from(height)),
            ]);
            if let Err(err) = cb.apply(&JsValue::NULL, &args) {
                warn!(?err, "canvas_updated callback threw");
            }
        }
    }
}

impl iron_remote_desktop::Session for Session {
    type SessionTerminationInfo = SessionTerminationInfo;
    type InputTransaction = InputTransaction;
    type ClipboardData = ClipboardData;
    type Error = IronError;

    async fn run(&self) -> Result<Self::SessionTerminationInfo, Self::Error> {
        let rdp_reader = self
            .rdp_reader
            .borrow_mut()
            .take()
            .context("RDP session can be started only once")?;

        let mut input_events = self
            .input_events_rx
            .borrow_mut()
            .take()
            .context("RDP session can be started only once")?;

        let connection_result = self
            .connection_result
            .borrow_mut()
            .take()
            .expect("run called only once");

        let mut clipboard = self.clipboard.borrow_mut().take().expect("run called only once");
        let mut wasm_printer = self.printer.borrow_mut().take().expect("run called only once");
        let wasm_sound = self.sound.borrow_mut().take().expect("run called only once");

        let mut framed = ironrdp_futures::LocalFuturesFramed::new(rdp_reader);

        debug!("Initialize canvas");

        let desktop_width =
            NonZeroU32::new(u32::from(connection_result.desktop_size.width)).context("desktop width is zero")?;
        let desktop_height =
            NonZeroU32::new(u32::from(connection_result.desktop_size.height)).context("desktop height is zero")?;

        let mut gui =
            Canvas::new(self.render_canvas.clone(), desktop_width, desktop_height).context("canvas initialization")?;

        debug!("Canvas initialized");

        info!("Start RDP session");

        let mut image = DecodedImage::new(
            PixelFormat::RgbA32,
            connection_result.desktop_size.width,
            connection_result.desktop_size.height,
        );

        let mut requested_resize = None;

        // Reused across frames so per-region extraction doesn't allocate on every draw.
        let mut draw_buffer = WriteBuf::new();

        // Latest session watermark, forwarded by the graphics handler. Re-blended onto
        // out-of-band AVC regions (which bypass the handler's flush-time re-blend).
        let mut current_watermark: Option<Watermark> = None;

        // Full-desktop rectangle used for the post-connect Refresh Rect (see below).
        // `desktop_size` is Copy, so read it before the builder consumes the rest.
        let desktop_refresh_rect = ironrdp::pdu::geometry::InclusiveRectangle {
            left: 0,
            top: 0,
            right: connection_result.desktop_size.width.saturating_sub(1),
            bottom: connection_result.desktop_size.height.saturating_sub(1),
        };

        // We retain the factory to drive the Deactivation-Reactivation Sequence locally.
        let activation_factory = connection_result.activation_factory;

        let mut active_stage = ActiveStageBuilder {
            static_channels: connection_result.static_channels,
            user_channel_id: connection_result.user_channel_id,
            io_channel_id: connection_result.io_channel_id,
            message_channel_id: connection_result.message_channel_id,
            share_id: connection_result.share_id,
            compression_type: connection_result.compression_type,
            enable_server_pointer: connection_result.enable_server_pointer,
            pointer_software_rendering: connection_result.pointer_software_rendering,
        }
        .build();

        // Timer interval for driving clipboard lock timeouts (5 second interval)
        let mut cleanup_interval = IntervalStream::new(5_000).fuse();

        // On (re)connect to a persistent RDP session, the server composes updates
        // assuming client-side cached/persisted content for regions it does not
        // explicitly repaint. A fresh web client lacks that state, so those regions
        // render as stale/coarse "gray blocks" (e.g. wallpaper covered by a transient
        // element and never restored, or a tile left at its coarse quality pass). We
        // ask the server to redraw the whole screen a few times over the first seconds
        // (TS_REFRESH_RECT_PDU); the server re-encodes the true current screen at full
        // quality, clearing the stale regions. `areas_to_refresh` is the full desktop.
        let mut refresh_interval = IntervalStream::new(2_500).fuse();
        let mut refresh_rect_fired: u32 = 0;

        let disconnect_reason = 'outer: loop {
            let outputs = select! {
                frame = framed.read_pdu().fuse() => {
                    let (action, payload) = frame.context("read frame")?;
                    trace!(?action, frame_length = payload.len(), "Frame received");

                    active_stage.process(&mut image, action, &payload)?
                }
                input_events = input_events.next() => {
                    let event = input_events.context("read next input events")?;

                    match event {
                        RdpInputEvent::Cliprdr(message) => {
                            if let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() {
                                if let Some(svc_messages) = match message {
                                    ClipboardMessage::SendInitiateCopy(formats) => Some(
                                        cliprdr.initiate_copy(&formats)
                                            .context("cliprdr initiate copy")?
                                    ),
                                    ClipboardMessage::SendInitiateFileCopy(files) => Some(
                                        cliprdr.initiate_file_copy(files)
                                            .context("cliprdr initiate file copy")?
                                    ),
                                    ClipboardMessage::SendFormatData(response) => Some(
                                        cliprdr.submit_format_data(response)
                                            .context("cliprdr submit format data")?
                                    ),
                                    ClipboardMessage::SendInitiatePaste(format) => Some(
                                        cliprdr.initiate_paste(format)
                                            .context("cliprdr initiate paste")?
                                    ),
                                    ClipboardMessage::SendFileContentsRequest(request) => Some(
                                        cliprdr.request_file_contents(request)
                                            .context("cliprdr request file contents")?
                                    ),
                                    ClipboardMessage::SendFileContentsResponse(response) => Some(
                                        cliprdr.submit_file_contents(response)
                                            .context("cliprdr submit file contents")?
                                    ),
                                    ClipboardMessage::Error(e) => {
                                        error!(error = %e, "Clipboard backend error");
                                        None
                                    }
                                } {
                                    let frame = active_stage.process_svc_processor_messages(svc_messages)?;
                                    // Send the messages to the server
                                    vec![ActiveStageOutput::ResponseFrame(frame)]
                                } else {
                                    // No messages to send to the server
                                    Vec::new()
                                }
                            } else  {
                                warn!("Clipboard event received, but Cliprdr is not available");
                                Vec::new()
                            }
                        }
                        RdpInputEvent::ClipboardBackend(event) => {
                            use crate::clipboard::WasmClipboardBackendMessage;

                            // Handle messages that need direct cliprdr access
                            match event {
                                WasmClipboardBackendMessage::FileContentsRequestSend { stream_id, index, flags, position, size, clip_data_id } => {
                                    if let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() {
                                        let request = FileContentsRequest {
                                            stream_id,
                                            index,
                                            flags,
                                            position,
                                            requested_size: size,
                                            data_id: clip_data_id,
                                        };
                                        match cliprdr.request_file_contents(request) {
                                            Ok(svc_messages) => {
                                                let frame = active_stage.process_svc_processor_messages(svc_messages)?;
                                                vec![ActiveStageOutput::ResponseFrame(frame)]
                                            }
                                            Err(e) => {
                                                error!(error = %e, "File contents request failed");
                                                Vec::new()
                                            }
                                        }
                                    } else {
                                        warn!("Request file contents received, but Cliprdr is not available");
                                        Vec::new()
                                    }
                                }
                                WasmClipboardBackendMessage::FileContentsResponseSend { stream_id, is_error, data } => {
                                    if let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() {
                                        let response = if is_error {
                                            FileContentsResponse::new_error(stream_id)
                                        } else {
                                            FileContentsResponse::new_data_response(stream_id, data)
                                        };
                                        match cliprdr.submit_file_contents(response) {
                                            Ok(svc_messages) => {
                                                let frame = active_stage.process_svc_processor_messages(svc_messages)?;
                                                vec![ActiveStageOutput::ResponseFrame(frame)]
                                            }
                                            Err(e) => {
                                                error!(error = %e, "File contents submit failed");
                                                Vec::new()
                                            }
                                        }
                                    } else {
                                        warn!("Submit file contents received, but Cliprdr is not available");
                                        Vec::new()
                                    }
                                }
                                WasmClipboardBackendMessage::InitiateFileCopy { files } => {
                                    if let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() {
                                        // Convert FileMetadata to FileDescriptor using the
                                        // validated conversion that checks name length/emptiness
                                        // and sets proper file attributes.
                                        let file_descriptors: Vec<FileDescriptor> = files
                                            .into_iter()
                                            .filter_map(|f| match f.to_file_descriptor() {
                                                Ok(desc) => Some(desc),
                                                Err(e) => {
                                                    warn!(error = format!("{e:#}"), "Skipping file with invalid metadata");
                                                    None
                                                }
                                            })
                                            .collect();

                                        match cliprdr.initiate_file_copy(file_descriptors) {
                                            Ok(svc_messages) => {
                                                let frame = active_stage.process_svc_processor_messages(svc_messages)?;
                                                vec![ActiveStageOutput::ResponseFrame(frame)]
                                            }
                                            Err(e) => {
                                                error!(error = %e, "Initiate file copy failed");
                                                Vec::new()
                                            }
                                        }
                                    } else {
                                        warn!("Initiate file copy received, but Cliprdr is not available");
                                        Vec::new()
                                    }
                                }
                                // All other messages are forwarded to clipboard backend
                                other => {
                                    if let Some(clipboard) = &mut clipboard {
                                        clipboard.process_event(other)?;
                                    }
                                    Vec::new()
                                }
                            }
                        }
                        RdpInputEvent::FastPath(events) => {
                            active_stage.process_fastpath_input(&mut image, &events)
                                .context("fast path input events processing")?
                        }
                        RdpInputEvent::Graphics(region) => {
                            // EGFX-decoded region → blit straight to the canvas. The
                            // handler already composited into its surface buffers, so
                            // this is a direct paint (no ActiveStage involvement).
                            let (rx, ry, rw, rh) = (region.x, region.y, region.width, region.height);
                            let right = rx.saturating_add(rw).saturating_sub(1);
                            let bottom = ry.saturating_add(rh).saturating_sub(1);
                            let rect = ironrdp::pdu::geometry::InclusiveRectangle {
                                left: rx.min(u32::from(u16::MAX)) as u16,
                                top: ry.min(u32::from(u16::MAX)) as u16,
                                right: right.min(u32::from(u16::MAX)) as u16,
                                bottom: bottom.min(u32::from(u16::MAX)) as u16,
                            };
                            let mut data = region.data;
                            if let Err(e) = gui.draw(&mut data, rect) {
                                warn!(error = format!("{e:#}"), "failed to draw EGFX region");
                            }
                            // Passive: tell an external presenter which area changed.
                            self.notify_canvas_updated(rx, ry, rw, rh);
                            Vec::new()
                        }
                        RdpInputEvent::Watermark(wm) => {
                            // Forward the tile to the GPU AVC draw path so JS can overdraw
                            // it on each frame (the CPU fallback re-blends `current_watermark`
                            // in Rust instead).
                            if let Some(cb) = &self.avc_watermark_callback {
                                let rgba = js_sys::Uint8Array::from(wm.rgba.as_slice());
                                let args = js_sys::Array::from_iter([
                                    rgba.into(),
                                    JsValue::from_f64(f64::from(wm.width)),
                                    JsValue::from_f64(f64::from(wm.height)),
                                    JsValue::from_f64(f64::from(wm.cell_w)),
                                    JsValue::from_f64(f64::from(wm.cell_h)),
                                    JsValue::from_f64(f64::from(wm.off_x)),
                                    JsValue::from_f64(f64::from(wm.off_y)),
                                    JsValue::from_f64(f64::from(wm.opacity)),
                                ]);
                                if let Err(err) = cb.apply(&JsValue::NULL, &args) {
                                    warn!(?err, "AVC watermark callback threw");
                                }
                            }
                            // Retain the current watermark for re-blending onto CPU AVC regions.
                            current_watermark = Some(wm);
                            Vec::new()
                        }
                        RdpInputEvent::AvcRegion(region, frame_id) => {
                            // CPU-readback fallback PIXEL-delivery path: JS read the decoded
                            // frame back to RGBA and handed us the pixels to blit. Out-of-band
                            // AVC decode bypassed the handler's surface buffer (and thus its
                            // flush-time watermark re-blend), so re-blend the mark here before
                            // painting, then draw exactly like a normal Graphics region.
                            //
                            // The FrameAcknowledge for `frame_id` was already sent at DECODE
                            // (RdpInputEvent::AvcAck), so we do NOT ack here — flow control is
                            // paced by decode throughput, not this present. (`frame_id` is
                            // retained only for symmetry / potential future use.)
                            let _ = frame_id;
                            let (rx, ry, rw, rh) = (region.x, region.y, region.width, region.height);
                            let mut data = region.data;
                            if let Some(wm) = &current_watermark {
                                blend_watermark_into(wm, &mut data, rx, ry, rw, rh);
                            }
                            let right = rx.saturating_add(rw).saturating_sub(1);
                            let bottom = ry.saturating_add(rh).saturating_sub(1);
                            let rect = ironrdp::pdu::geometry::InclusiveRectangle {
                                left: rx.min(u32::from(u16::MAX)) as u16,
                                top: ry.min(u32::from(u16::MAX)) as u16,
                                right: right.min(u32::from(u16::MAX)) as u16,
                                bottom: bottom.min(u32::from(u16::MAX)) as u16,
                            };
                            if let Err(e) = gui.draw(&mut data, rect) {
                                warn!(error = format!("{e:#}"), "failed to draw AVC region");
                            }
                            // Passive: the CPU AVC readback path updated the canvas here
                            // (the GPU direct-draw path notifies JS-side from AvcDecoder).
                            self.notify_canvas_updated(rx, ry, rw, rh);
                            Vec::new()
                        }
                        RdpInputEvent::AvcAck(frame_id) => {
                            // The WebCodecs decoder produced a frame for `frame_id`. Send its
                            // deferred FrameAcknowledge NOW (at decode-completion) so the server
                            // is paced to our real decode throughput instead of a present
                            // round-trip. Presentation (GPU direct-draw or the CPU AvcRegion
                            // pixel path) happens independently; the bounded per-surface present
                            // FIFO in JS caps display latency so early acking can't build an
                            // unbounded backlog.
                            match encode_avc_frame_ack(&mut active_stage, frame_id) {
                                Ok(Some(frame)) => vec![ActiveStageOutput::ResponseFrame(frame)],
                                Ok(None) => Vec::new(),
                                Err(e) => {
                                    warn!(error = format!("{e:#}"), "failed to send AVC frame-ack");
                                    Vec::new()
                                }
                            }
                        }
                        RdpInputEvent::Avc(frame) => {
                            // Hand the compressed AVC main sub-stream to the browser
                            // WebCodecs decoder. It decodes asynchronously and returns
                            // RGBA via the `on_avc_decoded` extension, which re-enters
                            // the loop as an `RdpInputEvent::AvcRegion`.
                            if let Some(cb) = &self.avc_decode_callback {
                                let data = js_sys::Uint8Array::from(frame.main_stream.as_slice());
                                let args = js_sys::Array::from_iter([
                                    JsValue::from_f64(f64::from(frame.surface_id)),
                                    JsValue::from_f64(f64::from(frame.frame_id)),
                                    JsValue::from_f64(f64::from(frame.x)),
                                    JsValue::from_f64(f64::from(frame.y)),
                                    JsValue::from_f64(f64::from(frame.width)),
                                    JsValue::from_f64(f64::from(frame.height)),
                                    data.into(),
                                ]);
                                if let Err(err) = cb.apply(&JsValue::NULL, &args) {
                                    warn!(?err, "AVC decode callback threw");
                                }
                            } else {
                                trace!(
                                    bytes = frame.main_stream.len(),
                                    "AVC frame dropped: no WebCodecs decoder registered"
                                );
                            }
                            Vec::new()
                        }
                        RdpInputEvent::Resize { width, height, scale_factor, physical_size } => {
                            if width == 0 || height == 0 {
                                warn!("Resize event ignored: width or height is zero");
                                Vec::new()
                            } else if let Some(response_frame) = active_stage.encode_resize(width, height, scale_factor, physical_size) {
                                let width = NonZeroU32::new(width).expect("width is guaranteed to be non-zero due to the prior check");
                                let height = NonZeroU32::new(height).expect("height is guaranteed to be non-zero due to the prior check");
                                requested_resize = Some((width, height));
                                vec![ActiveStageOutput::ResponseFrame(response_frame?)]
                            } else {
                                Vec::new()
                            }
                        },
                        RdpInputEvent::Printer(message) => {
                            // The printer backend lives inside the Rdpdr SVC
                            // processor (Send-only); the front-end
                            // `WasmPrinter` owns the JS callback (!Send) and
                            // lives here. Just forward the message.
                            if let Some(ref mut wasm_printer) = wasm_printer {
                                wasm_printer.process_message(message);
                            } else {
                                warn!("Printer event received, but no printer is configured");
                            }
                            Vec::new()
                        }
                        RdpInputEvent::Sound(message) => {
                            // Like the printer, the RDPSND backend is Send-only and
                            // lives in the SVC processor; `WasmSound` owns the JS
                            // callback (!Send) and lives here. Forward the PCM chunk.
                            if let Some(ref wasm_sound) = wasm_sound {
                                wasm_sound.process_message(message);
                            } else {
                                warn!("Sound event received, but no audio backend is configured");
                            }
                            Vec::new()
                        }
                        RdpInputEvent::TerminateSession => {
                            active_stage.graceful_shutdown()
                                .context("graceful shutdown")?
                        }
                        RdpInputEvent::ProtectedSessionRefused => {
                            info!("Refusing capture-protected session (not enforceable in browser)");
                            vec![ActiveStageOutput::Terminate(GracefulDisconnectReason::Other(
                                PROTECTED_SESSION_REFUSAL.to_owned(),
                            ))]
                        }
                    }
                }
                _ = cleanup_interval.next() => {
                    // Drive clipboard lock timeout cleanup
                    if let Some(cliprdr) = active_stage.get_svc_processor_mut::<CliprdrClient>() {
                        match cliprdr.drive_timeouts() {
                            Ok(svc_messages) => {
                                let frame = active_stage.process_svc_processor_messages(svc_messages)?;
                                if !frame.is_empty() {
                                    vec![ActiveStageOutput::ResponseFrame(frame)]
                                } else {
                                    Vec::new()
                                }
                            }
                            Err(e) => {
                                warn!(error = %e, "Clipboard timeout cleanup failed");
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    }
                }
                _ = refresh_interval.next() => {
                    // Force a full-screen repaint for the first few ticks after connect
                    // to clear stale/coarse regions the server assumed were cached.
                    if refresh_rect_fired < 3 {
                        refresh_rect_fired += 1;
                        let mut frame = WriteBuf::new();
                        match active_stage.encode_static(
                            &mut frame,
                            ironrdp::pdu::rdp::headers::ShareDataPdu::RefreshRectangle(
                                ironrdp::pdu::rdp::refresh_rectangle::RefreshRectanglePdu {
                                    areas_to_refresh: vec![desktop_refresh_rect.clone()],
                                },
                            ),
                        ) {
                            Ok(_) => {
                                debug!(fired = refresh_rect_fired, "Sent full-screen Refresh Rect");
                                vec![ActiveStageOutput::ResponseFrame(frame.into_inner())]
                            }
                            Err(e) => {
                                warn!(error = %e, "Failed to encode Refresh Rect");
                                Vec::new()
                            }
                        }
                    } else {
                        Vec::new()
                    }
                }
            };

            // RAIL Window List orders: the fast-path processor buffered any
            // drawing-order updates from this frame. Decode the window lifecycle
            // and surface it (the window *content* renders via the normal
            // GraphicsUpdate path below). Full local-window compositing is a
            // frontend concern; here we make the decoded state observable.
            for orders in active_stage.take_rail_orders() {
                for order in WindowOrder::decode_orders_update(&orders) {
                    log_rail_window_order(&order);
                }
            }

            for out in outputs {
                match out {
                    ActiveStageOutput::ResponseFrame(frame) => {
                        self.writer_tx
                            .unbounded_send(frame)
                            .context("Send frame to writer task")?;
                    }
                    ActiveStageOutput::GraphicsUpdate(region) => {
                        let region = extract_partial_image(&image, region, &mut draw_buffer);
                        // Capture the updated rect before `region` is moved into draw().
                        let (ux, uy, uw, uh) = (
                            u32::from(region.left),
                            u32::from(region.top),
                            u32::from(region.right.saturating_sub(region.left)) + 1,
                            u32::from(region.bottom.saturating_sub(region.top)) + 1,
                        );
                        gui.draw(draw_buffer.filled_mut(), region)
                            .context("draw updated region")?;
                        draw_buffer.clear();
                        // Passive: classic bitmap/Surface-Bits path updated the canvas.
                        self.notify_canvas_updated(ux, uy, uw, uh);
                    }
                    ActiveStageOutput::PointerDefault => {
                        self.set_cursor_style(CursorStyle::Default)?;
                    }
                    ActiveStageOutput::PointerHidden => {
                        self.set_cursor_style(CursorStyle::Hidden)?;
                    }
                    ActiveStageOutput::PointerPosition { .. } => {
                        // Not applicable for web.
                    }
                    ActiveStageOutput::PointerBitmap(pointer) => {
                        // Maximum allowed cursor size for browsers is 32x32, because bigger sizes
                        // will cause the following issues:
                        // - cursors bigger than 128x128 are not supported in browsers.
                        // - cursors bigger than 32x32 will default to the system cursor if their
                        //   sprite does not fit in the browser's viewport, introducing an abrupt
                        //   cursor style change when the cursor is moved to the edge of the
                        //   browser window.
                        //
                        // Therefore, we need to scale the cursor sprite down to 32x32 if it is
                        // bigger than that.
                        const MAX_CURSOR_SIZE: u16 = 32;
                        // INVARIANT: 0 < scale <= 1.0
                        // INVARIANT: pointer.width * scale <= MAX_CURSOR_SIZE
                        // INVARIANT: pointer.height * scale <= MAX_CURSOR_SIZE
                        let scale = if pointer.width >= pointer.height && pointer.width > MAX_CURSOR_SIZE {
                            Some(f64::from(MAX_CURSOR_SIZE) / f64::from(pointer.width))
                        } else if pointer.height > MAX_CURSOR_SIZE {
                            Some(f64::from(MAX_CURSOR_SIZE) / f64::from(pointer.height))
                        } else {
                            None
                        };

                        let (png_width, png_height, hotspot_x, hotspot_y, rgba_buffer) = if let Some(scale) = scale {
                            // Per invariants: Following conversions will never saturate.
                            let scaled_width = f64_to_u16_saturating_cast(f64::from(pointer.width) * scale);
                            let scaled_height = f64_to_u16_saturating_cast(f64::from(pointer.height) * scale);
                            let hotspot_x = f64_to_u16_saturating_cast(f64::from(pointer.hotspot_x) * scale);
                            let hotspot_y = f64_to_u16_saturating_cast(f64::from(pointer.hotspot_y) * scale);

                            // Per invariants: scaled_width * scaled_height * 4 <= 32 * 32 * 4 < usize::MAX
                            #[expect(clippy::arithmetic_side_effects)]
                            let resized_rgba_buffer_size = usize::from(scaled_width * scaled_height * 4);

                            let mut rgba_resized = vec![0u8; resized_rgba_buffer_size];
                            let mut resizer = resize::new(
                                usize::from(pointer.width),
                                usize::from(pointer.height),
                                usize::from(scaled_width),
                                usize::from(scaled_height),
                                resize::Pixel::RGBA8P,
                                resize::Type::Lanczos3,
                            )
                            .context("failed to initialize cursor resizer")?;

                            resizer
                                .resize(pointer.bitmap_data.as_pixels(), rgba_resized.as_pixels_mut())
                                .context("failed to resize cursor")?;

                            (
                                scaled_width,
                                scaled_height,
                                hotspot_x,
                                hotspot_y,
                                Cow::Owned(rgba_resized),
                            )
                        } else {
                            (
                                pointer.width,
                                pointer.height,
                                pointer.hotspot_x,
                                pointer.hotspot_y,
                                Cow::Borrowed(pointer.bitmap_data.as_slice()),
                            )
                        };

                        // Encode PNG.
                        let mut png_buffer = Vec::new();
                        {
                            let mut encoder =
                                png::Encoder::new(&mut png_buffer, u32::from(png_width), u32::from(png_height));

                            encoder.set_color(png::ColorType::Rgba);
                            encoder.set_depth(png::BitDepth::Eight);
                            encoder.set_compression(png::Compression::Fast);
                            let mut writer = encoder.write_header().context("PNG encoder header write failed")?;
                            writer
                                .write_image_data(&rgba_buffer)
                                .context("failed to encode pointer PNG")?;
                        }

                        // Encode PNG into Base64 data URL.
                        let mut style = "data:image/png;base64,".to_owned();
                        base64::engine::general_purpose::STANDARD.encode_string(png_buffer, &mut style);

                        self.set_cursor_style(CursorStyle::Url {
                            data: style,
                            hotspot_x,
                            hotspot_y,
                        })?;
                    }
                    ActiveStageOutput::DeactivateAll => {
                        // Execute the Deactivation-Reactivation Sequence:
                        // https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/dfc234ce-481a-4674-9a5d-2a7bafb14432
                        debug!("Received Server Deactivate All PDU, executing Deactivation-Reactivation Sequence");

                        // We need to perform resize after receiving the Deactivate All PDU, because there may be frames
                        // with the previous dimensions arriving between the resize request and this message.
                        if let Some((width, height)) = requested_resize {
                            gui.resize(width, height);
                            requested_resize = None;
                        }

                        let mut connection_activation = activation_factory.create();
                        let mut buf = WriteBuf::new();
                        'activation_seq: loop {
                            let written =
                                single_sequence_step_read(&mut framed, &mut connection_activation, &mut buf).await?;

                            if written.size().is_some() {
                                self.writer_tx
                                    .unbounded_send(buf.filled().to_vec())
                                    .context("Send frame to writer task")?;
                            }

                            if let ConnectionActivationState::Finalized {
                                desktop_size,
                                share_id,
                                input_flags: _,
                                enable_server_pointer,
                                pointer_software_rendering,
                                ..
                            } = connection_activation.connection_activation_state()
                            {
                                debug!("Deactivation-Reactivation Sequence completed");
                                image = DecodedImage::new(PixelFormat::RgbA32, desktop_size.width, desktop_size.height);
                                active_stage.reactivate(
                                    connection_activation.io_channel_id(),
                                    connection_activation.user_channel_id(),
                                    share_id,
                                    enable_server_pointer,
                                    pointer_software_rendering,
                                );
                                break 'activation_seq;
                            }
                        }
                    }
                    ActiveStageOutput::MultitransportRequest(pdu) => {
                        debug!(
                            request_id = pdu.request_id,
                            requested_protocol = ?pdu.requested_protocol,
                            "Multitransport request received (UDP transport not implemented)"
                        );
                    }
                    ActiveStageOutput::AutoDetect(request) => {
                        debug!(?request, "Auto-detect");
                    }
                    ActiveStageOutput::AutoReconnectCookie(_) => {
                        debug!("Server Auto-Reconnect Cookie received (automatic reconnection not implemented)");
                    }
                    ActiveStageOutput::SaveSessionInfo { logon_complete: true } => {
                        debug!("RDP login complete");
                    }
                    ActiveStageOutput::SaveSessionInfo { logon_complete: false } => {
                        debug!("RDP session info notification");
                    }
                    ActiveStageOutput::Terminate(reason) => break 'outer reason,
                }
            }
        };

        info!(%disconnect_reason, "RDP session terminated");

        Ok(SessionTerminationInfo {
            reason: disconnect_reason,
        })
    }

    fn desktop_size(&self) -> DesktopSize {
        DesktopSize {
            width: self.desktop_size.width,
            height: self.desktop_size.height,
        }
    }

    fn apply_inputs(&self, transaction: Self::InputTransaction) -> Result<(), Self::Error> {
        let inputs = self.input_database.borrow_mut().apply(transaction);
        self.h_send_inputs(inputs)
    }

    fn release_all_inputs(&self) -> Result<(), Self::Error> {
        let inputs = self.input_database.borrow_mut().release_all();
        self.h_send_inputs(inputs)
    }

    fn synchronize_lock_keys(
        &self,
        scroll_lock: bool,
        num_lock: bool,
        caps_lock: bool,
        kana_lock: bool,
    ) -> Result<(), Self::Error> {
        use ironrdp::pdu::input::fast_path::FastPathInput;

        let event = ironrdp::input::synchronize_event(scroll_lock, num_lock, caps_lock, kana_lock);
        let fastpath_input = FastPathInput::single(event);

        let frame = ironrdp::core::encode_vec(&fastpath_input).context("FastPathInput encoding")?;

        self.writer_tx
            .unbounded_send(frame)
            .context("Send frame to writer task")?;

        Ok(())
    }

    fn shutdown(&self) -> Result<(), Self::Error> {
        self.input_events_tx
            .unbounded_send(RdpInputEvent::TerminateSession)
            .context("failed to send terminate session event to writer task")?;

        Ok(())
    }

    async fn on_clipboard_paste(&self, content: &Self::ClipboardData) -> Result<(), Self::Error> {
        self.input_events_tx
            .unbounded_send(RdpInputEvent::ClipboardBackend(
                WasmClipboardBackendMessage::LocalClipboardChanged(content.clone()),
            ))
            .context("send clipboard backend event")?;

        Ok(())
    }

    fn resize(
        &self,
        width: u32,
        height: u32,
        scale_factor: Option<u32>,
        physical_width: Option<u32>,
        physical_height: Option<u32>,
    ) {
        if self
            .input_events_tx
            .unbounded_send(RdpInputEvent::Resize {
                width,
                height,
                scale_factor,
                physical_size: physical_width.and_then(|width| physical_height.map(|height| (width, height))),
            })
            .is_err()
        {
            warn!("Failed to send resize event, receiver is closed");
        }
    }

    fn supports_unicode_keyboard_shortcuts(&self) -> bool {
        // RDP does not support Unicode keyboard shortcuts.
        // When key combinations are executed, only plain scancode events are allowed to function correctly.
        false
    }

    fn invoke_extension(&self, ext: Extension) -> Result<JsValue, Self::Error> {
        // File transfer operations are protocol-specific (RDPECLIP) and routed
        // through invoke_extension rather than dedicated Session trait methods
        // to keep the iron-remote-desktop trait surface protocol-agnostic.
        iron_remote_desktop::extension_match! {
            match ext;
            |request_file_contents: JsValue| {
                let obj = into_object(request_file_contents)?;
                let stream_id = get_u32(&obj, "stream_id")?;
                let file_index = get_i32(&obj, "file_index")?;
                let flags = get_u32(&obj, "flags")?;
                let position = get_u64(&obj, "position")?;
                let size = get_u32(&obj, "size")?;
                let clip_data_id = get_u32_opt(&obj, "clip_data_id")?;

                self.input_events_tx
                    .unbounded_send(RdpInputEvent::ClipboardBackend(
                        WasmClipboardBackendMessage::FileContentsRequestSend {
                            stream_id,
                            index: file_index,
                            flags: FileContentsFlags::from_bits_truncate(flags),
                            position,
                            size,
                            clip_data_id,
                        },
                    ))
                    .context("send file contents request")
                    .map_err(IronError::from)?;

                return Ok(JsValue::NULL);
            };
            |submit_file_contents: JsValue| {
                let obj = into_object(submit_file_contents)?;
                let stream_id = get_u32(&obj, "stream_id")?;
                let is_error = get_bool(&obj, "is_error")?;
                let data_val = js_sys::Reflect::get(&obj, &JsValue::from_str("data"))
                    .map_err(|e| IronError::from(anyhow::anyhow!("get property `data`: {e:?}")))?;
                let data = js_sys::Uint8Array::new(&data_val).to_vec();

                self.input_events_tx
                    .unbounded_send(RdpInputEvent::ClipboardBackend(
                        WasmClipboardBackendMessage::FileContentsResponseSend {
                            stream_id,
                            is_error,
                            data,
                        },
                    ))
                    .context("send file contents response")
                    .map_err(IronError::from)?;

                return Ok(JsValue::NULL);
            };
            |initiate_file_copy: JsValue| {
                let file_list = parse_file_metadata_array(initiate_file_copy)?;

                self.input_events_tx
                    .unbounded_send(RdpInputEvent::ClipboardBackend(
                        WasmClipboardBackendMessage::InitiateFileCopy { files: file_list },
                    ))
                    .context("send initiate file copy")
                    .map_err(IronError::from)?;

                return Ok(JsValue::NULL);
            };
            // Return path for out-of-band AVC decode: JS hands back RGBA for a region
            // it decoded via WebCodecs. Re-enters the loop as a Graphics region and is
            // blitted to the canvas by the existing `RdpInputEvent::Graphics` arm.
            |on_avc_decoded: JsValue| {
                let obj = into_object(on_avc_decoded)?;
                let frame_id = get_u32(&obj, "frameId")?;
                let x = get_u32(&obj, "x")?;
                let y = get_u32(&obj, "y")?;
                let width = get_u32(&obj, "width")?;
                let height = get_u32(&obj, "height")?;
                let data_val = js_sys::Reflect::get(&obj, &JsValue::from_str("data"))
                    .map_err(|e| IronError::from(anyhow::anyhow!("get property `data`: {e:?}")))?;
                let data = js_sys::Uint8Array::new(&data_val).to_vec();

                self.input_events_tx
                    .unbounded_send(RdpInputEvent::AvcRegion(GraphicsRegion { x, y, width, height, data }, frame_id))
                    .context("send AVC-decoded region")
                    .map_err(IronError::from)?;

                return Ok(JsValue::NULL);
            };
            // Decode-complete ack signal: the WebCodecs decoder produced a frame, so send
            // the deferred FrameAcknowledge for its eGFX frame_id. Fired at DECODE (not
            // present) so the server is paced to real decode throughput.
            |on_avc_ack: JsValue| {
                let obj = into_object(on_avc_ack)?;
                let frame_id = get_u32(&obj, "frameId")?;

                self.input_events_tx
                    .unbounded_send(RdpInputEvent::AvcAck(frame_id))
                    .context("send AVC ack")
                    .map_err(IronError::from)?;

                return Ok(JsValue::NULL);
            };
        }

        Err(
            IronError::from(anyhow::Error::msg(format!("unknown extension: {}", ext.ident())))
                .with_kind(IronErrorKind::General),
        )
    }
}

/// Encode a deferred graphics `FrameAcknowledge` for a now-presented AVC frame into
/// wire bytes. Reaches the `GraphicsPipelineClient` through the DRDYNVC static
/// processor (the only exposed mutable path), pops the deferred ack for `frame_id`,
/// and frames it drdynvc → SVC → x224. Returns `None` when the graphics channel isn't
/// active or `frame_id` has no pending ack.
fn encode_avc_frame_ack(active_stage: &mut ActiveStage, frame_id: u32) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(gfx_channel_id) = active_stage
        .get_dvc::<GraphicsPipelineClient>()
        .map(|dvc| dvc.channel_id())
    else {
        return Ok(None);
    };

    let ack_msgs = {
        let Some(drdynvc) = active_stage.get_svc_processor_mut::<DrdynvcClient>() else {
            return Ok(None);
        };
        let Some(mut chan) = drdynvc.get_dvc_by_channel_id_mut::<GraphicsPipelineClient>(gfx_channel_id) else {
            return Ok(None);
        };
        chan.processor_mut().build_frame_ack(frame_id)
    };
    if ack_msgs.is_empty() {
        return Ok(None);
    }

    let svc = ironrdp::dvc::encode_dvc_messages(gfx_channel_id, ack_msgs, ironrdp::svc::ChannelFlags::empty())
        .map_err(|e| anyhow::anyhow!("encode gfx frame-ack (drdynvc): {e}"))?;
    let frame = active_stage
        .encode_dvc_messages(svc)
        .map_err(|e| anyhow::anyhow!("frame gfx frame-ack (svc): {e}"))?;
    Ok(Some(frame))
}

fn into_object(val: JsValue) -> Result<js_sys::Object, IronError> {
    val.dyn_into::<js_sys::Object>()
        .map_err(|_| anyhow::anyhow!("expected object").into())
}

fn get_u32(obj: &js_sys::Object, key: &str) -> Result<u32, IronError> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;
    let f = val
        .as_f64()
        .with_context(|| format!("invalid type for property `{key}`"))?;
    Ok(f64_to_u32_saturating_cast(f))
}

fn get_i32(obj: &js_sys::Object, key: &str) -> Result<i32, IronError> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;
    let f = val
        .as_f64()
        .with_context(|| format!("invalid type for property `{key}`"))?;
    Ok(f64_to_i32_saturating_cast(f))
}

fn get_u64(obj: &js_sys::Object, key: &str) -> Result<u64, IronError> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;
    let f = val
        .as_f64()
        .with_context(|| format!("invalid type for property `{key}`"))?;
    // Validate integer precision before casting
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    if !f.is_finite() || f < 0.0 || f.fract() != 0.0 || f > MAX_SAFE_INTEGER {
        return Err(anyhow::anyhow!(
            "property `{key}` must be a finite non-negative integer <= Number.MAX_SAFE_INTEGER (got: {f})"
        )
        .into());
    }
    #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(f as u64)
}

fn get_bool(obj: &js_sys::Object, key: &str) -> Result<bool, IronError> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;
    val.as_bool()
        .with_context(|| format!("invalid type for property `{key}`"))
        .map_err(Into::into)
}

fn get_u32_opt(obj: &js_sys::Object, key: &str) -> Result<Option<u32>, IronError> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;
    if val.is_undefined() || val.is_null() {
        return Ok(None);
    }
    let f = val
        .as_f64()
        .with_context(|| format!("invalid type for property `{key}`"))?;
    Ok(Some(f64_to_u32_saturating_cast(f)))
}

fn parse_print_job_stream_callbacks(callbacks: JsValue) -> anyhow::Result<JsPrinterStreamCallbacks> {
    let callbacks = callbacks
        .dyn_into::<js_sys::Object>()
        .map_err(|_| anyhow::anyhow!("expected object"))?;

    Ok(JsPrinterStreamCallbacks {
        on_job_start: get_optional_function(&callbacks, "onJobStart")?,
        on_job_data: get_required_function(&callbacks, "onJobData")?,
        on_job_complete: get_required_function(&callbacks, "onJobComplete")?,
        on_job_error: get_optional_function(&callbacks, "onJobError")?,
    })
}

fn parse_sound_callbacks(callbacks: JsValue) -> anyhow::Result<JsSoundCallbacks> {
    let callbacks = callbacks
        .dyn_into::<js_sys::Object>()
        .map_err(|_| anyhow::anyhow!("expected object"))?;

    Ok(JsSoundCallbacks {
        on_wave: get_required_function(&callbacks, "onWave")?,
        on_close: get_optional_function(&callbacks, "onClose")?,
    })
}

fn get_required_function(obj: &js_sys::Object, key: &str) -> anyhow::Result<js_sys::Function> {
    get_optional_function(obj, key)?.with_context(|| format!("missing function `{key}`"))
}

fn get_optional_function(obj: &js_sys::Object, key: &str) -> anyhow::Result<Option<js_sys::Function>> {
    let val = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|e| anyhow::anyhow!("get property `{key}`: {e:?}"))?;

    if val.is_undefined() || val.is_null() {
        return Ok(None);
    }

    val.dyn_into::<js_sys::Function>()
        .map(Some)
        .map_err(|_| anyhow::anyhow!("property `{key}` must be a function"))
}

#[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn f64_to_u32_saturating_cast(f: f64) -> u32 {
    f.clamp(0.0, f64::from(u32::MAX)) as u32
}

#[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
fn f64_to_i32_saturating_cast(f: f64) -> i32 {
    f.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// Parse a JsValue (expected to be a JS array of file metadata objects)
/// into a `Vec<FileMetadata>`.
fn parse_file_metadata_array(files: JsValue) -> Result<Vec<FileMetadata>, IronError> {
    let js_array = js_sys::Array::from(&files);
    #[expect(
        clippy::as_conversions,
        reason = "JavaScript array length is u32, safe to convert to usize"
    )]
    let mut file_list = Vec::with_capacity(js_array.length() as usize);

    for i in 0..js_array.length() {
        let file_obj = js_array.get(i);
        let name = js_sys::Reflect::get(&file_obj, &JsValue::from_str("name"))
            .ok()
            .and_then(|v| v.as_string())
            .context("file name is required")?;
        let size_f64 = js_sys::Reflect::get(&file_obj, &JsValue::from_str("size"))
            .ok()
            .and_then(|v| v.as_f64())
            .context("file size is required")?;
        // JS numbers are f64; reject fractional or out-of-safe-integer-range values
        // to avoid silent truncation when casting to u64
        const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
        if !size_f64.is_finite() || size_f64 < 0.0 || size_f64.fract() != 0.0 || size_f64 > MAX_SAFE_INTEGER {
            return Err(anyhow::anyhow!(
                "file size must be a finite non-negative integer <= Number.MAX_SAFE_INTEGER (got: {size_f64})"
            )
            .into());
        }
        #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let size = size_f64 as u64;

        let last_modified_f64 = js_sys::Reflect::get(&file_obj, &JsValue::from_str("lastModified"))
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        // Store as JS timestamp (ms since Unix epoch). FileMetadata::to_file_descriptor()
        // handles the conversion to Windows FILETIME for the wire format.
        const MAX_SAFE_TS: f64 = 9_007_199_254_740_991.0;
        #[expect(clippy::as_conversions, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let last_modified = if last_modified_f64.is_finite()
            && (0.0..=MAX_SAFE_TS).contains(&last_modified_f64)
            && last_modified_f64.fract() == 0.0
        {
            last_modified_f64 as u64
        } else {
            0
        };

        let path = js_sys::Reflect::get(&file_obj, &JsValue::from_str("path"))
            .ok()
            .and_then(|v| v.as_string())
            .filter(|s| !s.is_empty());

        let is_directory = js_sys::Reflect::get(&file_obj, &JsValue::from_str("isDirectory"))
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        file_list.push(FileMetadata {
            name,
            path,
            size,
            last_modified,
            is_directory,
        });
    }

    Ok(file_list)
}

/// Parse the `remote_app` extension value — a JS object
/// `{ program, args?, workingDir? }` — into a RAIL config. Returns `None` when
/// `program` is missing/empty (so RAIL stays disabled).
/// Parses the `monitors` extension payload — a JS array of
/// `{ left, top, width, height, primary? }` objects (virtual-desktop pixel
/// coordinates) — into GCC [`Monitor`](GccMonitor) rectangles.
///
/// `right`/`bottom` are inclusive (`right = left + width - 1`). Entries with a
/// non-positive width or height are skipped. Exactly one monitor must be primary:
/// if none is flagged, the first entry is promoted; if several are, only the first
/// primary is kept. A non-array or empty payload yields an empty vec (single-monitor
/// fallback).
fn parse_monitors(value: &JsValue) -> Vec<GccMonitor> {
    let Some(array) = value.dyn_ref::<js_sys::Array>() else {
        warn!("`monitors` extension: expected a JSON array of monitor rectangles");
        return Vec::new();
    };

    let read_f64 = |entry: &JsValue, key: &str| -> Option<f64> {
        js_sys::Reflect::get(entry, &JsValue::from_str(key))
            .ok()
            .and_then(|v| v.as_f64())
    };

    #[expect(clippy::as_conversions, clippy::cast_possible_truncation)]
    let to_i32 = |v: f64| v.round() as i32;

    let mut monitors = Vec::new();
    for entry in array.iter() {
        let left = read_f64(&entry, "left").map(to_i32).unwrap_or(0);
        let top = read_f64(&entry, "top").map(to_i32).unwrap_or(0);
        let width = read_f64(&entry, "width").map(to_i32).unwrap_or(0);
        let height = read_f64(&entry, "height").map(to_i32).unwrap_or(0);

        if width <= 0 || height <= 0 {
            warn!(width, height, "`monitors` extension: skipping monitor with non-positive size");
            continue;
        }

        let primary = js_sys::Reflect::get(&entry, &JsValue::from_str("primary"))
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        monitors.push(GccMonitor {
            left,
            top,
            // Inclusive far edges, matching FreeRDP's TS_MONITOR_DEF convention.
            right: left.saturating_add(width).saturating_sub(1),
            bottom: top.saturating_add(height).saturating_sub(1),
            flags: if primary {
                MonitorFlags::PRIMARY
            } else {
                MonitorFlags::empty()
            },
        });
    }

    // Enforce exactly one primary: promote the first when none is set, and clear
    // extra primaries so the GCC block (and the server) sees a single primary.
    let primary_count = monitors
        .iter()
        .filter(|m| m.flags.contains(MonitorFlags::PRIMARY))
        .count();
    if !monitors.is_empty() && primary_count == 0 {
        monitors[0].flags |= MonitorFlags::PRIMARY;
    } else if primary_count > 1 {
        let mut seen_primary = false;
        for monitor in &mut monitors {
            if monitor.flags.contains(MonitorFlags::PRIMARY) {
                if seen_primary {
                    monitor.flags.remove(MonitorFlags::PRIMARY);
                } else {
                    seen_primary = true;
                }
            }
        }
    }

    monitors
}

/// Computes the bounding-box size (width, height) spanning every monitor rectangle,
/// clamped to the `u16` desktop-size range. `right`/`bottom` are inclusive, so the
/// span is `max_right - min_left + 1` by `max_bottom - min_top + 1`. Returns `None`
/// for an empty list or a degenerate box.
fn monitors_bounding_box(monitors: &[GccMonitor]) -> Option<(u16, u16)> {
    let min_left = monitors.iter().map(|m| m.left).min()?;
    let min_top = monitors.iter().map(|m| m.top).min()?;
    let max_right = monitors.iter().map(|m| m.right).max()?;
    let max_bottom = monitors.iter().map(|m| m.bottom).max()?;

    let width = i64::from(max_right) - i64::from(min_left) + 1;
    let height = i64::from(max_bottom) - i64::from(min_top) + 1;

    if width <= 0 || height <= 0 {
        return None;
    }

    let width = u16::try_from(width).unwrap_or(u16::MAX);
    let height = u16::try_from(height).unwrap_or(u16::MAX);

    Some((width, height))
}

fn parse_remote_app(value: &JsValue) -> Option<connector::RailConfig> {
    let get = |key: &str| {
        js_sys::Reflect::get(value, &JsValue::from_str(key))
            .ok()
            .and_then(|v| v.as_string())
    };
    let program = get("program").filter(|p| !p.is_empty())?;
    Some(connector::RailConfig {
        exe_or_file: program,
        working_dir: get("workingDir").unwrap_or_default(),
        arguments: get("args").unwrap_or_default(),
    })
}

/// Log a decoded RAIL Window List order at INFO so the RemoteApp window
/// lifecycle is observable in the browser console. This is the seam a future
/// local-window renderer would hook to composite real windows.
fn log_rail_window_order(order: &WindowOrder) {
    match order {
        WindowOrder::CreateWindow { window_id, state } => {
            info!(
                window_id = format!("{window_id:#x}"),
                title = state.title.as_deref().unwrap_or(""),
                offset = ?state.window_offset,
                size = ?state.window_size,
                show = ?state.show_state,
                "RAIL: window created"
            );
        }
        WindowOrder::UpdateWindow { window_id, state } => {
            info!(
                window_id = format!("{window_id:#x}"),
                offset = ?state.window_offset,
                size = ?state.window_size,
                "RAIL: window updated"
            );
        }
        WindowOrder::DeleteWindow { window_id } => {
            info!(window_id = format!("{window_id:#x}"), "RAIL: window deleted");
        }
        WindowOrder::WindowIcon { window_id, cached } => {
            info!(window_id = format!("{window_id:#x}"), cached, "RAIL: window icon");
        }
        WindowOrder::NotifyIcon {
            window_id,
            notify_id,
            deleted,
        } => {
            info!(
                window_id = format!("{window_id:#x}"),
                notify_id, deleted, "RAIL: notify icon"
            );
        }
        WindowOrder::Desktop {
            active_window_id,
            window_ids,
        } => {
            info!(active = ?active_window_id, count = window_ids.len(), "RAIL: monitored desktop / z-order");
        }
    }
}

fn build_config(
    username: String,
    password: String,
    domain: Option<String>,
    client_name: String,
    desktop_size: DesktopSize,
) -> connector::Config {
    connector::Config {
        credentials: Credentials::UsernamePassword { username, password },
        domain,
        // TODO(#327): expose these options from the WASM module.
        enable_tls: true,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        keyboard_type: ironrdp::pdu::gcc::KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0, // the server SHOULD use the default active input locale identifier
        keyboard_functional_keys_count: 12,
        connection_type: ironrdp::pdu::gcc::ConnectionType::Lan,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize {
            width: desktop_size.width,
            height: desktop_size.height,
        },
        // Multi-monitor layout is populated after build (from the `monitors`
        // extension); empty means a single implicit monitor (legacy behavior).
        monitors: Vec::new(),
        bitmap: Some(connector::BitmapConfig {
            // Request a 32bpp session: with 32 the connector emits highColorDepth=24
            // + WANT_32_BPP_SESSION in the GCC client core data. Advertising 16 here
            // makes proxies/servers clamp the session to 16bpp (RGB565), which the
            // eGFX renderer then misreads as 32bpp XRGB → scrambled colors.
            color_depth: 32,
            lossy_compression: true,
            codecs: client_codecs_capabilities(&[]).expect("can't panic for &[]"),
        }),
        #[expect(
            clippy::arithmetic_side_effects,
            reason = "fine unless we end up with an insanely big version"
        )]
        client_build: semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .map_or(0, |version| version.major * 100 + version.minor * 10 + version.patch)
            .pipe(u32::try_from)
            .expect("fine until major ~42949672"),
        client_name,
        // NOTE: hardcode this value like in freerdp
        // https://github.com/FreeRDP/FreeRDP/blob/4e24b966c86fdf494a782f0dfcfc43a057a2ea60/libfreerdp/core/settings.c#LL49C34-L49C70
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: ironrdp::pdu::rdp::capability_sets::MajorPlatformType::UNSPECIFIED,
        compression_type: None,
        enable_server_pointer: false,
        autologon: false,
        enable_audio_playback: false,
        request_data: None,
        pointer_software_rendering: false,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        // eGFX (MS-RDPEGFX) is DISABLED so the graphics path is classic RemoteFX
        // (MS-RDPRFX) over Surface Bits instead. Rationale: IronRDP's eGFX decode is
        // an incomplete foundation — it decodes only AVC (H.264), Uncompressed and RFX
        // Progressive, and does NOT implement the ClearCodec/Planar/NSCodec that carry
        // the bulk of desktop UI/text over eGFX (see the `"unsupported codec"` fallback
        // in ironrdp-egfx, tracked upstream); that codec mix, plus
        // the fragile stateful surface-cache/compositing, is what produced the artifacts.
        // Classic RemoteFX instead encodes the WHOLE framebuffer as RFX tiles with a
        // single mature codec that IronRDP fully decodes (ironrdp-session `rfx.rs`,
        // dispatched from `fast_path.rs` on CODEC_ID_REMOTEFX). We advertise it via the
        // RemoteFX bitmap codec in `bitmap.codecs` below (client_codecs_capabilities);
        // turning eGFX off makes the (FreeRDP) proxy/server pick RemoteFX Surface Bits.
        // eGFX is ON: the client core now decodes the full multi-codec eGFX stream
        // (ClearCodec + RFX Progressive + AVC/uncompressed). The Bubble rdp-proxy
        // REQUIRES the client to announce eGFX, so this is also what the proxy expects.
        support_graphics_pipeline: true,
        // RAIL (Remote Programs) is populated after build (from the `remoteApp`
        // extension); None means a normal desktop/alternate-shell session.
        rail: None,
    }
}

async fn writer_task(
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    rdp_writer: WriteHalf<WebSocket>,
    outbound_limit: Option<usize>,
) {
    debug!("writer task started");

    async fn inner(
        mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
        mut rdp_writer: WriteHalf<WebSocket>,
        outbound_limit: Option<usize>,
    ) -> anyhow::Result<()> {
        while let Some(frame) = rx.next().await {
            match outbound_limit {
                Some(max_size) if frame.len() > max_size => {
                    // Send in chunks.
                    for chunk in frame.chunks(max_size) {
                        rdp_writer.write_all(chunk).await.context("couldn't write chunk")?;
                        rdp_writer.flush().await.context("couldn't flush chunk")?;
                    }
                }
                _ => {
                    // Send complete frame (default case).
                    rdp_writer.write_all(&frame).await.context("couldn't write frame")?;
                    rdp_writer.flush().await.context("couldn't flush frame")?;
                }
            }
        }

        Ok(())
    }

    match inner(rx, rdp_writer, outbound_limit).await {
        Ok(()) => debug!("writer task ended gracefully"),
        Err(e) => error!("writer task ended unexpectedly: {e:#}"),
    }
}

struct ConnectParams {
    ws: WebSocket,
    config: connector::Config,
    proxy_auth_token: String,
    destination: String,
    pcb: Option<String>,
    kdc_proxy_url: Option<String>,
    clipboard_backend: Option<WasmClipboardBackend>,
    printer_backend: Option<WasmPrinterBackend>,
    printer_device_id: u32,
    printer_name: String,
    printer_driver_name: String,
    sound_backend: Option<WasmSoundBackend>,
    /// Matches the `client_name` in the connector config; used as the
    /// `computer_name` when constructing the `Rdpdr` processor.
    computer_name: String,
    use_display_control: bool,
    /// Advertise AVC eGFX caps (H.264). Gated by the web UI "Enhanced graphics"
    /// toggle via the `advertise_avc` builder extension.
    advertise_avc: bool,
    graphics_handler: Option<WasmGraphicsHandler>,
}

fn default_printer_driver_name() -> String {
    printer_driver_name_for_macos_major_version(browser_macos_major_version()).to_owned()
}

fn printer_driver_name_for_macos_major_version(macos_major_version: Option<u32>) -> &'static str {
    if macos_major_version.is_some_and(|major| 14 <= major) {
        MICROSOFT_PRINT_TO_PDF_DRIVER_NAME
    } else {
        DEFAULT_PRINTER_DRIVER_NAME
    }
}

#[cfg(target_arch = "wasm32")]
fn browser_macos_major_version() -> Option<u32> {
    let user_agent = web_sys::window()?.navigator().user_agent().ok()?;
    macos_major_version_from_user_agent(&user_agent)
}

#[cfg(not(target_arch = "wasm32"))]
fn browser_macos_major_version() -> Option<u32> {
    None
}

#[cfg(any(target_arch = "wasm32", test))]
fn macos_major_version_from_user_agent(user_agent: &str) -> Option<u32> {
    let (_, version) = user_agent.split_once("Mac OS X ")?;
    version
        .split(|ch: char| !ch.is_ascii_digit())
        .next()
        .and_then(|major| major.parse().ok())
}

async fn connect(
    ConnectParams {
        ws,
        config,
        proxy_auth_token,
        destination,
        pcb,
        kdc_proxy_url,
        clipboard_backend,
        printer_backend,
        printer_device_id,
        printer_name,
        printer_driver_name,
        sound_backend,
        computer_name,
        use_display_control,
        advertise_avc,
        graphics_handler,
    }: ConnectParams,
) -> Result<(connector::ConnectionResult, WebSocket), IronError> {
    let mut framed = ironrdp_futures::LocalFuturesFramed::new(ws);

    // In web browser environments, we do not have an easy access to the local address of the socket.
    let dummy_client_addr = core::net::SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 33899));

    // Capture the RAIL app before `config` is moved into the connector, so we can
    // attach the matching `rail` static channel below.
    let rail = config.rail.clone();

    let mut connector = ClientConnector::new(config, dummy_client_addr);

    if let Some(clipboard_backend) = clipboard_backend {
        connector.attach_static_channel(CliprdrClient::new(Box::new(clipboard_backend)));
    }

    // RAIL (Remote Programs): launch the published app over the `rail` channel.
    // The connector already advertised RAIL + Window List caps and set INFO_RAIL.
    if let Some(rail) = rail {
        connector.attach_static_channel(RailChannel::with_app(RemoteApp {
            exe_or_file: rail.exe_or_file,
            working_dir: rail.working_dir,
            arguments: rail.arguments,
        }));
    }

    // RDPSND (audio output). Attached when audio playback is enabled, OR as a no-op
    // when only the printer is redirected: Windows servers only speak on RDPDR when
    // RDPSND is advertised too (MS-RDPEFS Appendix A<1>), so the printer path needs
    // the channel present even though it wants no sound. A real sound backend also
    // satisfies that dependency, so it takes priority when both are configured.
    //
    // Audio also rides a dynamic virtual channel: modern hosts (Windows 10/11,
    // Azure Virtual Desktop) prefer `AUDIO_PLAYBACK_DVC` over the static channel.
    // When real audio is enabled we register BOTH — the static "rdpsnd" SVC below
    // and the `AUDIO_PLAYBACK_DVC` DVC further down — sharing one sound sink (a
    // clone of the backend). The server opens whichever transport it prefers; the
    // idle one simply never receives a Server Audio Formats PDU. This clone is
    // taken before the match consumes `sound_backend` for the static channel.
    let dvc_sound_backend = sound_backend.clone();

    match (sound_backend, printer_backend.is_some()) {
        (Some(sound_backend), _) => {
            connector.attach_static_channel(Rdpsnd::new(Box::new(sound_backend)));
        }
        (None, true) => {
            connector.attach_static_channel(Rdpsnd::new(Box::new(NoopRdpsndBackend)));
        }
        (None, false) => {}
    }

    if let Some(printer_backend) = printer_backend {
        connector.attach_static_channel(
            Rdpdr::new(Box::new(printer_backend), computer_name).with_printer_driver(
                printer_device_id,
                printer_name,
                printer_driver_name,
            ),
        );
    }

    // DisplayControl, the EGFX graphics pipeline, and AUDIO_PLAYBACK_DVC audio are
    // all dynamic virtual channels, so they ride the same single DRDYNVC static
    // channel.
    if use_display_control || graphics_handler.is_some() || dvc_sound_backend.is_some() {
        let mut drdynvc = DrdynvcClient::new();
        if let Some(dvc_sound_backend) = dvc_sound_backend {
            // AUDIO_PLAYBACK_DVC (MS-RDPEA over DVC). Same RDPSND PDU flow as the
            // static "rdpsnd" channel registered above, different transport; AVD
            // prefers this one. Shares the static backend's sound sink via clone.
            drdynvc = drdynvc.with_dynamic_channel(RdpsndDvcClient::new(Box::new(dvc_sound_backend)));
        }
        if use_display_control {
            drdynvc = drdynvc.with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())));
        }
        if let Some(graphics_handler) = graphics_handler {
            // AVC MVP (2026-08-10): advertise AVC420/AVC444 caps so the server (AVD/
            // Windows) sends AVC. The H.264 main sub-stream is decoded out-of-band by
            // the browser WebCodecs decoder (see `AvcFrame`/`on_avc_frame` →
            // `avc_decode_callback`), giving full-color 4:2:0 desktop/video.
            //
            // Now controlled per-connection by `advertise_avc` (set from the web UI
            // "Enhanced graphics" toggle via the `advertise_avc` extension). When
            // false the server falls back to ClearCodec / RFX-Progressive, which this
            // client already decodes correctly — so a PROD build can ship AVC off by
            // simply not enabling the toggle, without a code change.
            drdynvc = drdynvc.with_dynamic_channel(
                GraphicsPipelineClient::new(Box::new(graphics_handler), None).advertise_avc(advertise_avc),
            );
        }
        connector.attach_static_channel(drdynvc);
    }

    let (upgraded, server_public_key) =
        connect_rdcleanpath(&mut framed, &mut connector, destination.clone(), proxy_auth_token, pcb).await?;

    let connection_result = ironrdp_futures::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut WasmNetworkClient,
        (&destination).into(),
        server_public_key,
        url::Url::parse(kdc_proxy_url.unwrap_or_default().as_str()) // if kdc_proxy_url does not exit, give url parser a empty string, it will fail anyway and map to a None
            .ok()
            .map(|url| KerberosConfig {
                kdc_proxy_url: Some(url),
                // HACK: It's supposed to be the computer name of the client, but since it's not easy to retrieve this information in the browser,
                // we set the destination hostname instead because it happens to work.
                hostname: destination,
            }),
    )
    .await?;

    let ws = framed.into_inner_no_leftover();

    Ok((connection_result, ws))
}

async fn connect_rdcleanpath<S>(
    framed: &mut ironrdp_futures::Framed<S>,
    connector: &mut ClientConnector,
    destination: String,
    proxy_auth_token: String,
    pcb: Option<String>,
) -> Result<(ironrdp_futures::Upgraded, Vec<u8>), IronError>
where
    S: ironrdp_futures::FramedRead + FramedWrite,
{
    use ironrdp::connector::Sequence as _;
    use x509_cert::der::Decode as _;

    #[derive(Clone, Copy, Debug)]
    struct RDCleanPathHint;

    const RDCLEANPATH_HINT: RDCleanPathHint = RDCleanPathHint;

    impl ironrdp::pdu::PduHint for RDCleanPathHint {
        fn find_size(&self, bytes: &[u8]) -> ironrdp::core::DecodeResult<Option<(bool, usize)>> {
            match ironrdp_rdcleanpath::RDCleanPathPdu::detect(bytes) {
                ironrdp_rdcleanpath::DetectionResult::Detected { total_length, .. } => Ok(Some((true, total_length))),
                ironrdp_rdcleanpath::DetectionResult::NotEnoughBytes => Ok(None),
                ironrdp_rdcleanpath::DetectionResult::Failed => Err(ironrdp::core::other_err!(
                    "RDCleanPathHint",
                    "detection failed (invalid PDU)"
                )),
            }
        }
    }

    let mut buf = WriteBuf::new();

    info!("Begin connection procedure");

    {
        // RDCleanPath request

        let connector::ClientConnectorState::ConnectionInitiationSendRequest = connector.state else {
            return Err(anyhow::Error::msg("invalid connector state (send request)").into());
        };

        debug_assert!(connector.next_pdu_hint().is_none());

        let written = connector.step_no_input(&mut buf)?;
        let x224_pdu_len = written.size().expect("written size");
        debug_assert_eq!(x224_pdu_len, buf.filled_len());
        let x224_pdu = buf.filled().to_vec();

        let rdcleanpath_req =
            ironrdp_rdcleanpath::RDCleanPathPdu::new_request(x224_pdu, destination, proxy_auth_token, pcb)
                .context("new RDCleanPath request")?;
        debug!(message = ?rdcleanpath_req, "Send RDCleanPath request");
        let rdcleanpath_req = rdcleanpath_req.to_der().context("RDCleanPath request encode")?;

        framed
            .write_all(&rdcleanpath_req)
            .await
            .context("couldn't write RDCleanPath request")?;
    }

    {
        // RDCleanPath response

        let rdcleanpath_res = framed
            .read_by_hint(&RDCLEANPATH_HINT)
            .await
            .context("read RDCleanPath request")?;

        let rdcleanpath_res =
            ironrdp_rdcleanpath::RDCleanPathPdu::from_der(&rdcleanpath_res).context("RDCleanPath response decode")?;

        debug!(message = ?rdcleanpath_res, "Received RDCleanPath PDU");

        let (x224_connection_response, server_cert_chain) =
            match rdcleanpath_res.into_enum().context("invalid RDCleanPath PDU")? {
                ironrdp_rdcleanpath::RDCleanPath::Request { .. } => {
                    return Err(anyhow::Error::msg("received an unexpected RDCleanPath type (request)").into());
                }
                ironrdp_rdcleanpath::RDCleanPath::Response {
                    x224_connection_response,
                    server_cert_chain,
                    server_addr: _,
                } => (x224_connection_response, server_cert_chain),
                ironrdp_rdcleanpath::RDCleanPath::GeneralErr(error) => {
                    let details = iron_remote_desktop::RDCleanPathDetails::new(
                        error.http_status_code,
                        error.wsa_last_error,
                        error.tls_alert_code,
                    );
                    return Err(
                        IronError::from(anyhow::Error::new(error).context("received an RDCleanPath error"))
                            .with_kind(IronErrorKind::RDCleanPath)
                            .with_rdcleanpath_details(details),
                    );
                }
                ironrdp_rdcleanpath::RDCleanPath::NegotiationErr {
                    x224_connection_response,
                } => {
                    // Try to decode as X.224 Connection Confirm to extract negotiation failure details.
                    if let Ok(x224_confirm) = ironrdp_core::decode::<
                        ironrdp::pdu::x224::X224<ironrdp::pdu::nego::ConnectionConfirm>,
                    >(&x224_connection_response)
                    {
                        if let ironrdp::pdu::nego::ConnectionConfirm::Failure { code } = x224_confirm.0 {
                            // Convert to negotiation failure instead of generic RDCleanPath error.
                            let negotiation_failure = connector::NegotiationFailure::from(code);
                            return Err(IronError::from(
                                anyhow::Error::new(negotiation_failure).context("RDP negotiation failed"),
                            )
                            .with_kind(IronErrorKind::NegotiationFailure));
                        }
                    }

                    // Fallback to generic error if we can't decode the negotiation failure.
                    return Err(
                        IronError::from(anyhow::Error::msg("received an RDCleanPath negotiation error"))
                            .with_kind(IronErrorKind::RDCleanPath),
                    );
                }
            };

        let connector::ClientConnectorState::ConnectionInitiationWaitConfirm { .. } = connector.state else {
            return Err(anyhow::Error::msg("invalid connector state (wait confirm)").into());
        };

        debug_assert!(connector.next_pdu_hint().is_some());

        buf.clear();
        let written = connector.step(x224_connection_response.as_bytes(), &mut buf)?;

        debug_assert!(written.is_nothing());

        let server_cert = server_cert_chain
            .into_iter()
            .next()
            .context("server cert chain missing from rdcleanpath response")?;

        let cert = x509_cert::Certificate::from_der(server_cert.as_bytes())
            .context("failed to decode x509 certificate sent by proxy")?;

        let server_public_key = cert
            .tbs_certificate()
            .subject_public_key_info()
            .subject_public_key
            .as_bytes()
            .context("subject public key BIT STRING is not aligned")?
            .to_owned();

        let should_upgrade = ironrdp_futures::skip_connect_begin(connector);

        // At this point, proxy established the TLS session.

        let upgraded = ironrdp_futures::mark_as_upgraded(should_upgrade, connector);

        Ok((upgraded, server_public_key))
    }
}

#[expect(clippy::as_conversions, clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn f64_to_u16_saturating_cast(value: f64) -> u16 {
    value as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test helpers
    fn create_test_input_channel() -> (
        mpsc::UnboundedSender<RdpInputEvent>,
        mpsc::UnboundedReceiver<RdpInputEvent>,
    ) {
        mpsc::unbounded()
    }

    #[test]
    fn printer_driver_defaults_to_postscript_when_macos_version_is_unknown() {
        assert_eq!(
            printer_driver_name_for_macos_major_version(None),
            DEFAULT_PRINTER_DRIVER_NAME
        );
    }

    #[test]
    fn printer_driver_uses_pdf_for_macos_14_and_newer() {
        assert_eq!(
            printer_driver_name_for_macos_major_version(Some(13)),
            DEFAULT_PRINTER_DRIVER_NAME
        );
        assert_eq!(
            printer_driver_name_for_macos_major_version(Some(14)),
            MICROSOFT_PRINT_TO_PDF_DRIVER_NAME
        );
        assert_eq!(
            printer_driver_name_for_macos_major_version(Some(15)),
            MICROSOFT_PRINT_TO_PDF_DRIVER_NAME
        );
    }

    #[test]
    fn macos_major_version_is_parsed_from_user_agent() {
        assert_eq!(
            macos_major_version_from_user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6_1) AppleWebKit/605.1.15"),
            Some(14)
        );
        assert_eq!(
            macos_major_version_from_user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36"),
            Some(10)
        );
        assert_eq!(
            macos_major_version_from_user_agent("Mozilla/5.0 (Windows NT 10.0)"),
            None
        );
    }

    #[test]
    fn test_request_file_contents_parameter_marshalling() {
        let (tx, mut rx) = create_test_input_channel();

        // Send request with various parameters
        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id: 123,
                index: 5,
                flags: FileContentsFlags::RANGE,
                position: 1024,
                size: 4096,
                clip_data_id: Some(42),
            },
        ))
        .unwrap();

        // Verify message parameters
        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id,
                index,
                flags,
                position,
                size,
                clip_data_id,
            })) => {
                assert_eq!(stream_id, 123);
                assert_eq!(index, 5);
                assert_eq!(flags, FileContentsFlags::RANGE);
                assert_eq!(position, 1024);
                assert_eq!(size, 4096);
                assert_eq!(clip_data_id, Some(42));
            }
            _ => panic!("Expected FileContentsRequestSend with correct parameters"),
        }
    }

    #[test]
    fn test_request_file_contents_size_flag() {
        let (tx, mut rx) = create_test_input_channel();

        // Send SIZE request
        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id: 1,
                index: 0,
                flags: FileContentsFlags::SIZE,
                position: 0,
                size: 8,
                clip_data_id: Some(1),
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsRequestSend {
                flags, ..
            })) => {
                assert_eq!(flags, FileContentsFlags::SIZE);
            }
            _ => panic!("Expected SIZE request"),
        }
    }

    #[test]
    fn test_request_file_contents_without_clip_data_id() {
        let (tx, mut rx) = create_test_input_channel();

        // Send request without clip_data_id
        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id: 10,
                index: 0,
                flags: FileContentsFlags::RANGE,
                position: 0,
                size: 1024,
                clip_data_id: None,
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsRequestSend {
                clip_data_id,
                ..
            })) => {
                assert_eq!(clip_data_id, None);
            }
            _ => panic!("Expected request without clip_data_id"),
        }
    }

    #[test]
    fn test_submit_file_contents_success_response() {
        let (tx, mut rx) = create_test_input_channel();

        let data = vec![1, 2, 3, 4, 5];
        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsResponseSend {
                stream_id: 42,
                is_error: false,
                data: data.clone(),
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsResponseSend {
                stream_id,
                is_error,
                data: received_data,
            })) => {
                assert_eq!(stream_id, 42);
                assert!(!is_error);
                assert_eq!(received_data, data);
            }
            _ => panic!("Expected FileContentsResponseSend success"),
        }
    }

    #[test]
    fn test_submit_file_contents_error_response() {
        let (tx, mut rx) = create_test_input_channel();

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsResponseSend {
                stream_id: 99,
                is_error: true,
                data: vec![],
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsResponseSend {
                is_error,
                ..
            })) => {
                assert!(is_error);
            }
            _ => panic!("Expected error response"),
        }
    }

    #[test]
    fn test_submit_file_contents_size_response() {
        let (tx, mut rx) = create_test_input_channel();

        // 8-byte size response (little-endian)
        let size_data = vec![0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]; // 4096 bytes

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsResponseSend {
                stream_id: 1,
                is_error: false,
                data: size_data.clone(),
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsResponseSend {
                data, ..
            })) => {
                assert_eq!(data.len(), 8);
                assert_eq!(data, size_data);
            }
            _ => panic!("Expected size response"),
        }
    }

    #[test]
    fn test_initiate_file_copy_message() {
        let (tx, mut rx) = create_test_input_channel();

        let files = vec![
            FileMetadata {
                name: "file1.txt".to_owned(),
                path: None,
                size: 1024,
                last_modified: 1_700_000_000_000,
                is_directory: false,
            },
            FileMetadata {
                name: "file2.pdf".to_owned(),
                path: Some("docs".to_owned()),
                size: 2048,
                last_modified: 1_700_000_001_000,
                is_directory: false,
            },
        ];

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::InitiateFileCopy { files },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::InitiateFileCopy {
                files: received_files,
            })) => {
                assert_eq!(received_files.len(), 2);
                assert_eq!(received_files[0].name, "file1.txt");
                assert_eq!(received_files[0].path, None);
                assert_eq!(received_files[0].size, 1024);
                assert_eq!(received_files[1].name, "file2.pdf");
                assert_eq!(received_files[1].path, Some("docs".to_owned()));
                assert_eq!(received_files[1].size, 2048);
            }
            _ => panic!("Expected InitiateFileCopy message"),
        }
    }

    #[test]
    fn test_large_position_value_marshalling() {
        let (tx, mut rx) = create_test_input_channel();

        // Test with large position value (near u64 max)
        let large_position = u64::MAX - 1000;

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id: 1,
                index: 0,
                flags: FileContentsFlags::RANGE,
                position: large_position,
                size: 1024,
                clip_data_id: Some(1),
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsRequestSend {
                position,
                ..
            })) => {
                assert_eq!(position, large_position);
            }
            _ => panic!("Expected correct position marshalling"),
        }
    }

    #[test]
    fn test_zero_size_file() {
        let (tx, mut rx) = create_test_input_channel();

        let files = vec![FileMetadata {
            name: "empty.txt".to_owned(),
            path: None,
            size: 0,
            last_modified: 0,
            is_directory: false,
        }];

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::InitiateFileCopy { files },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::InitiateFileCopy {
                files: received_files,
            })) => {
                assert_eq!(received_files[0].size, 0);
            }
            _ => panic!("Expected zero-size file"),
        }
    }

    #[test]
    fn test_file_with_special_characters_in_name() {
        let (tx, mut rx) = create_test_input_channel();

        let files = vec![FileMetadata {
            name: "test file (1) [copy].txt".to_owned(),
            path: None,
            size: 100,
            last_modified: 0,
            is_directory: false,
        }];

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::InitiateFileCopy { files },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::InitiateFileCopy {
                files: received_files,
            })) => {
                assert_eq!(received_files[0].name, "test file (1) [copy].txt");
            }
            _ => panic!("Expected file with special characters"),
        }
    }

    #[test]
    fn test_empty_file_list() {
        let (tx, mut rx) = create_test_input_channel();

        let files: Vec<FileMetadata> = vec![];

        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::InitiateFileCopy { files },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::InitiateFileCopy {
                files: received_files,
            })) => {
                assert!(received_files.is_empty());
            }
            _ => panic!("Expected empty file list"),
        }
    }

    #[test]
    fn test_flags_bits_conversion() {
        let (tx, mut rx) = create_test_input_channel();

        // Test SIZE flag (0x1)
        let size_flags = FileContentsFlags::SIZE;
        assert_eq!(size_flags.bits(), 0x1);

        // Test DATA flag (0x2)
        let data_flags = FileContentsFlags::RANGE;
        assert_eq!(data_flags.bits(), 0x2);

        // Test that flags convert correctly through the channel
        tx.unbounded_send(RdpInputEvent::ClipboardBackend(
            WasmClipboardBackendMessage::FileContentsRequestSend {
                stream_id: 1,
                index: 0,
                flags: FileContentsFlags::from_bits_truncate(0x1),
                position: 0,
                size: 8,
                clip_data_id: None,
            },
        ))
        .unwrap();

        match rx.try_recv() {
            Ok(RdpInputEvent::ClipboardBackend(WasmClipboardBackendMessage::FileContentsRequestSend {
                flags, ..
            })) => {
                assert_eq!(flags.bits(), 0x1);
            }
            _ => panic!("Expected correct flags conversion"),
        }
    }
}
