//! RD Gateway transport: [MS-TSGU] over a WebSocket, for gateways that admit a client by an
//! access token (PAA cookie) and leave RDP's own TLS to the client, such as the diode front.
//!
//! Unlike RDCleanPath, the RDP inside the tunnel is encrypted end to end, from this client to the
//! RDP server: the gateway, and the web server in front of it, relay only ciphertext.

use core::pin::Pin;
use core::task::{Context, Poll, ready};
use std::io;
use std::sync::Arc;

use anyhow::Context as _;
use futures_util::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use gloo_net::websocket::futures::WebSocket;
// Its `web` feature gives rustls a clock in the browser, for certificate expiry.
use rustls_pki_types as _;
use tracing::{debug, info, warn};

/// What RDP runs over once connected: the proxy's WebSocket, or a gateway tunnel with TLS inside.
pub(crate) trait RdpIo: AsyncRead + AsyncWrite + Unpin {}

impl<T: AsyncRead + AsyncWrite + Unpin + ?Sized> RdpIo for T {}

pub(crate) type RdpStream = Box<dyn RdpIo>;

// [MS-TSGU] 2.2.10 packet types
const HANDSHAKE_REQUEST: u16 = 0x1;
const HANDSHAKE_RESPONSE: u16 = 0x2;
const TUNNEL_CREATE: u16 = 0x4;
const TUNNEL_RESPONSE: u16 = 0x5;
const TUNNEL_AUTH: u16 = 0x6;
const TUNNEL_AUTH_RESPONSE: u16 = 0x7;
const CHANNEL_CREATE: u16 = 0x8;
const CHANNEL_RESPONSE: u16 = 0x9;
const DATA: u16 = 0xA;
const SERVICE_MESSAGE: u16 = 0xB;
const KEEPALIVE: u16 = 0xD;
const CLOSE_CHANNEL: u16 = 0x10;
const CLOSE_CHANNEL_RESPONSE: u16 = 0x11;

const HEADER: usize = 8;
const EXT_AUTH_PAA: u16 = 0x2;
const TUNNEL_FIELD_PAA_COOKIE: u16 = 0x1;
/// HTTP_CAPABILITY_MESSAGING_SERVICE_MSG: the gateway may tell us why it refused the logon.
const CAPABILITY_SERVICE_MESSAGE: u32 = 0x8;
const PROTOCOL_RDP: u16 = 3;
/// E_PROXY_COOKIE_AUTHENTICATION_ACCESS_DENIED
const COOKIE_DENIED: u32 = 0x8007_59F8;

/// RDP bytes per DATA packet: a Windows gateway relays TLS records, at most about 16 KiB, and
/// mstsc refuses 32 KiB packets; the diode front caps a client frame at 64 KiB.
const DATA_CHUNK: usize = 8 * 1024;
/// The largest packet taken from the gateway.
const MAX_PACKET: usize = 1024 * 1024;

