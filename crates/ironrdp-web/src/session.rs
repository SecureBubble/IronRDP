use core::cell::RefCell;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::num::NonZeroU32;
use core::time::Duration;
use std::borrow::Cow;
use std::collections::HashMap;
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
use ironrdp::rdperp::orders::{WindowOrder, WindowState};
use ironrdp::rdperp::pdu::RAIL_WMSZ_MOVE;
use ironrdp::rdpsnd::client::{NoopRdpsndBackend, Rdpsnd, RdpsndClientHandler, RdpsndDvcListener};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput, GracefulDisconnectReason};
use ironrdp::svc::SvcProcessorMessages;
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
use crate::graphics::{LocalDrag, RailWindowStore, WasmGraphicsHandler, WasmGraphicsMessageProxy, Watermark};
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
    /// Unified WebGL present callback (extension `surface_present_callback`). When set, the run loop
    /// forwards each decoded NON-AVC region `(x, y, width, height, rgba: Uint8Array)` to JS — which
    /// uploads it into the single surface-0 WebGL texture via `texSubImage2D` — INSTEAD of painting
    /// the 2D canvas. This is the GPU-composite path (`?ironwebgl=1`); `None` = classic 2D
    /// `put_image_data`. The 2D `render_canvas` stays blank behind the JS-owned WebGL canvas.
    surface_present_callback: Option<js_sys::Function>,
    /// WebGL present layout (extension `surface_layout_callback`). Fired when the RemoteApp
    /// window layout (or present mode) changes so the JS renderer knows what to clip the
    /// surface-0 texture to. Only meaningful alongside `surface_present_callback`.
    surface_layout_callback: Option<js_sys::Function>,
    /// WebGL GPU copy callback (extension `surface_copy_callback`). Executes eGFX
    /// SurfaceToSurface copies inside the GPU surface texture.
    surface_copy_callback: Option<js_sys::Function>,
    /// RAIL active-window notification (extension `rail_window_callback`). Fired
    /// when the presentation rect of the active RAIL (RemoteApp) top-level window
    /// changes, with `(x, y, width, height)` in virtual-desktop coordinates. The
    /// webapp uses it to crop/scale the canvas so only the app window fills the
    /// viewport (RAIL paints the whole desktop surface; the surround is stale /
    /// unpainted). `x`/`y` can be negative in RAIL, hence i32. Absent (a full
    /// desktop session) => never fired, so the crop is never applied.
    rail_window_callback: Option<js_sys::Function>,
    /// Render-canvas backing-store resize notification (trait `canvas_resized_callback`).
    /// Fired after the run loop resizes the canvas to a HiDef RAIL window surface so the
    /// JS element re-fits the (now differently-sized) canvas to the viewport with its
    /// existing single-monitor "fit" logic. The `<iron-remote-desktop>` element registers
    /// this callback unconditionally, but the bundle only ever invokes it on the HiDef RAIL
    /// window-present path — a normal desktop session never resizes the canvas here, so its
    /// path is unchanged. Absent => the resize still happens; only the re-fit is skipped.
    canvas_resized_callback: Option<js_sys::Function>,

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
            surface_present_callback: None,
            surface_layout_callback: None,
            surface_copy_callback: None,
            rail_window_callback: None,
            canvas_resized_callback: None,

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

    /// Classic RDP never resizes the framebuffer, but HiDef RAIL (piece 3) does: the app
    /// window's eGFX surface — presented AS the canvas — can differ in size from the
    /// negotiated desktop. Store the callback so the run loop can tell the JS element to
    /// re-fit after it resizes the canvas backing store. Never invoked for a normal desktop
    /// session (the canvas is only resized on the window-present path).
    fn canvas_resized_callback(&self, callback: js_sys::Function) -> Self {
        self.0.borrow_mut().canvas_resized_callback = Some(callback);
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
            // Unified WebGL present: registering it routes each NON-AVC region's RGBA to JS (which
            // uploads it into the surface-0 WebGL texture) instead of the 2D canvas.
            |surface_present_callback: JsValue| {
                self.0.borrow_mut().surface_present_callback = surface_present_callback.dyn_into::<js_sys::Function>().ok();
            };
            // WebGL present layout: registering it lets the run loop tell the JS renderer which
            // RemoteApp window rects to clip the surface-0 texture to (and when to blank or
            // present full-screen). Inert without `surface_present_callback`.
            |surface_layout_callback: JsValue| {
                self.0.borrow_mut().surface_layout_callback = surface_layout_callback.dyn_into::<js_sys::Function>().ok();
            };
            // RAIL (RemoteApp) active-window rect notification. Registering it lets the
            // run loop report the active top-level window's presentation rect so the
            // webapp can crop/scale to just that window (see field docs). Passive: it
            // never affects protocol / frame-ack state and never fires for a full desktop.
            // WebGL GPU copy: executes SurfaceToSurface inside the GPU surface texture.
            |surface_copy_callback: JsValue| {
                self.0.borrow_mut().surface_copy_callback = surface_copy_callback.dyn_into::<js_sys::Function>().ok();
            };
            |rail_window_callback: JsValue| {
                self.0.borrow_mut().rail_window_callback = rail_window_callback.dyn_into::<js_sys::Function>().ok();
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
            surface_present_callback,
            surface_layout_callback,
            surface_copy_callback,
            rail_window_callback,
            canvas_resized_callback,
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
            surface_present_callback = inner.surface_present_callback.clone();
            surface_layout_callback = inner.surface_layout_callback.clone();
            surface_copy_callback = inner.surface_copy_callback.clone();
            rail_window_callback = inner.rail_window_callback.clone();
            canvas_resized_callback = inner.canvas_resized_callback.clone();
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
        // (0,0) for single-monitor and for any layout whose primary is the top-left monitor.
        let mut rail_desktop_origin: (i32, i32) = (0, 0);
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
                rail_desktop_origin = monitors_desktop_origin(&monitors);
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
        // Shared HiDef-RAIL window-position store: the run loop writes each RAIL window's
        // desktop position (decoded from Window List orders) here, and the graphics handler
        // reads it while compositing every mapped window. The handler is `Send` and is moved
        // into the eGFX DVC processor by `connect()`, so this `Send`+`Clone` handle is the only
        // bridge back to it from the run loop.
        let rail_store = RailWindowStore::default();
        // A registered `surface_present_callback` IS the `?ironwebgl=1` switch: it means JS owns
        // the composite (it draws the AVC video straight into the surface-0 WebGL texture), so the
        // handler must feed it decoded rects + a window layout instead of compositing itself.
        let graphics_handler = Some(WasmGraphicsHandler::new(
            WasmGraphicsMessageProxy::new(input_events_tx.clone()),
            rail_store.clone(),
            surface_present_callback.is_some(),
            rail_desktop_origin,
        ));

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
            rail_store,

            render_canvas,
            set_cursor_style_callback,
            set_cursor_style_callback_context,
            avc_decode_callback,
            avc_watermark_callback,
            canvas_updated_callback,
            surface_present_callback,
            surface_layout_callback,
            surface_copy_callback,
            rail_window_callback,
            canvas_resized_callback,

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
    /// An out-of-band-decoded AVC region (RGBA), returned by the JS WebCodecs decoder for
    /// `surface_id`, already cropped to ONE valid region rect in SURFACE coordinates
    /// (`region.x`/`y`), with the eGFX `frame_id` its pixels belong to. The run loop composites
    /// it into that surface's buffer via the normal codec path so the compositor scales/positions
    /// it (correct for HiDef RAIL windows). This is the CPU-readback fallback's PIXEL-delivery
    /// path only; the FrameAcknowledge for `frame_id` was already sent at decode via
    /// [`RdpInputEvent::AvcAck`].
    AvcRegion {
        surface_id: u16,
        region: GraphicsRegion,
        frame_id: u32,
    },
    /// The WebCodecs decoder produced a frame for eGFX `frame_id`: send its deferred
    /// FrameAcknowledge. Fired at DECODE-completion (not present) so the server is paced to
    /// real decode throughput rather than a present round-trip — the fix for the
    /// two-monitor + AVC stutter. No drawing here; presentation happens independently on the
    /// JS side (GPU direct-draw or the CPU [`RdpInputEvent::AvcRegion`] pixel path).
    AvcAck(u32),
    /// WebGL present (`?ironwebgl=1`): the presentation layout for the frame — a mode
    /// ([`crate::graphics::WEBGL_LAYOUT_CLIP`] and friends) plus the RemoteApp window rects in
    /// desktop coords. The graphics handler is `Send` and has no JS access, so it routes the
    /// layout here; the run loop hands it to the JS renderer, which GPU-clips the presented
    /// surface to those rects. Emitted only when the layout actually changes.
    SurfaceLayout {
        mode: u8,
        /// The eGFX surface's real size — the authoritative dimension for the JS GPU texture.
        surface_w: u32,
        surface_h: u32,
        rects: Vec<(i32, i32, u32, u32)>,
    },
    /// WebGL present: an eGFX `SURFACE_TO_SURFACE` screen-to-screen copy that must be executed on
    /// the GPU, because the pixels it moves live only in the JS surface texture (AVC never reaches
    /// the WASM `SurfaceBuf` on this path). The host uses this to RELOCATE a window instead of
    /// re-encoding it, so getting it wrong duplicates content and paints black.
    SurfaceCopy {
        src_x: u32,
        src_y: u32,
        width: u32,
        height: u32,
        points: Vec<(u32, u32)>,
    },
    /// The current session watermark, forwarded by the graphics handler so the run
    /// loop can re-blend it onto out-of-band AVC regions.
    Watermark(Watermark),
    /// HiDef RAIL (piece 3): the active window-mapped surface being presented changed
    /// size, so the render-canvas backing store must be resized to match. In HiDef RAIL
    /// the app window's graphics live on their OWN eGFX surface (MapSurfaceToWindow) with
    /// NO output/desktop surface, and that surface — usually a different size than the
    /// negotiated desktop — is presented AS the canvas. Emitted by the graphics handler
    /// only when an active window surface exists (never for a normal output-mapped
    /// desktop / legacy-RAIL / multimon session, so the output present path is untouched).
    GraphicsResize {
        width: u32,
        height: u32,
    },
    Resize {
        width: u32,
        height: u32,
        scale_factor: Option<u32>,
        physical_size: Option<(u32, u32)>,
    },
    TerminateSession,
    /// HiDef RAIL local drag: the input path moved the dragged window's position (in
    /// `rail_store`) client-side; ask the run loop to recomposite immediately (no server
    /// round-trip). Carries no data — the new position is already in the shared store.
    RailPresent,
    /// HiDef RAIL local drag finished (mouse-up): send the client `WindowMove` PDU with the
    /// window's final desktop rect (right/bottom exclusive) so the host snaps to it.
    RailWindowMove {
        window_id: u32,
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    },
    /// The server marked a surface capture-protected (proxy `PROTECT_SURFACE`).
    /// A browser cannot enforce capture protection, so the session is refused
    /// fail-closed rather than shown unprotected. See [`PROTECTED_SESSION_REFUSAL`].
    ProtectedSessionRefused,
}

/// User-facing reason shown when a capture-protected session is refused in the
/// browser. Fail-closed: we never display protected content in a client that
/// cannot honor `SetWindowDisplayAffinity`-style screen-capture exclusion.
const PROTECTED_SESSION_REFUSAL: &str = "This session requires screen-capture protection, which isn't available in the browser — please use the native client.";

/// A decoded RGBA region positioned in output (desktop) coordinates.
#[derive(Debug)]
pub(crate) struct GraphicsRegion {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Row-major RGBA8888, `width * height * 4` bytes.
    pub(crate) data: Vec<u8>,
    /// Present with the alpha-preserving path ([`Canvas::draw_preserve_alpha`]) instead of the
    /// default force-opaque [`Canvas::draw`]. Set only for HiDef RAIL window-mapped surfaces,
    /// whose per-pixel alpha (window edges/shadows) must survive to the canvas. `false` for the
    /// desktop/output and AVC paths, keeping them byte-for-byte identical to before.
    pub(crate) preserve_alpha: bool,
}

/// A raw AVC (H.264) main sub-stream awaiting out-of-band (WebCodecs) decode.
///
/// The H.264 picture is coded at the FULL surface size and aligned to the surface origin
/// (0,0); only the `regions` sub-rects carry valid video — the encoder fills the rest with
/// YUV(0,0,0), which decodes to green (BT.601). So the decoder must blit ONLY the region
/// rects, never the whole picture (that green padding is the "green border" artifact).
/// Region coords are therefore in SURFACE space (== source coords in the coded picture),
/// and the decoded RGBA is composited back through the per-surface `SurfaceBuf` path (like
/// every other codec) so it scales/positions correctly for HiDef RAIL windows.
#[derive(Debug)]
pub(crate) struct AvcFrameEvent {
    pub(crate) surface_id: u16,
    /// eGFX frame this picture belongs to; echoed back on present so the run loop can
    /// send the deferred FrameAcknowledge (flow control).
    pub(crate) frame_id: u32,
    /// Surface's output (desktop) origin — only the GPU direct-draw path (multi-monitor,
    /// output-mapped) uses it to place a region; the `SurfaceBuf` path positions via the
    /// compositor and ignores it.
    pub(crate) origin_x: u32,
    pub(crate) origin_y: u32,
    /// Valid sub-rects `(x, y, w, h)` in SURFACE space. Blit only these.
    pub(crate) regions: Vec<(u32, u32, u32, u32)>,
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
    /// Shared HiDef-RAIL window positions, fed to the graphics handler's compositor from
    /// the run loop's Window List order decoding. Inert for non-RAIL / output-mapped sessions.
    rail_store: RailWindowStore,

    render_canvas: HtmlCanvasElement,
    set_cursor_style_callback: js_sys::Function,
    set_cursor_style_callback_context: JsValue,
    /// WebCodecs AVC decode callback; `None` if no browser decoder was registered.
    avc_decode_callback: Option<js_sys::Function>,
    /// AVC watermark callback; forwards the session watermark to the GPU draw path.
    avc_watermark_callback: Option<js_sys::Function>,
    /// Passive render-canvas update notification; `None` if no presenter registered.
    canvas_updated_callback: Option<js_sys::Function>,
    /// Unified WebGL present callback; `None` = classic 2D present. When set, NON-AVC regions are
    /// forwarded to JS (surface-0 WebGL texture) instead of painting the 2D canvas.
    surface_present_callback: Option<js_sys::Function>,
    /// WebGL present layout callback; `None` if the JS renderer registered none. Carries the
    /// present mode + RemoteApp window rects the JS renderer clips the surface-0 texture to.
    surface_layout_callback: Option<js_sys::Function>,
    /// WebGL GPU copy callback; executes eGFX SurfaceToSurface inside the GPU surface texture.
    surface_copy_callback: Option<js_sys::Function>,
    /// RAIL active-window rect notification; `None` if the webapp registered none.
    rail_window_callback: Option<js_sys::Function>,
    /// Render-canvas backing-store resize notification; fired after the run loop resizes the
    /// canvas to a HiDef RAIL window surface so the JS element re-fits it. `None` if none set.
    canvas_resized_callback: Option<js_sys::Function>,

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

    /// Forward a decoded NON-AVC region's RGBA to the JS WebGL renderer (surface-0 `texSubImage2D`),
    /// on the `?ironwebgl=1` path only. Returns `true` if a callback consumed it, so the caller
    /// skips the classic 2D `gui.draw`. The RGBA is presented as-is; the WebGL shader forces alpha
    /// opaque, so no per-pixel alpha fix-up is needed here. Best-effort: a throwing callback is
    /// logged and still returns `true` (never fall back to 2D mid-session).
    fn present_surface_region(&self, x: u32, y: u32, width: u32, height: u32, data: &[u8]) -> bool {
        let Some(cb) = &self.surface_present_callback else {
            return false;
        };
        let args = js_sys::Array::from_iter([
            JsValue::from_f64(f64::from(x)),
            JsValue::from_f64(f64::from(y)),
            JsValue::from_f64(f64::from(width)),
            JsValue::from_f64(f64::from(height)),
            js_sys::Uint8Array::from(data).into(),
        ]);
        if let Err(err) = cb.apply(&JsValue::NULL, &args) {
            warn!(?err, "surface_present callback threw");
        }
        true
    }

    /// Hand the WebGL renderer the presentation layout: `mode` (blank / full-screen / clip — see
    /// [`crate::graphics::WEBGL_LAYOUT_CLIP`]) and the RemoteApp window rects in desktop coords,
    /// flattened as `[x, y, w, h, ...]`. JS clips the presented surface-0 texture to them, which is
    /// what keeps a dragged window from leaving a ghost. Best-effort: a throwing callback is logged
    /// and never influences protocol / frame-ack state.
    fn notify_surface_layout(&self, mode: u8, surface_w: u32, surface_h: u32, rects: &[(i32, i32, u32, u32)]) {
        let Some(cb) = &self.surface_layout_callback else {
            return;
        };
        let flat = js_sys::Int32Array::new_with_length(u32::try_from(rects.len() * 4).unwrap_or(0));
        for (i, &(x, y, w, h)) in rects.iter().enumerate() {
            let base = u32::try_from(i * 4).unwrap_or(0);
            flat.set_index(base, x);
            flat.set_index(base + 1, y);
            flat.set_index(base + 2, i32::try_from(w).unwrap_or(0));
            flat.set_index(base + 3, i32::try_from(h).unwrap_or(0));
        }
        let args = js_sys::Array::from_iter([
            JsValue::from_f64(f64::from(mode)),
            JsValue::from_f64(f64::from(surface_w)),
            JsValue::from_f64(f64::from(surface_h)),
            flat.into(),
        ]);
        if let Err(err) = cb.apply(&JsValue::NULL, &args) {
            warn!(?err, "surface_layout callback threw");
        }
    }

    /// Execute an eGFX SurfaceToSurface copy on the GPU (WebGL path): copy the `w`x`h` block at
    /// (`src_x`,`src_y`) of the surface texture to each destination point. Best-effort.
    fn notify_surface_copy(&self, src_x: u32, src_y: u32, width: u32, height: u32, points: &[(u32, u32)]) {
        let Some(cb) = &self.surface_copy_callback else {
            return;
        };
        let flat = js_sys::Int32Array::new_with_length(u32::try_from(points.len() * 2).unwrap_or(0));
        for (i, &(x, y)) in points.iter().enumerate() {
            let base = u32::try_from(i * 2).unwrap_or(0);
            flat.set_index(base, i32::try_from(x).unwrap_or(0));
            flat.set_index(base + 1, i32::try_from(y).unwrap_or(0));
        }
        let args = js_sys::Array::from_iter([
            JsValue::from_f64(f64::from(src_x)),
            JsValue::from_f64(f64::from(src_y)),
            JsValue::from_f64(f64::from(width)),
            JsValue::from_f64(f64::from(height)),
            flat.into(),
        ]);
        if let Err(err) = cb.apply(&JsValue::NULL, &args) {
            warn!(?err, "surface_copy callback threw");
        }
    }

    /// Notify the JS element that the render-canvas backing store was resized (HiDef RAIL
    /// piece 3), so it re-fits the (already-resized) canvas to the viewport with its existing
    /// single-monitor "fit" logic. The element's registered handler reads the canvas's own
    /// `width`/`height` and takes no arguments, so none are passed. Best-effort: a throwing
    /// callback is logged and ignored and never influences protocol / frame-ack state. No-op
    /// if no callback was registered (the resize still happened; only the re-fit is skipped).
    fn notify_canvas_resized(&self) {
        if let Some(cb) = &self.canvas_resized_callback {
            if let Err(err) = cb.apply(&JsValue::NULL, &js_sys::Array::new()) {
                warn!(?err, "canvas_resized callback threw");
            }
        }
    }

    /// Notify the webapp that the active RAIL (RemoteApp) window's presentation rect
    /// changed to `(x, y, width, height)` in virtual-desktop coordinates, so it can
    /// crop/scale the canvas to just that window. `x`/`y` may be negative in RAIL, so
    /// they are `i32`. Best-effort: a throwing callback is logged and ignored, and it
    /// never influences protocol / frame-ack state. No-op if no callback was registered.
    fn notify_rail_window(&self, x: i32, y: i32, width: u32, height: u32) {
        if let Some(cb) = &self.rail_window_callback {
            let args = js_sys::Array::from_iter([
                JsValue::from_f64(f64::from(x)),
                JsValue::from_f64(f64::from(y)),
                JsValue::from_f64(f64::from(width)),
                JsValue::from_f64(f64::from(height)),
            ]);
            if let Err(err) = cb.apply(&JsValue::NULL, &args) {
                warn!(?err, "rail_window callback threw");
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

        // RAIL (RemoteApp) window tracking. The host paints the whole virtual-desktop
        // surface but sends Window List orders describing each top-level window's
        // geometry; we track the ACTIVE top-level window's rect and report it to the
        // webapp so it can crop/scale to just that window (killing the stale surround).
        // `rail_windows` accumulates each accepted top-level window's optional geometry
        // deltas (Window List updates are partial); `rail_active` is the window whose
        // rect we currently report; `rail_last_reported` dedupes so we fire only on a
        // real change. All inert for a full desktop session (no window orders arrive).
        let mut rail_windows: HashMap<u32, RailWindowGeom> = HashMap::new();
        let mut rail_active: Option<u32> = None;
        let mut rail_last_reported: Option<(i32, i32, u32, u32)> = None;
        // Monotonic z-order counter for HiDef RAIL compositing: each Window List Create/Update
        // stamps the touched window with the next value, so the most-recently-touched window
        // composites on top (a simple most-recent-on-top ordering; MS-RDPERP's Desktop order
        // window_ids list is the exact z-order and could refine this later).
        let mut rail_z_counter: u32 = 0;

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
                            // WebGL path: forward the region to JS (surface-0 texSubImage2D) and skip
                            // the 2D paint entirely. Classic path: paint the 2D canvas. HiDef RAIL
                            // window surfaces preserve per-pixel alpha (window edges/shadows); the
                            // desktop/output path forces opaque as before.
                            if !self.present_surface_region(rx, ry, rw, rh, &data) {
                                let draw_result = if region.preserve_alpha {
                                    gui.draw_preserve_alpha(&data, rect)
                                } else {
                                    gui.draw(&mut data, rect)
                                };
                                if let Err(e) = draw_result {
                                    warn!(error = format!("{e:#}"), "failed to draw EGFX region");
                                }
                            }
                            // Passive: tell an external presenter which area changed.
                            self.notify_canvas_updated(rx, ry, rw, rh);
                            Vec::new()
                        }
                        RdpInputEvent::SurfaceCopy { src_x, src_y, width, height, points } => {
                            // GPU screen-to-screen copy (window relocation). No WASM pixels move.
                            self.notify_surface_copy(src_x, src_y, width, height, &points);
                            Vec::new()
                        }
                        RdpInputEvent::SurfaceLayout { mode, surface_w, surface_h, rects } => {
                            // WebGL present only: forward the RemoteApp window layout to the JS
                            // renderer, which GPU-clips the surface-0 texture to it. No pixels and
                            // no protocol state involved.
                            self.notify_surface_layout(mode, surface_w, surface_h, &rects);
                            Vec::new()
                        }
                        RdpInputEvent::GraphicsResize { width, height } => {
                            // HiDef RAIL (piece 3): the active window-mapped surface presented
                            // AS the canvas changed size, so resize the canvas backing store to
                            // match, then ask the JS element to re-fit it to the viewport. Only
                            // ever reached for a HiDef RAIL session; a normal output-mapped
                            // desktop never emits GraphicsResize.
                            match (NonZeroU32::new(width), NonZeroU32::new(height)) {
                                (Some(w), Some(h)) => {
                                    debug!(width, height, "HiDef RAIL: resizing canvas to window surface");
                                    gui.resize(w, h);
                                    // Re-fit on the JS side (reads the new canvas size); the
                                    // notify_canvas_updated is inert unless a multimon presenter
                                    // registered, but keeps the full-repaint contract symmetric.
                                    self.notify_canvas_resized();
                                    self.notify_canvas_updated(0, 0, width, height);
                                }
                                _ => warn!(width, height, "ignoring HiDef RAIL canvas resize with zero dimension"),
                            }
                            Vec::new()
                        }
                        RdpInputEvent::Watermark(wm) => {
                            // Forward the tile to the GPU AVC draw path so JS can overdraw it on
                            // each frame. The CPU AVC path now composites through the surface
                            // buffer, so its watermark is re-blended by the surface flush/composite
                            // (graphics.rs), not here.
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
                            let _ = wm;
                            Vec::new()
                        }
                        RdpInputEvent::AvcRegion { surface_id, region, frame_id } => {
                            // CPU-readback PIXEL-delivery path: JS read the decoded frame back to
                            // RGBA, cropped to ONE valid region rect in SURFACE coords, and handed
                            // us the pixels. Composite it into that surface's buffer through the
                            // same path as ClearCodec/Planar, then run a present so the compositor
                            // scales/positions it (HiDef RAIL) or flushes it (output-mapped) — this
                            // is what fixes the green border (only region rects are blitted) and the
                            // RAIL offset/scale (video now rides the per-window compositor instead of
                            // being slapped onto the canvas at raw coords). The watermark is
                            // re-blended by the surface flush/composite, so no manual blend here.
                            //
                            // The FrameAcknowledge for `frame_id` was already sent at DECODE
                            // (RdpInputEvent::AvcAck), so we do NOT ack here — flow control is paced
                            // by decode throughput, not this present.
                            let _ = frame_id;
                            let (rx, ry, rw, rh) = (region.x, region.y, region.width, region.height);
                            deliver_avc_region(&mut active_stage, surface_id, rx, ry, rw, rh, region.data);
                            force_gfx_present(&mut active_stage);
                            // Passive: notify an external presenter of the changed area.
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
                            // Hand the compressed AVC main sub-stream to the browser WebCodecs
                            // decoder. It decodes asynchronously and, for each valid region rect
                            // (SURFACE coords), returns cropped RGBA via the `on_avc_decoded`
                            // extension, which re-enters the loop as an `RdpInputEvent::AvcRegion`.
                            // `regions` is a flat [x,y,w,h, x,y,w,h, ...] Uint32Array so only the
                            // valid sub-rects are blitted (never the green YUV(0,0,0) padding).
                            if let Some(cb) = &self.avc_decode_callback {
                                let data = js_sys::Uint8Array::from(frame.main_stream.as_slice());
                                let mut flat: Vec<u32> = Vec::with_capacity(frame.regions.len() * 4);
                                for (x, y, w, h) in &frame.regions {
                                    flat.extend_from_slice(&[*x, *y, *w, *h]);
                                }
                                let regions = js_sys::Uint32Array::from(flat.as_slice());
                                let args = js_sys::Array::from_iter([
                                    JsValue::from_f64(f64::from(frame.surface_id)),
                                    JsValue::from_f64(f64::from(frame.frame_id)),
                                    JsValue::from_f64(f64::from(frame.origin_x)),
                                    JsValue::from_f64(f64::from(frame.origin_y)),
                                    regions.into(),
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
                        RdpInputEvent::RailPresent => {
                            // The input path moved the dragged window locally; recomposite now.
                            force_gfx_present(&mut active_stage);
                            Vec::new()
                        }
                        RdpInputEvent::RailWindowMove { window_id, left, top, right, bottom } => {
                            // Local drag ended: report the final rect so the host snaps to it.
                            let msg = active_stage
                                .get_svc_processor::<RailChannel>()
                                .map(|r| r.window_move(window_id, left, top, right, bottom));
                            let mut outs = Vec::new();
                            if let Some(msg) = msg {
                                match active_stage.process_svc_processor_messages(
                                    SvcProcessorMessages::<RailChannel>::from(vec![msg]),
                                ) {
                                    Ok(frame) if !frame.is_empty() => {
                                        debug!(target: "rail_diag", window_id = format!("{window_id:#x}"), left, top, right, bottom, "RAIL: sent client WindowMove");
                                        outs.push(ActiveStageOutput::ResponseFrame(frame));
                                    }
                                    Ok(_) => {}
                                    Err(e) => warn!(error = %e, "RAIL WindowMove send failed"),
                                }
                            }
                            force_gfx_present(&mut active_stage);
                            outs
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
            // Tracks whether any RAIL order this pass changed a window position, so we can
            // force one compositing pass afterwards (positions arrive on a different channel
            // than — and usually after — the eGFX EndFrame that painted the window).
            let mut rail_position_changed = false;
            // While a window is being dragged LOCALLY, ignore the host's Window-List position
            // updates for it — they lag the local drag and would snap it backward each frame. The
            // host confirms the final position with an UPDATE after our WindowMove, and the drag
            // is cleared by then, so that authoritative snap still applies.
            let dragging_window = self.rail_store.local_drag().map(|d| d.window_id);
            for orders in active_stage.take_rail_orders() {
                for order in WindowOrder::decode_orders_update(&orders) {
                    log_rail_window_order(&order);
                    // Apply the window geometry: track the active top-level window and,
                    // when its presentation rect changes, tell the webapp to crop to it.
                    match &order {
                        WindowOrder::CreateWindow { window_id, state } => {
                            // Track EVERY window's geometry (a later Update can flip
                            // presentability), but only FEED the compositor the ones that are
                            // real, presentable RemoteApp windows. WS_EX_NOACTIVATE chrome
                            // (shadows / drag markers / slivers) and the 0x0 phantom are dropped
                            // so they neither ghost the canvas nor sit topmost eating clicks.
                            let geom = rail_windows.entry(*window_id).or_default();
                            geom.merge(state);
                            if geom.is_presentable() {
                                rail_active = Some(*window_id);
                                // Feed this window's desktop rect (position + size) to the
                                // compositor. Prefer windowOffset/windowSize (full frame incl.
                                // border/title), else visibleOffset/clientAreaSize. Widen the u32
                                // windowId to u64. Skip while it's being locally dragged.
                                if let Some((wx, wy)) = geom.window_offset.or(geom.visible_offset) {
                                    if dragging_window != Some(u64::from(*window_id)) {
                                        let (ww, wh) = geom.window_size.or(geom.client_area_size).unwrap_or((0, 0));
                                        rail_z_counter = rail_z_counter.wrapping_add(1);
                                        self.rail_store.set_rail_window(
                                            u64::from(*window_id),
                                            wx,
                                            wy,
                                            u32::try_from(ww).unwrap_or(0),
                                            u32::try_from(wh).unwrap_or(0),
                                            rail_z_counter,
                                        );
                                        rail_position_changed = true;
                                    }
                                }
                            } else {
                                // Chrome / phantom: keep it out of the compositor.
                                self.rail_store.remove_rail_window(u64::from(*window_id));
                                rail_position_changed = true;
                            }
                        }
                        WindowOrder::UpdateWindow { window_id, state } => {
                            if let Some(geom) = rail_windows.get_mut(window_id) {
                                geom.merge(state);
                                if geom.is_presentable() {
                                    rail_active = Some(*window_id);
                                    if let Some((wx, wy)) = geom.window_offset.or(geom.visible_offset) {
                                        if dragging_window != Some(u64::from(*window_id)) {
                                            let (ww, wh) = geom.window_size.or(geom.client_area_size).unwrap_or((0, 0));
                                            rail_z_counter = rail_z_counter.wrapping_add(1);
                                            self.rail_store.set_rail_window(
                                                u64::from(*window_id),
                                                wx,
                                                wy,
                                                u32::try_from(ww).unwrap_or(0),
                                                u32::try_from(wh).unwrap_or(0),
                                                rail_z_counter,
                                            );
                                            rail_position_changed = true;
                                        }
                                    }
                                } else {
                                    // Became (or stayed) non-presentable: drop from compositor.
                                    self.rail_store.remove_rail_window(u64::from(*window_id));
                                    rail_position_changed = true;
                                }
                            }
                        }
                        WindowOrder::DeleteWindow { window_id } => {
                            rail_windows.remove(window_id);
                            // HiDef RAIL: drop the window from the compositor too.
                            self.rail_store.remove_rail_window(u64::from(*window_id));
                            rail_position_changed = true;
                            // If the active window went away, stop reporting but keep the
                            // last rect shown (the app may be closing the whole session).
                            if rail_active == Some(*window_id) {
                                rail_active = None;
                            }
                        }
                        WindowOrder::Desktop { non_monitored, .. } => {
                            // Non-Monitored Desktop = the input desktop switched to one RAIL isn't
                            // tracking (Ctrl+Alt+Del / lock / UAC secure desktop). The host paints
                            // it full-screen on the primary surface with no RAIL window, so tell the
                            // compositor to present surface 0 full-screen instead of clipping to the
                            // stale app-window rects. An Actively Monitored Desktop order clears it.
                            self.rail_store.set_secure_desktop(*non_monitored);
                            rail_position_changed = true;
                        }
                        _ => {}
                    }

                    // Recompute the active window's rect; fire only on a real change.
                    if let Some(active_id) = rail_active {
                        if let Some(rect) = rail_windows.get(&active_id).and_then(RailWindowGeom::rect) {
                            if rail_last_reported != Some(rect) {
                                rail_last_reported = Some(rect);
                                let (x, y, w, h) = rect;
                                debug!(
                                    window_id = format!("{active_id:#x}"),
                                    x, y, w, h, "RAIL: active window rect"
                                );
                                self.notify_rail_window(x, y, w, h);
                            }
                        }
                    }
                }
            }

            // HiDef RAIL: a window's position arrives on the RAIL channel independently of the
            // eGFX EndFrame that painted it. The compositor only runs on EndFrame, so a window
            // whose position lands after its last paint would freeze on the stale desktop frame.
            // After applying this pass's RAIL orders, force one compositing pass so any
            // newly-positioned window is placed immediately. No-op for non-HiDef sessions.
            if rail_position_changed {
                force_gfx_present(&mut active_stage);
            }

            // HiDef RAIL local move/size: the RAIL SVC processor buffered any ServerLocalMoveSize
            // PDUs. Begin a client-side local drag on a MOVE start (the input path then follows
            // the cursor without server round-trips); on end, clear residual state. Resize
            // move-size types stay server-driven.
            let move_events = active_stage
                .get_svc_processor_mut::<RailChannel>()
                .map(|c| c.take_move_size_events())
                .unwrap_or_default();
            // Path A (MS-style non-HiDef RAIL): window moves are SERVER-DRIVEN. The host moves
            // the window on the one desktop surface and re-renders it; we just forward the mouse
            // (like the shipping prod client / Microsoft's web client). We must NOT begin a
            // client-side local drag here — that was a HiDef-only optimization (smooth per-window-
            // surface drag), and under Path A it is actively harmful: it suppresses the mouse
            // forwarding the host needs while our compositor draws the raw desktop surface and
            // never moves the window locally, so the window wouldn't move at all. Flip this to
            // true only if per-window HiDef compositing is ever re-enabled.
            const ENABLE_CLIENT_LOCAL_DRAG: bool = false;
            for ms in move_events {
                if ENABLE_CLIENT_LOCAL_DRAG && ms.is_move_size_start && ms.move_size_type == RAIL_WMSZ_MOVE {
                    match rail_windows
                        .get(&ms.window_id)
                        .and_then(|g| g.window_size.or(g.client_area_size))
                    {
                        Some((w, h)) => {
                            self.rail_store.begin_local_drag(LocalDrag {
                                window_id: u64::from(ms.window_id),
                                anchor_x: i32::from(ms.pos_x),
                                anchor_y: i32::from(ms.pos_y),
                                width: w,
                                height: h,
                            });
                            debug!(target: "rail_diag", window_id = format!("{:#x}", ms.window_id), anchor_x = ms.pos_x, anchor_y = ms.pos_y, w, h, "RAIL: begin local move");
                        }
                        None => {
                            debug!(target: "rail_diag", window_id = format!("{:#x}", ms.window_id), "RAIL: local move START but no tracked window size -> cannot drag")
                        }
                    }
                } else if ms.is_move_size_start {
                    // Report the ACTUAL reason. This used to say "is a RESIZE (not MOVE)", which is
                    // wrong whenever `ENABLE_CLIENT_LOCAL_DRAG` is false: the const short-circuits
                    // the branch above, so genuine MOVEs (RAIL_WMSZ_MOVE = 0x0009) land here too and
                    // got reported as resizes. That message cost a downstream investigation, which
                    // concluded from it that the MOVE constant was mismapped — it is not.
                    let is_move = ms.move_size_type == RAIL_WMSZ_MOVE;
                    debug!(
                        target: "rail_diag",
                        move_size_type = ms.move_size_type,
                        is_move,
                        local_drag_enabled = ENABLE_CLIENT_LOCAL_DRAG,
                        "RAIL: move/size START left server-driven"
                    );
                } else if !ms.is_move_size_start {
                    // Host END. Normally the client mouse-up already ended the drag (and sent the
                    // WindowMove), so this returns None and is a no-op. If it's still active (the
                    // client missed the mouse-up, e.g. the cursor left the canvas), report the
                    // final dragged position now so the window snaps there instead of back.
                    if let Some(drag) = self.rail_store.end_local_drag() {
                        if let Some((fx, fy)) = self.rail_store.window_pos(drag.window_id) {
                            let _ = self.input_events_tx.unbounded_send(RdpInputEvent::RailWindowMove {
                                window_id: u32::try_from(drag.window_id).unwrap_or(0),
                                left: i16::try_from(fx).unwrap_or(0),
                                top: i16::try_from(fy).unwrap_or(0),
                                right: i16::try_from(fx.saturating_add(drag.width)).unwrap_or(i16::MAX),
                                bottom: i16::try_from(fy.saturating_add(drag.height)).unwrap_or(i16::MAX),
                            });
                        }
                    }
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
        // Mouse coords arrive in canvas pixels; `bbox_origin` maps them to desktop-absolute
        // (0,0 in desktop mode / non-RAIL).
        let (ox, oy) = self.rail_store.bbox_origin();

        // HiDef RAIL local move: the host handed us the drag loop (ServerLocalMoveSize START).
        // Drive the dragged window's position CLIENT-SIDE from the cursor and do NOT forward the
        // mouse to the host; on release, send one WindowMove with the final rect so the host
        // snaps. This turns the laggy per-frame server round-trip into a smooth local drag.
        if let Some(drag) = self.rail_store.local_drag() {
            for op in transaction {
                match op {
                    ironrdp::input::Operation::MouseMove(p) => {
                        let nx = i32::from(p.x) + ox - drag.anchor_x;
                        let ny = i32::from(p.y) + oy - drag.anchor_y;
                        self.rail_store.move_rail_window(drag.window_id, nx, ny);
                        let _ = self.input_events_tx.unbounded_send(RdpInputEvent::RailPresent);
                    }
                    ironrdp::input::Operation::MouseButtonReleased(ironrdp::input::MouseButton::Left) => {
                        let (fx, fy) = self.rail_store.window_pos(drag.window_id).unwrap_or((0, 0));
                        self.rail_store.end_local_drag();
                        let _ = self.input_events_tx.unbounded_send(RdpInputEvent::RailWindowMove {
                            window_id: u32::try_from(drag.window_id).unwrap_or(0),
                            left: i16::try_from(fx).unwrap_or(0),
                            top: i16::try_from(fy).unwrap_or(0),
                            right: i16::try_from(fx.saturating_add(drag.width)).unwrap_or(i16::MAX),
                            bottom: i16::try_from(fy.saturating_add(drag.height)).unwrap_or(i16::MAX),
                        });
                        // The pre-drag mouse-DOWN went through the input DB (button marked held)
                        // but we consumed the mouse-up here without it, so clear the DB's held
                        // state — otherwise the next click sees Left already down. Nothing is sent
                        // to the host (it owned the move loop and expects only the WindowMove).
                        let _ = self.input_database.borrow_mut().release_all();
                    }
                    // Ignore all other input while a local move is in progress.
                    _ => {}
                }
            }
            return Ok(());
        }

        // HiDef RAIL: the canvas is the composited window bounding-box sub-region, so the
        // browser reports mouse coordinates relative to the bbox — but the host expects
        // DESKTOP-absolute coordinates. The compositor draws each window at `canvas = desktop -
        // bbox_origin`; the inverse must be applied to outgoing pointer coords or clicks land at
        // the wrong desktop point (and miss any window not at desktop (0,0)). `bbox_origin` is
        // (0,0) for normal output-mapped / non-RAIL sessions, so this is a no-op there.
        let inputs = if ox != 0 || oy != 0 {
            let shifted = transaction.into_iter().map(|op| match op {
                ironrdp::input::Operation::MouseMove(p) => {
                    ironrdp::input::Operation::MouseMove(ironrdp::input::MousePosition {
                        x: u16::try_from((i32::from(p.x) + ox).clamp(0, i32::from(u16::MAX))).unwrap_or(u16::MAX),
                        y: u16::try_from((i32::from(p.y) + oy).clamp(0, i32::from(u16::MAX))).unwrap_or(u16::MAX),
                    })
                }
                other => other,
            });
            self.input_database.borrow_mut().apply(shifted)
        } else {
            self.input_database.borrow_mut().apply(transaction)
        };
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
            // Return path for out-of-band AVC decode: JS hands back RGBA for ONE region rect it
            // decoded via WebCodecs, in SURFACE coordinates. Re-enters the loop as an
            // `RdpInputEvent::AvcRegion`, which composites it into `surface_id`'s buffer through
            // the normal per-surface path (so the compositor scales/positions it for HiDef RAIL).
            |on_avc_decoded: JsValue| {
                let obj = into_object(on_avc_decoded)?;
                let surface_id = get_u32(&obj, "surfaceId")? as u16;
                let frame_id = get_u32(&obj, "frameId")?;
                let x = get_u32(&obj, "x")?;
                let y = get_u32(&obj, "y")?;
                let width = get_u32(&obj, "width")?;
                let height = get_u32(&obj, "height")?;
                let data_val = js_sys::Reflect::get(&obj, &JsValue::from_str("data"))
                    .map_err(|e| IronError::from(anyhow::anyhow!("get property `data`: {e:?}")))?;
                let data = js_sys::Uint8Array::new(&data_val).to_vec();

                self.input_events_tx
                    .unbounded_send(RdpInputEvent::AvcRegion {
                        surface_id,
                        region: GraphicsRegion { x, y, width, height, data, preserve_alpha: false },
                        frame_id,
                    })
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

/// Force the eGFX graphics handler to run a compositing pass now, outside a server
/// `EndFrame`. Called after RAIL Window List orders change a window position so a HiDef
/// RemoteApp window whose position arrived after its last paint is composited immediately
/// (see [`GraphicsPipelineClient::present_now`]). Reaches the client through DRDYNVC, the
/// only exposed mutable path. Best-effort: silently no-ops if the graphics channel is down.
fn force_gfx_present(active_stage: &mut ActiveStage) {
    let Some(gfx_channel_id) = active_stage
        .get_dvc::<GraphicsPipelineClient>()
        .map(|dvc| dvc.channel_id())
    else {
        return;
    };
    let Some(drdynvc) = active_stage.get_svc_processor_mut::<DrdynvcClient>() else {
        return;
    };
    let Some(mut chan) = drdynvc.get_dvc_by_channel_id_mut::<GraphicsPipelineClient>(gfx_channel_id) else {
        return;
    };
    chan.processor_mut().present_now();
}

/// Composite an out-of-band-decoded AVC region (RGBA, already cropped to one valid region rect
/// in SURFACE coords) into its surface via the eGFX graphics handler, exactly like a
/// synchronously-decoded codec region. The caller runs [`force_gfx_present`] afterwards to flush
/// it. Reaches the client through DRDYNVC (the only exposed mutable path); no-ops if the graphics
/// channel is down.
fn deliver_avc_region(
    active_stage: &mut ActiveStage,
    surface_id: u16,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    data: Vec<u8>,
) {
    let Some(gfx_channel_id) = active_stage
        .get_dvc::<GraphicsPipelineClient>()
        .map(|dvc| dvc.channel_id())
    else {
        return;
    };
    let Some(drdynvc) = active_stage.get_svc_processor_mut::<DrdynvcClient>() else {
        return;
    };
    let Some(mut chan) = drdynvc.get_dvc_by_channel_id_mut::<GraphicsPipelineClient>(gfx_channel_id) else {
        return;
    };
    chan.processor_mut()
        .deliver_avc_region(surface_id, x, y, width, height, data);
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
            warn!(
                width,
                height, "`monitors` extension: skipping monitor with non-positive size"
            );
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

/// Translation from the host's DESKTOP coordinate space into eGFX OUTPUT (framebuffer) space.
///
/// Two different spaces, and RAIL vs eGFX each use a different one:
///   * GCC Client Monitor Data -- and therefore every RAIL Window-List rect the host sends -- is
///     PRIMARY-RELATIVE: the primary monitor is at (0,0) and a monitor to its left/above has a
///     NEGATIVE origin. (MS-RDPBCGR requires this; FreeRDP rejects a layout whose primary isn't
///     at 0/0.)
///   * The eGFX framebuffer is BOUNDING-BOX space: its top-left is (0,0), which is where each
///     surface's `MapSurfaceToOutput` origin lives.
///
/// The two coincide only when the primary monitor IS the top-left one -- always true for a single
/// monitor, which is why clipping RAIL rects as if they were output coords worked until now.
/// Otherwise every RAIL rect is off by the bounding-box minimum.
fn monitors_desktop_origin(monitors: &[GccMonitor]) -> (i32, i32) {
    let min_left = monitors.iter().map(|m| m.left).min().unwrap_or(0);
    let min_top = monitors.iter().map(|m| m.top).min().unwrap_or(0);
    (-i32::from(min_left), -i32::from(min_top))
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
/// `WS_EX_NOACTIVATE` (win32 extended window style): a window that must not be activated —
/// used by Windows for every non-interactive RAIL artifact (drop shadows, drag markers,
/// slivers) and the always-topmost 0x0 phantom. It is the single bit that cleanly separates
/// real RemoteApp windows and interactive popups (dropdowns are `WS_EX_TOOLWINDOW` but NOT
/// NOACTIVATE) from that chrome. Presentation and the RAIL-position feed both gate on it (per
/// the proxy's deterministic ghost/phantom trace), so shadows/markers/phantoms are neither
/// composited nor allowed to sit topmost eating clicks.
const WS_EX_NOACTIVATE: u32 = 0x0800_0000;

/// `WS_POPUP` + `WS_EX_TOOLWINDOW`: the full-desktop shell window and the 0×0 helper chrome are
/// uniquely this pair (popup, tool-window, no caption); a real app window — even maximized — is
/// `WS_CAPTION`/overlapped (neither bit). Dropping this class filters the shell WITHOUT a size
/// heuristic, so a legitimately maximized app is still presented (per the proxy's style trace).
/// (Tooltips/menus are popup+toolwindow too — fine to drop as transient chrome.)
const WS_POPUP: u32 = 0x8000_0000;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;

/// Accumulated geometry of one RAIL window, merged across partial Window List updates (each
/// order carries only the fields whose bit is set). Coordinates are virtual-desktop pixels and
/// may be negative in RAIL, hence `i32`.
#[derive(Default, Clone, Copy)]
struct RailWindowGeom {
    /// `windowOffset`: top-left of the whole window (incl. non-client frame).
    window_offset: Option<(i32, i32)>,
    /// `windowSize`: full window size (incl. frame).
    window_size: Option<(i32, i32)>,
    /// `visibleOffset`: top-left of the visible (on-desktop) region — preferred origin.
    visible_offset: Option<(i32, i32)>,
    /// `clientAreaSize`: client-area size — size fallback when `windowSize` is absent.
    client_area_size: Option<(i32, i32)>,
    /// `extendedStyle`: win32 `WS_EX_*` bits — checked for `WS_EX_NOACTIVATE` to drop chrome.
    extended_style: Option<u32>,
    /// `style`: win32 `WS_*` bits — checked for `WS_POPUP` (shell/helper class). NOT gated on
    /// `WS_VISIBLE`: the wire toggles it as noise on live windows (see `is_presentable`).
    style: Option<u32>,
}

impl RailWindowGeom {
    /// Merge the present fields of a Window List order onto the tracked geometry.
    /// Absent fields (partial update) leave the previously-tracked value intact.
    fn merge(&mut self, state: &WindowState) {
        if let Some(p) = state.window_offset {
            self.window_offset = Some((p.x, p.y));
        }
        if let Some(s) = state.window_size {
            self.window_size = Some((s.width, s.height));
        }
        if let Some(p) = state.visible_offset {
            self.visible_offset = Some((p.x, p.y));
        }
        if let Some(s) = state.client_area_size {
            self.client_area_size = Some((s.width, s.height));
        }
        if let Some(ex) = state.extended_style {
            self.extended_style = Some(ex);
        }
        if let Some(st) = state.style {
            self.style = Some(st);
        }
    }

    /// Whether this is a real, presentable RemoteApp window (vs. non-interactive chrome or the
    /// 0x0 phantom): it must have a non-zero size AND not be `WS_EX_NOACTIVATE`. Interactive
    /// tool-window popups (e.g. dropdowns) are NOT NOACTIVATE, so they pass. Both the compositor
    /// feed and (future) hit-testing gate on this — the proxy's deterministic ghost filter.
    fn is_presentable(&self) -> bool {
        // DELIBERATELY does NOT gate on WS_VISIBLE or showState. The proxy proved on the wire
        // (session 73890406) that a live RemoteApp window's WS_VISIBLE toggles constantly as noise
        // (0x14cf0000 <-> 0x000b0000, decoupled from any real state change — no size change, no
        // desktop order nearby) and that showState is equally unreliable. Honoring either makes the
        // window blink out of the presentable set mid-session (and, with the full-screen
        // secure-desktop present, false-fire to a full desktop with no CAD at all). The two events
        // that actually matter are handled elsewhere: DeleteWindow removes a window, and the RAIL
        // Non-Monitored Desktop order (DESKTOP_NONE) is the ONLY unambiguous Ctrl+Alt+Del / lock /
        // UAC signal (see the compositor's secure-desktop path). Here we classify STRUCTURE only.
        //
        // Full-desktop shell + 0×0 helpers are uniquely WS_POPUP && WS_EX_TOOLWINDOW; drop that
        // class (a maximized app is WS_CAPTION/overlapped, so it stays). No size heuristic needed.
        let popup = self.style.is_some_and(|s| s & WS_POPUP != 0);
        let toolwin = self.extended_style.is_some_and(|ex| ex & WS_EX_TOOLWINDOW != 0);
        if popup && toolwin {
            return false;
        }
        if self.extended_style.is_some_and(|ex| ex & WS_EX_NOACTIVATE != 0) {
            return false;
        }
        matches!(self.window_size.or(self.client_area_size), Some((w, h)) if w > 0 && h > 0)
    }

    /// Compute the presentation rect `(x, y, width, height)` for the crop. Prefers
    /// `window_offset` (the full window frame, which is what `window_size` measures) and
    /// falls back to `visible_offset` only when no window offset has ever arrived. Returns
    /// `None` until a usable origin and a non-degenerate size are both known.
    ///
    /// The preference order matches the live Path A compositor. It used to be inverted here,
    /// which mattered because `visible_offset` was mis-parsed for any order carrying
    /// `WND_RECTS` (see `orders.rs`) -- this helper reached for the corrupt value first.
    fn rect(&self) -> Option<(i32, i32, u32, u32)> {
        let (ox, oy) = self.window_offset.or(self.visible_offset)?;
        let (sw, sh) = self.window_size.or(self.client_area_size)?;
        if sw <= 0 || sh <= 0 {
            return None;
        }
        #[expect(clippy::cast_sign_loss, reason = "sw/sh are > 0 per the guard above")]
        Some((ox, oy, sw as u32, sh as u32))
    }
}

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
            non_monitored,
            active_window_id,
            window_ids,
        } => {
            debug!(
                target: "rail_diag",
                non_monitored,
                active = ?active_window_id,
                count = window_ids.len(),
                "RAIL: desktop order (non_monitored=secure desktop / CAD)"
            );
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
            //
            // Registered as a re-createable LISTENER, not a one-shot channel:
            // Windows/AVD open AUDIO_PLAYBACK_DVC, close it during early-session
            // renegotiation (~1s in), then re-open it. A one-shot registration is
            // consumed on the first open and then NO_LISTENER-rejects (0xC0000001)
            // every reopen, so audio never plays. The listener rebuilds a fresh
            // RdpsndDvcClient per create, each cloning the shared sink (same mpsc
            // sender / JS callback), so playback resumes after the reopen.
            drdynvc = drdynvc.with_listener(RdpsndDvcListener::new(move || {
                Box::new(dvc_sound_backend.clone()) as Box<dyn RdpsndClientHandler>
            }));
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
