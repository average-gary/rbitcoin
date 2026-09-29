//! One TDP session: Noise handshake, `SetupConnection`, then TDP messages.

use crate::template;
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
use rbitcoin_net::{BlockingRegion, ChainHub};
use std::io;
use std::sync::Arc;
use template_distribution_sv2::{
    CoinbaseOutputConstraints, MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS, MESSAGE_TYPE_NEW_TEMPLATE,
};
use tokio::net::TcpStream;

const TDP_VERSION: u16 = 2;

#[derive(Clone, Copy)]
enum Phase {
    AwaitingSetup,
    AwaitingConstraints,
    Active,
}

enum Next {
    Continue(Phase),
    Close,
}

pub(crate) async fn serve(
    stream: TcpStream,
    responder: Box<Responder>,
    chain: Arc<ChainHub>,
) -> io::Result<()> {
    let mut conn = NoiseConn::accept(stream, responder).await?;
    let mut phase = Phase::AwaitingSetup;
    let mut last_template_id = 0u64;
    loop {
        let frame = conn.recv().await?;
        let next = match phase {
            Phase::AwaitingSetup => on_setup(&mut conn, frame).await?,
            Phase::AwaitingConstraints | Phase::Active
                if frame.msg_type == MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS =>
            {
                last_template_id += 1;
                on_constraints(&mut conn, &chain, frame, last_template_id).await?
            }
            Phase::AwaitingConstraints | Phase::Active => {
                rbitcoin_log::info!("sv2: ignoring message {:#x}", frame.msg_type);
                Next::Continue(phase)
            }
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

async fn on_constraints(
    conn: &mut NoiseConn,
    chain: &Arc<ChainHub>,
    mut frame: Frame,
    template_id: u64,
) -> io::Result<Next> {
    let Ok(c) = binary_sv2::from_bytes::<CoinbaseOutputConstraints>(&mut frame.payload) else {
        rbitcoin_log::info!("sv2: undecodable CoinbaseOutputConstraints");
        return Ok(Next::Close);
    };
    let (size, sigops) = (
        c.coinbase_output_max_additional_size,
        c.coinbase_output_max_additional_sigops,
    );
    let chain = Arc::clone(chain);
    let t = tokio::task::spawn_blocking(move || {
        let _g = BlockingRegion::enter();
        template::build(&chain, size, sigops)
    })
    .await
    .map_err(io::Error::other)?;
    let msg = t
        .to_message(template_id, true)
        .map_err(|e| io::Error::other(format!("sv2 NewTemplate: {e:?}")))?;
    conn.send(MESSAGE_TYPE_NEW_TEMPLATE, msg).await?;
    Ok(Next::Continue(Phase::Active))
}