fn packet(kind: u16, body: &[u8]) -> io::Result<Vec<u8>> {
    let length = u32::try_from(HEADER + body.len()).map_err(|_| io::Error::other("packet too long"))?;
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// UTF-16LE, with a terminating NUL when `nul`.
fn utf16(text: &str, nul: bool) -> Vec<u8> {
    text.encode_utf16()
        .chain(nul.then_some(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// A length-prefixed UTF-16LE string (HTTP_UNICODE_STRING / HTTP_BYTE_BLOB).
fn put_string(out: &mut Vec<u8>, text: &str, nul: bool) -> io::Result<()> {
    let bytes = utf16(text, nul);
    let length = u16::try_from(bytes.len()).map_err(|_| io::Error::other("string too long"))?;
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&bytes);
    Ok(())
}

fn u16_at(body: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(body.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(body: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(body.get(at..at + 4)?.try_into().ok()?))
}

/// The text of a SERVICE_MESSAGE body: a length-prefixed UTF-16LE string, maybe NUL-terminated.
fn service_message_text(body: &[u8]) -> String {
    let length = usize::from(u16_at(body, 0).unwrap_or(0));
    let units: Vec<u16> = body
        .get(2..2 + length)
        .unwrap_or_default()
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Splits one whole packet off the front of `buf`: its type and body, or `None` until more bytes come.
fn take_packet(buf: &mut Vec<u8>) -> io::Result<Option<(u16, Vec<u8>)>> {
    let (Some(kind), Some(length)) = (u16_at(buf, 0), u32_at(buf, 4)) else {
        return Ok(None);
    };
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    if !(HEADER..=MAX_PACKET).contains(&length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("gateway packet of {length} bytes"),
        ));
    }
    if buf.len() < length {
        return Ok(None);
    }
    let body = buf.get(HEADER..length).unwrap_or_default().to_vec();
    buf.drain(..length);
    Ok(Some((kind, body)))
}

/// An open gateway channel: reads and writes carry the RDP byte stream, wrapped in DATA packets.
pub(crate) struct Tunnel {
    ws: WebSocket,
    /// Bytes from the gateway not yet split into packets.
    incoming: Vec<u8>,
    /// RDP bytes from the last DATA packet not yet read.
    data: Vec<u8>,
    data_read: usize,
    /// A packet to the gateway not yet fully written.
    outgoing: Vec<u8>,
    outgoing_written: usize,
    /// The gateway's last service message: why it refused or ended the session.
    message: Option<String>,
    /// The page's handler for the gateway's messages, if it gave one.
    on_message: Option<js_sys::Function>,
    closed: bool,
    /// CLOSE_CHANNEL was queued: the gateway ends the RDP session at once.
    close_sent: bool,
}

impl Tunnel {
    /// Runs the gateway handshake on an open WebSocket: the access token admits the client, and
    /// the channel then carries RDP to `target` (which a token-routed gateway may ignore).
    pub(crate) async fn open(
        ws: WebSocket,
        access_token: &str,
        target: &str,
        client_name: &str,
        on_message: Option<js_sys::Function>,
    ) -> anyhow::Result<Self> {
        let mut tunnel = Self {
            ws,
            incoming: Vec::new(),
            data: Vec::new(),
            data_read: 0,
            outgoing: Vec::new(),
            outgoing_written: 0,
            message: None,
            on_message,
            closed: false,
            close_sent: false,
        };

        // version 1.0, client version 0, the access-token (PAA) extended authentication
        let mut body = vec![1, 0, 0, 0];
        body.extend_from_slice(&EXT_AUTH_PAA.to_le_bytes());
        tunnel.send(HANDSHAKE_REQUEST, &body).await?;
        let response = tunnel.expect(HANDSHAKE_RESPONSE).await?;
        tunnel.check(u32_at(&response, 0), "handshake")?;

        let mut body = Vec::new();
        body.extend_from_slice(&CAPABILITY_SERVICE_MESSAGE.to_le_bytes());
        body.extend_from_slice(&TUNNEL_FIELD_PAA_COOKIE.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        put_string(&mut body, access_token, false)?;
        tunnel.send(TUNNEL_CREATE, &body).await?;
        let response = tunnel.expect(TUNNEL_RESPONSE).await?;
        tunnel.check(u32_at(&response, 2), "tunnel")?;

        let mut body = 0u16.to_le_bytes().to_vec();
        put_string(&mut body, client_name, true)?;
        tunnel.send(TUNNEL_AUTH, &body).await?;
        let response = tunnel.expect(TUNNEL_AUTH_RESPONSE).await?;
        tunnel.check(u32_at(&response, 0), "tunnel authorization")?;

        let (host, port) = split_host_port(target);
        let mut body = vec![1, 0]; // one resource, no alternatives
        body.extend_from_slice(&port.to_le_bytes());
        body.extend_from_slice(&PROTOCOL_RDP.to_le_bytes());
        put_string(&mut body, host, true)?;
        tunnel.send(CHANNEL_CREATE, &body).await?;
        let response = tunnel.expect(CHANNEL_RESPONSE).await?;
        tunnel.check(u32_at(&response, 0), "channel")?;

        info!("RD Gateway channel open");
        Ok(tunnel)
    }

    /// A SERVICE_MESSAGE: kept for the error a refusal ends in, and passed to the page.
    fn take_message(&mut self, body: &[u8]) {
        let text = service_message_text(body);
        info!(%text, "RD Gateway message");
        if let Some(on_message) = &self.on_message {
            if let Err(err) = on_message.call1(&wasm_bindgen::JsValue::NULL, &wasm_bindgen::JsValue::from_str(&text)) {
                warn!(?err, "gateway_message_callback threw");
            }
        }
        self.message = Some(text);
    }

    fn check(&self, code: Option<u32>, step: &str) -> anyhow::Result<()> {
        match code {
            Some(0) => Ok(()),
            Some(code) => {
                let reason = if code == COOKIE_DENIED {
                    "the gateway refused the access token (expired, already used, or issued for another address)"
                        .to_owned()
                } else {
                    format!("the gateway refused the {step} (0x{code:08X})")
                };
                Err(match &self.message {
                    Some(message) => anyhow::anyhow!("{reason}: {message}"),
                    None => anyhow::anyhow!(reason),
                })
            }
            None => anyhow::bail!("short gateway {step} response"),
        }
    }

    async fn send(&mut self, kind: u16, body: &[u8]) -> anyhow::Result<()> {
        self.outgoing = packet(kind, body)?;
        self.outgoing_written = 0;
        core::future::poll_fn(|cx| self.poll_drain(cx)).await?;
        self.ws.flush().await?;
        Ok(())
    }

    /// The next packet of type `kind`; service messages and keepalives on the way are taken in.
    async fn expect(&mut self, kind: u16) -> anyhow::Result<Vec<u8>> {
        loop {
            if let Some((got, body)) = take_packet(&mut self.incoming)? {
                match got {
                    SERVICE_MESSAGE => {
                        self.take_message(&body);
                    }
                    KEEPALIVE => {}
                    _ if got == kind => return Ok(body),
                    _ => anyhow::bail!("unexpected gateway packet 0x{got:X} (waiting for 0x{kind:X})"),
                }
                continue;
            }
            let mut chunk = vec![0u8; 16 * 1024];
            let read = core::future::poll_fn(|cx| Pin::new(&mut self.ws).poll_read(cx, &mut chunk))
                .await
                .context("gateway connection")?;
            if read == 0 {
                return Err(match &self.message {
                    Some(message) => anyhow::anyhow!("the gateway closed the connection: {message}"),
                    None => anyhow::anyhow!("the gateway closed the connection"),
                });
            }
            self.incoming.extend_from_slice(&chunk[..read]);
        }
    }

    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.outgoing_written < self.outgoing.len() {
            let written = ready!(Pin::new(&mut self.ws).poll_write(cx, &self.outgoing[self.outgoing_written..]))?;
            if written == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.outgoing_written += written;
        }
        self.outgoing.clear();
        self.outgoing_written = 0;
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for Tunnel {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        loop {
            if this.data_read < this.data.len() {
                let n = buf.len().min(this.data.len() - this.data_read);
                buf[..n].copy_from_slice(&this.data[this.data_read..this.data_read + n]);
                this.data_read += n;
                return Poll::Ready(Ok(n));
            }
            if this.closed {
                return Poll::Ready(Ok(0));
            }
            match take_packet(&mut this.incoming)? {
                Some((DATA, body)) => {
                    let length = usize::from(u16_at(&body, 0).unwrap_or(0));
                    let Some(bytes) = body.get(2..2 + length) else {
                        return Poll::Ready(Err(io::Error::new(io::ErrorKind::InvalidData, "short DATA packet")));
                    };
                    this.data = bytes.to_vec();
                    this.data_read = 0;
                }
                Some((SERVICE_MESSAGE, body)) => {
                    this.take_message(&body);
                }
                Some((KEEPALIVE, _)) => {}
                Some((CLOSE_CHANNEL | CLOSE_CHANNEL_RESPONSE, _)) => {
                    debug!("RD Gateway closed the channel");
                    this.closed = true;
                }
                Some((kind, _)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected gateway packet 0x{kind:X}"),
                    )));
                }
                None => {
                    let mut chunk = [0u8; 16 * 1024];
                    let read = ready!(Pin::new(&mut this.ws).poll_read(cx, &mut chunk))?;
                    if read == 0 {
                        this.closed = true;
                    }
                    this.incoming.extend_from_slice(&chunk[..read]);
                }
            }
        }
    }
}

impl AsyncWrite for Tunnel {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        ready!(this.poll_drain(cx))?;
        let n = buf.len().min(DATA_CHUNK);
        let mut body = Vec::with_capacity(2 + n);
        body.extend_from_slice(&u16::try_from(n).expect("n <= DATA_CHUNK").to_le_bytes());
        body.extend_from_slice(&buf[..n]);
        this.outgoing = packet(DATA, &body)?;
        this.outgoing_written = 0;
        // Taken: what can't be written now goes before the next write, or on flush.
        if let Poll::Ready(Err(e)) = this.poll_drain(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_drain(cx))?;
        Pin::new(&mut self.ws).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_drain(cx))?;
        if !self.close_sent && !self.closed {
            // HTTP_CLOSE_PACKET, status 0: tells the gateway to end the RDP session now.
            self.outgoing = packet(CLOSE_CHANNEL, &0u32.to_le_bytes())?;
            self.outgoing_written = 0;
            self.close_sent = true;
            ready!(self.poll_drain(cx))?;
        }
        Pin::new(&mut self.ws).poll_close(cx)
    }
}

