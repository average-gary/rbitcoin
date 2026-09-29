//! Test-only TDP client: a Noise initiator pinned to the TP's authority key.

use crate::transport::{Frame, NoiseConn};
use binary_sv2::Str0255;
use common_messages_sv2::{Protocol, SetupConnection, MESSAGE_TYPE_SETUP_CONNECTION};
use std::io;
use std::net::SocketAddr;
use template_distribution_sv2::{
    CoinbaseOutputConstraints, MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS,
};
use tokio::net::TcpStream;

pub struct TpClient {
    conn: NoiseConn,
}

impl TpClient {
    /// TCP connect and complete the NX handshake against `authority_pubkey`.
    pub async fn connect(addr: SocketAddr, authority_pubkey: [u8; 32]) -> io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let initiator = noise_sv2::Initiator::from_raw_k(authority_pubkey)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{e:?}")))?;
        Ok(Self {
            conn: NoiseConn::connect(stream, initiator).await?,
        })
    }

    /// Send `SetupConnection`; `protocol` is the raw discriminant (2 = TDP).
    pub async fn setup_connection(
        &mut self,
        protocol: u8,
        min_version: u16,
        max_version: u16,
        flags: u32,
    ) -> io::Result<()> {
        let protocol = Protocol::try_from(protocol)
            .map_err(|()| io::Error::new(io::ErrorKind::InvalidInput, "protocol"))?;
        let s = |v: &'static str| Str0255::try_from(v).expect("short literal");
        let msg = SetupConnection {
            protocol,
            min_version,
            max_version,
            flags,
            endpoint_host: s("127.0.0.1"),
            endpoint_port: 0,
            vendor: s("rbitcoin-test"),
            hardware_version: s(""),
            firmware: s(""),
            device_id: s(""),
        };
        self.conn.send(MESSAGE_TYPE_SETUP_CONNECTION, msg).await
    }

    pub async fn coinbase_output_constraints(
        &mut self,
        max_additional_size: u32,
        max_additional_sigops: u16,
    ) -> io::Result<()> {
        let msg = CoinbaseOutputConstraints {
            coinbase_output_max_additional_size: max_additional_size,
            coinbase_output_max_additional_sigops: max_additional_sigops,
        };
        self.conn
            .send(MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS, msg)
            .await
    }

    pub async fn recv(&mut self) -> io::Result<Frame> {
        self.conn.recv().await
    }
}
