#![cfg_attr(doc, doc = include_str!("../README.md"))]
#![doc(html_logo_url = "https://cdnweb.devolutions.net/images/projects/devolutions/logos/devolutions-icon-shadow.svg")]

#[rustfmt::skip] // do not re-order this pub use
pub use ironrdp_async::*;

use core::pin::Pin;
use std::io;

use bytes::BytesMut;
use futures_util::io::{AsyncRead, AsyncWrite};

pub type FuturesFramed<S> = Framed<FuturesStream<S>>;

pub struct FuturesStream<S> {
    inner: S,
}

impl<S> StreamWrapper for FuturesStream<S> {
    type InnerStream = S;

    fn from_inner(stream: Self::InnerStream) -> Self {
        Self { inner: stream }
    }

    fn into_inner(self) -> Self::InnerStream {
        self.inner
    }

    fn get_inner(&self) -> &Self::InnerStream {
        &self.inner
    }

    fn get_inner_mut(&mut self) -> &mut Self::InnerStream {
        &mut self.inner
    }
}

impl<S> FramedRead for FuturesStream<S>
where
    S: Send + Sync + Unpin + AsyncRead,
{
    type ReadFut<'read>
        = Pin<Box<dyn Future<Output = io::Result<usize>> + Send + Sync + 'read>>
    where
        Self: 'read;

    fn read<'a>(&'a mut self, buf: &'a mut BytesMut) -> Self::ReadFut<'a> {
        use futures_util::io::AsyncReadExt as _;

        Box::pin(async {
            // NOTE(perf): tokio implementation is more efficient
            //
            // 16 KiB, not 1 KiB. Measured on the wire (2026-08-23) against a live AVD session:
            // the server delivers PDU-granular WebSocket frames of ~1,623 bytes, dead uniform
            // (p50 = p90 = p99 = 1,623, max 6,653). A 1 KiB buffer therefore needed AT LEAST TWO
            // reads for every single frame, and in the browser build each read is a fresh
            // `Box::pin` heap allocation plus a JS<->wasm crossing -- paid ~18,000 times per
            // session for nothing. Verified after the change: exactly one read per PDU, ~1.5 KB
            // each.
            //
            // Honest caveat: this did NOT measurably reduce CPU. It removes a real inefficiency;
            // it is not the explanation for the web client using ~2.5x the CPU of the Microsoft
            // web client, which remains unexplained.
            let mut read_bytes = [0u8; 16 * 1024];
            let len = self.inner.read(&mut read_bytes).await?;
            buf.extend_from_slice(&read_bytes[..len]);

            Ok(len)
        })
    }
}

impl<S> FramedWrite for FuturesStream<S>
where
    S: Send + Sync + Unpin + AsyncWrite,
{
    type WriteAllFut<'write>
        = Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync + 'write>>
    where
        Self: 'write;

    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> Self::WriteAllFut<'a> {
        use futures_util::io::AsyncWriteExt as _;

        Box::pin(async {
            self.inner.write_all(buf).await?;
            self.inner.flush().await?;

            Ok(())
        })
    }
}

pub type LocalFuturesFramed<S> = Framed<LocalFuturesStream<S>>;

pub struct LocalFuturesStream<S> {
    inner: S,
}

impl<S> StreamWrapper for LocalFuturesStream<S> {
    type InnerStream = S;

    fn from_inner(stream: Self::InnerStream) -> Self {
        Self { inner: stream }
    }

    fn into_inner(self) -> Self::InnerStream {
        self.inner
    }

    fn get_inner(&self) -> &Self::InnerStream {
        &self.inner
    }

    fn get_inner_mut(&mut self) -> &mut Self::InnerStream {
        &mut self.inner
    }
}

impl<S> FramedRead for LocalFuturesStream<S>
where
    S: Unpin + AsyncRead,
{
    type ReadFut<'read>
        = Pin<Box<dyn Future<Output = io::Result<usize>> + 'read>>
    where
        Self: 'read;

    fn read<'a>(&'a mut self, buf: &'a mut BytesMut) -> Self::ReadFut<'a> {
        use futures_util::io::AsyncReadExt as _;

        Box::pin(async {
            // NOTE(perf): tokio implementation is more efficient
            //
            // 16 KiB, not 1 KiB. Measured on the wire (2026-08-23) against a live AVD session:
            // the server delivers PDU-granular WebSocket frames of ~1,623 bytes, dead uniform
            // (p50 = p90 = p99 = 1,623, max 6,653). A 1 KiB buffer therefore needed AT LEAST TWO
            // reads for every single frame, and in the browser build each read is a fresh
            // `Box::pin` heap allocation plus a JS<->wasm crossing -- paid ~18,000 times per
            // session for nothing. Verified after the change: exactly one read per PDU, ~1.5 KB
            // each.
            //
            // Honest caveat: this did NOT measurably reduce CPU. It removes a real inefficiency;
            // it is not the explanation for the web client using ~2.5x the CPU of the Microsoft
            // web client, which remains unexplained.
            let mut read_bytes = [0u8; 16 * 1024];
            let len = self.inner.read(&mut read_bytes[..]).await?;
            buf.extend_from_slice(&read_bytes[..len]);

            Ok(len)
        })
    }
}

impl<S> FramedWrite for LocalFuturesStream<S>
where
    S: Unpin + AsyncWrite,
{
    type WriteAllFut<'write>
        = Pin<Box<dyn Future<Output = io::Result<()>> + 'write>>
    where
        Self: 'write;

    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> Self::WriteAllFut<'a> {
        use futures_util::io::AsyncWriteExt as _;

        Box::pin(async {
            self.inner.write_all(buf).await?;
            self.inner.flush().await?;

            Ok(())
        })
    }
}