/// `host[:port]`, the port defaulting to 3389.
fn split_host_port(target: &str) -> (&str, u16) {
    match target.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, port.parse().unwrap_or(3389)),
        _ => (target, 3389),
    }
}

/// RDP's TLS, inside the tunnel. The server's certificate must chain to a public root and name
/// `server_name` (the name mstsc would check). Returns the stream and the server's public key,
/// which CredSSP binds to.
pub(crate) async fn upgrade_tls(stream: RdpStream, server_name: &str) -> anyhow::Result<(RdpStream, Vec<u8>)> {
    use x509_cert::der::Decode as _;

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls_rustcrypto::provider()))
        .with_safe_default_protocol_versions()
        .context("TLS configuration")?
        .with_root_certificates(roots)
        .with_no_client_auth();

    let (host, _) = split_host_port(server_name);
    let name = rustls::pki_types::ServerName::try_from(host.to_owned()).context("RDP server name")?;
    let tls = futures_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await
        .with_context(|| format!("TLS with the RDP server {host}"))?;

    let cert = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .context("the RDP server sent no certificate")?;
    let cert = x509_cert::Certificate::from_der(cert.as_ref()).context("the RDP server's certificate")?;
    let server_public_key = cert
        .tbs_certificate()
        .subject_public_key_info()
        .subject_public_key
        .as_bytes()
        .context("the RDP server's public key")?
        .to_owned();

    Ok((Box::new(tls), server_public_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_round_trip() {
        let mut buf = packet(DATA, &[2, 0, 0xAA, 0xBB]).expect("packet");
        assert_eq!(buf.len(), HEADER + 4);
        let tail = packet(KEEPALIVE, &[]).expect("packet");
        buf.extend_from_slice(&tail[..3]);

        let (kind, body) = take_packet(&mut buf).expect("valid").expect("whole");
        assert_eq!((kind, body), (DATA, vec![2, 0, 0xAA, 0xBB]));
        // only part of the next header has arrived
        assert!(take_packet(&mut buf).expect("valid").is_none());
        buf.extend_from_slice(&tail[3..]);
        assert_eq!(take_packet(&mut buf).expect("valid"), Some((KEEPALIVE, Vec::new())));
        assert!(buf.is_empty());
    }

    #[test]
    fn packet_length_is_checked() {
        let mut short = vec![0x0A, 0, 0, 0, 4, 0, 0, 0]; // shorter than its own header
        assert!(take_packet(&mut short).is_err());
        let mut huge = vec![0x0A, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0x7F];
        assert!(take_packet(&mut huge).is_err());
    }

    #[test]
    fn service_message() {
        let mut body = Vec::new();
        put_string(&mut body, "Ticket expired", true).expect("string");
        assert_eq!(service_message_text(&body), "Ticket expired");
        assert_eq!(service_message_text(&[9, 0]), "");
    }

    #[test]
    fn host_and_port() {
        assert_eq!(split_host_port("diode.example.com"), ("diode.example.com", 3389));
        assert_eq!(split_host_port("diode.example.com:3390"), ("diode.example.com", 3390));
        assert_eq!(split_host_port("fe80::1"), ("fe80::1", 3389));
    }
}
