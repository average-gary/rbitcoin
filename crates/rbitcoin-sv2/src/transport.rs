//! Noise_NX transport over one TCP stream: handshake, then encrypted SV2 frames.

use binary_sv2::{GetSize, Serialize};
use codec_sv2::{
    Decoded, Decrypted, Handshake, MessageFrame, NoiseDecoder, NoiseEncoder, TransportDecryptState,
    TransportEncryptState,
};
use noise_sv2::{Initiator, Responder};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

/// One decrypted SV2 frame: header message type and owned payload.
pub struct Frame {
    pub msg_type: u8,
    pub payload: Vec<u8>,
}

pub(crate) struct NoiseConn {
    reader: NoiseReader,
    writer: NoiseWriter,
}

/// Receive half. `recv` is not cancel-safe: a dropped call loses a partly
/// read frame, so a session that selects over input reads from a task.
pub(crate) struct NoiseReader {
    stream: OwnedReadHalf,
    decoder: NoiseDecoder,
    // `next_transport_frame` consumes the state; a failed round leaves `None`
    // and the connection must close (codec_sv2 nonce rule).
    rx: Option<TransportDecryptState>,
}

pub(crate) struct NoiseWriter {
    stream: OwnedWriteHalf,
    encoder: NoiseEncoder,
    tx: TransportEncryptState,
    /// Longest one socket write may make no progress.
    write_timeout: Duration,
}

fn codec_err(e: codec_sv2::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("sv2 codec: {e:?}"))
}

impl NoiseConn {
    pub(crate) async fn accept(
        mut stream: TcpStream,
        responder: Box<Responder>,
        write_timeout: Duration,
    ) -> io::Result<Self> {
        let mut decoder = NoiseDecoder::new();
        let mut encoder = NoiseEncoder::new();
        let first = loop {
            match decoder
                .next_handshake_frame::<Responder>()
                .map_err(codec_err)?
            {
                Decoded::Frame(m) => break m,
                Decoded::Incomplete(_) => stream.read_exact(decoder.writable()).await?,
            };
        };
        let re_pub = first
            .payload()
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "sv2: handshake size"))?;
        let (reply, transport) = Handshake::responder(responder)
            .step_1(re_pub)
            .map_err(codec_err)?;
        stream
            .write_all(encoder.encode_handshake(reply).as_ref())
            .await?;
        let (tx, rx) = transport.split();
        Ok(Self::new(stream, encoder, decoder, tx, rx, write_timeout))
    }

    pub(crate) async fn connect(
        mut stream: TcpStream,
        initiator: Box<Initiator>,
        write_timeout: Duration,
    ) -> io::Result<Self> {
        let mut decoder = NoiseDecoder::new();
        let mut encoder = NoiseEncoder::new();
        let (first, sent) = Handshake::initiator(initiator)
            .step_0()
            .map_err(codec_err)?;
        stream
            .write_all(encoder.encode_handshake(first).as_ref())
            .await?;
        let reply = loop {
            match decoder
                .next_handshake_frame::<codec_sv2::InitiatorSent>()
                .map_err(codec_err)?
            {
                Decoded::Frame(m) => break m,
                Decoded::Incomplete(_) => stream.read_exact(decoder.writable()).await?,
            };
        };
        let reply = reply
            .payload()
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "sv2: handshake size"))?;
        let (tx, rx) = sent.step_2(reply).map_err(codec_err)?.split();
        Ok(Self::new(stream, encoder, decoder, tx, rx, write_timeout))
    }

    fn new(
        stream: TcpStream,
        encoder: NoiseEncoder,
        decoder: NoiseDecoder,
        tx: TransportEncryptState,
        rx: TransportDecryptState,
        write_timeout: Duration,
    ) -> Self {
        let (read, write) = stream.into_split();
        Self {
            reader: NoiseReader {
                stream: read,
                decoder,
                rx: Some(rx),
            },
            writer: NoiseWriter {
                stream: write,
                encoder,
                tx,
                write_timeout,
            },
        }
    }

    pub(crate) fn into_split(self) -> (NoiseReader, NoiseWriter) {
        (self.reader, self.writer)
    }

    pub(crate) async fn send<T: Serialize + GetSize>(
        &mut self,
        msg_type: u8,
        msg: T,
    ) -> io::Result<()> {
        self.writer.send(msg_type, msg).await
    }

    pub(crate) async fn recv(&mut self) -> io::Result<Frame> {
        self.reader.recv().await
    }
}

impl NoiseWriter {
    pub(crate) async fn send<T: Serialize + GetSize>(
        &mut self,
        msg_type: u8,
        msg: T,
    ) -> io::Result<()> {
        let frame = MessageFrame::from_message(msg, msg_type, 0, false).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("sv2 frame: {e:?}"))
        })?;
        let bytes = self
            .encoder
            .encode_transport(frame, &mut self.tx)
            .map_err(codec_err)?;
        // Deadline per write call, not per frame: a slow reader still gets
        // a multi-MB RequestTransactionData.Success, one that stops does not
        // hold the session.
        let mut buf: &[u8] = bytes.as_ref();
        while !buf.is_empty() {
            let n = tokio::time::timeout(self.write_timeout, self.stream.write(buf))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sv2: write stalled"))??;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            buf = &buf[n..];
        }
        Ok(())
    }
}

impl NoiseReader {
    pub(crate) async fn recv(&mut self) -> io::Result<Frame> {
        loop {
            let state = self
                .rx
                .take()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "sv2: decrypt failed"))?;
            match self
                .decoder
                .next_transport_frame(state)
                .map_err(codec_err)?
            {
                Decrypted::Frame(mut frame, state) => {
                    self.rx = Some(state);
                    return Ok(Frame {
                        msg_type: frame.header().msg_type(),
                        payload: frame.payload().to_vec(),
                    });
                }
                Decrypted::Incomplete(_, state) => {
                    self.rx = Some(state);
                    self.stream.read_exact(self.decoder.writable()).await?;
                }
            }
        }
    }
}
