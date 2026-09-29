//! One TDP session: Noise handshake, `SetupConnection`, then TDP messages.

use crate::transport::{Frame, NoiseConn};
use binary_sv2::Str0255;
use common_messages_sv2::{
    Protocol, SetupConnection, SetupConnectionError, SetupConnectionSuccess,
    ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH,
    ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS,
    ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL, MESSAGE_TYPE_SETUP_CONNECTION,
    MESSAGE_TYPE_SETUP_CONNECTION_ERROR, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
};
use noise_sv2::Responder;
use std::io;
use tokio::net::TcpStream;

const TDP_VERSION: u16 = 2;

enum Phase {
    AwaitingSetup,
    AwaitingConstraints,
}

enum Next {
    Continue(Phase),
    Close,
}

pub(crate) async fn serve(stream: TcpStream, responder: Box<Responder>) -> io::Result<()> {
    let mut conn = NoiseConn::accept(stream, responder).await?;
    let mut phase = Phase::AwaitingSetup;
    loop {
        let frame = conn.recv().await?;
        let next = match phase {
            Phase::AwaitingSetup => on_setup(&mut conn, frame).await?,
            Phase::AwaitingConstraints => Next::Continue(Phase::AwaitingConstraints),
        };
        phase = match next {
            Next::Continue(p) => p,
            Next::Close => return Ok(()),
        };
    }
}

fn setup_error(m: &SetupConnection) -> Option<(u32, &'static str)> {
    if m.protocol != Protocol::TemplateDistributionProtocol {
        return Some((0, ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL));
    }
    if !(m.min_version..=m.max_version).contains(&TDP_VERSION) {
        return Some((0, ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH));
    }
    // TDP defines no SetupConnection flags: every set bit is unsupported.
    if m.flags != 0 {
        return Some((
            m.flags,
            ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS,
        ));
    }
    None
}

async fn on_setup(conn: &mut NoiseConn, mut frame: Frame) -> io::Result<Next> {
    if frame.msg_type != MESSAGE_TYPE_SETUP_CONNECTION {
        rbitcoin_log::info!("sv2: message {:#x} before SetupConnection", frame.msg_type);
        return Ok(Next::Close);
    }
    let Ok(setup) = binary_sv2::from_bytes::<SetupConnection>(&mut frame.payload) else {
        rbitcoin_log::info!("sv2: undecodable SetupConnection");
        return Ok(Next::Close);
    };
    if let Some((flags, code)) = setup_error(&setup) {
        let error_code = Str0255::try_from(code)
            .map_err(|e| io::Error::other(format!("sv2 error code: {e:?}")))?;
        let reply = SetupConnectionError { flags, error_code };
        conn.send(MESSAGE_TYPE_SETUP_CONNECTION_ERROR, reply)
            .await?;
        return Ok(Next::Close);
    }
    let reply = SetupConnectionSuccess {
        used_version: TDP_VERSION,
        flags: 0,
    };
    conn.send(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, reply)
        .await?;
    Ok(Next::Continue(Phase::AwaitingConstraints))
}
