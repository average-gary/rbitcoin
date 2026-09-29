//! One TDP session: Noise handshake, `SetupConnection`, then TDP messages.

use crate::template;
use crate::transport::{Frame, NoiseConn};
use binary_sv2::{Seq064K, Str0255, B016M, B064K};
use common_messages_sv2::{
    Protocol, SetupConnection, SetupConnectionError, SetupConnectionSuccess,
    ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH,
    ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS,
    ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL, MESSAGE_TYPE_SETUP_CONNECTION,
    MESSAGE_TYPE_SETUP_CONNECTION_ERROR, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
};
use noise_sv2::Responder;
use rbitcoin_net::{BlockingRegion, ChainHub};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use template_distribution_sv2::{
    CoinbaseOutputConstraints, RequestTransactionData, RequestTransactionDataError,
    RequestTransactionDataSuccess, ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID,
    ERROR_CODE_REQUEST_TRANSACTION_DATA_TEMPLATE_ID_NOT_FOUND,
    MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS, MESSAGE_TYPE_NEW_TEMPLATE,
    MESSAGE_TYPE_REQUEST_TRANSACTION_DATA, MESSAGE_TYPE_REQUEST_TRANSACTION_DATA_ERROR,
    MESSAGE_TYPE_REQUEST_TRANSACTION_DATA_SUCCESS, MESSAGE_TYPE_SET_NEW_PREV_HASH,
};
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;

const TDP_VERSION: u16 = 2;

/// RAM trade (docs/sv2-template-provider.md): each session keeps its last
/// few templates with their full txs, so `RequestTransactionData` and
/// `SubmitSolution` do not depend on the mempool still holding them.
const MAX_RETAINED: usize = 3;

/// The templates one session was sent. Ids are strictly increasing.
#[derive(Default)]
struct Templates {
    last_id: u64,
    current_prev: Option<[u8; 32]>,
    retained: VecDeque<(u64, template::Template)>,
}

impl Templates {
    fn retain(&mut self, id: u64, t: template::Template) {
        if self.retained.len() == MAX_RETAINED {
            self.retained.pop_front();
        }
        self.retained.push_back((id, t));
    }

    fn get(&self, id: u64) -> Option<&template::Template> {
        self.retained.iter().find(|(i, _)| *i == id).map(|(_, t)| t)
    }
}

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
    let mut templates = Templates::default();
    loop {
        let frame = conn.recv().await?;
        let next = match phase {
            Phase::AwaitingSetup => on_setup(&mut conn, frame).await?,
            Phase::AwaitingConstraints | Phase::Active
                if frame.msg_type == MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS =>
            {
                on_constraints(&mut conn, &chain, frame, &mut templates).await?
            }
            Phase::AwaitingConstraints | Phase::Active
                if frame.msg_type == MESSAGE_TYPE_REQUEST_TRANSACTION_DATA =>
            {
                on_request_transaction_data(&mut conn, frame, &templates).await?;
                Next::Continue(phase)
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
    templates: &mut Templates,
) -> io::Result<Next> {
    let Ok(c) = binary_sv2::from_bytes::<CoinbaseOutputConstraints>(&mut frame.payload) else {
        rbitcoin_log::info!("sv2: undecodable CoinbaseOutputConstraints");
        return Ok(Next::Close);
    };
    let (size, sigops) = (
        c.coinbase_output_max_additional_size,
        c.coinbase_output_max_additional_sigops,
    );
    let Some(t) = synced_template(chain, size, sigops).await? else {
        return Ok(Next::Close);
    };
    templates.last_id += 1;
    let template_id = templates.last_id;
    // sv2-spec 07 §7.3: a template on a new prev hash is future, then activated.
    let new_prev = templates.current_prev != Some(t.prev_hash);
    let msg = t
        .to_message(template_id, new_prev)
        .map_err(|e| io::Error::other(format!("sv2 NewTemplate: {e:?}")))?;
    conn.send(MESSAGE_TYPE_NEW_TEMPLATE, msg).await?;
    if new_prev {
        conn.send(MESSAGE_TYPE_SET_NEW_PREV_HASH, t.to_prev_hash(template_id))
            .await?;
        templates.current_prev = Some(t.prev_hash);
    }
    templates.retain(template_id, t);
    Ok(Next::Continue(Phase::Active))
}

/// An id this session was sent but no longer retains is stale; any other
/// unknown id was never sent.
async fn on_request_transaction_data(
    conn: &mut NoiseConn,
    mut frame: Frame,
    templates: &Templates,
) -> io::Result<()> {
    let Ok(RequestTransactionData { template_id }) = binary_sv2::from_bytes(&mut frame.payload)
    else {
        rbitcoin_log::info!("sv2: undecodable RequestTransactionData");
        return Ok(());
    };
    let Some(t) = templates.get(template_id) else {
        let code = if template_id <= templates.last_id {
            ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID
        } else {
            ERROR_CODE_REQUEST_TRANSACTION_DATA_TEMPLATE_ID_NOT_FOUND
        };
        let reply = RequestTransactionDataError {
            template_id,
            error_code: Str0255::try_from(code)
                .map_err(|e| io::Error::other(format!("sv2 error code: {e:?}")))?,
        };
        return conn
            .send(MESSAGE_TYPE_REQUEST_TRANSACTION_DATA_ERROR, reply)
            .await;
    };
    let raw: Vec<Vec<u8>> = t
        .txs
        .iter()
        .map(bitcoin::consensus::encode::serialize)
        .collect();
    let wire = |e: binary_sv2::Error| {
        io::Error::other(format!("sv2 RequestTransactionData.Success: {e:?}"))
    };
    let reply = RequestTransactionDataSuccess {
        template_id,
        excess_data: B064K::try_from(&[][..]).map_err(wire)?,
        transaction_list: Seq064K::new(
            raw.iter()
                .map(|tx| B016M::try_from(&tx[..]))
                .collect::<Result<_, _>>()
                .map_err(wire)?,
        )
        .map_err(wire)?,
    };
    conn.send(MESSAGE_TYPE_REQUEST_TRANSACTION_DATA_SUCCESS, reply)
        .await
}

/// No template while in IBD. Leaving IBD always comes with a new tip, so the
/// gate rechecks on tip events. `None`: the tip channel closed (shutdown).
async fn synced_template(
    chain: &Arc<ChainHub>,
    size: u32,
    sigops: u16,
) -> io::Result<Option<template::Template>> {
    let mut tips = chain.subscribe_tips();
    let mut logged = false;
    loop {
        let c = Arc::clone(chain);
        let t = tokio::task::spawn_blocking(move || {
            let _g = BlockingRegion::enter();
            (!c.in_ibd())
                .then(|| template::build(&c, size, sigops))
                .transpose()
        })
        .await
        .map_err(io::Error::other)??;
        if t.is_some() {
            return Ok(t);
        }
        if !logged {
            rbitcoin_log::info!("sv2: holding templates until the node leaves IBD");
            logged = true;
        }
        match tips.recv().await {
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return Ok(None),
        }
    }
}
