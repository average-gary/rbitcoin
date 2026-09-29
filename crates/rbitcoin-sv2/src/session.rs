//! One TDP session: Noise handshake, `SetupConnection`, then TDP messages.

use crate::template;
use crate::transport::{Frame, NoiseConn, NoiseWriter};
use binary_sv2::{Seq064K, Str0255, B016M, B064K};
use bitcoin::hashes::Hash;
use bitcoin::{block, Block, BlockHash, CompactTarget, Transaction, TxMerkleNode};
use common_messages_sv2::{
    Protocol, SetupConnection, SetupConnectionError, SetupConnectionSuccess,
    ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH,
    ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS,
    ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL, MESSAGE_TYPE_SETUP_CONNECTION,
    MESSAGE_TYPE_SETUP_CONNECTION_ERROR, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
};
use noise_sv2::Responder;
use rbitcoin_net::{BlockingRegion, ChainHub};
use rbitcoin_store::merkle_root_from_txids;
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use template_distribution_sv2::{
    CoinbaseOutputConstraints, RequestTransactionData, RequestTransactionDataError,
    RequestTransactionDataSuccess, SubmitSolution,
    ERROR_CODE_REQUEST_TRANSACTION_DATA_STALE_TEMPLATE_ID,
    ERROR_CODE_REQUEST_TRANSACTION_DATA_TEMPLATE_ID_NOT_FOUND,
    MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS, MESSAGE_TYPE_NEW_TEMPLATE,
    MESSAGE_TYPE_REQUEST_TRANSACTION_DATA, MESSAGE_TYPE_REQUEST_TRANSACTION_DATA_ERROR,
    MESSAGE_TYPE_REQUEST_TRANSACTION_DATA_SUCCESS, MESSAGE_TYPE_SET_NEW_PREV_HASH,
    MESSAGE_TYPE_SUBMIT_SOLUTION,
};
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

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
    /// `SetNewPrevHash.header_timestamp` sent for `current_prev`, and when.
    prev_sent: Option<(u32, Instant)>,
    /// A template was built since the last tip event.
    built_since_tip: bool,
    retained: VecDeque<Retained>,
}

struct Retained {
    id: u64,
    t: template::Template,
    prev_sent: (u32, Instant),
    /// Set once the tip moves on.
    retire_at: Option<Instant>,
}

impl Templates {
    fn retain(&mut self, id: u64, t: template::Template, prev_sent: (u32, Instant)) {
        if self.retained.len() == MAX_RETAINED {
            self.retained.pop_front();
        }
        self.retained.push_back(Retained {
            id,
            t,
            prev_sent,
            retire_at: None,
        });
    }

    /// Whether a tip event needs a rebuild: a new prev hash, or any build
    /// since the last event.
    fn on_tip(&mut self, hash: [u8; 32]) -> bool {
        // ChainHub publishes the store tip (connect or rollback reconnect),
        // then strips the block's txs from the mempool, then sends the event:
        // a build in between can select txs the block confirmed.
        std::mem::take(&mut self.built_since_tip) || self.current_prev != Some(hash)
    }

    fn get(&self, id: u64) -> Option<&Retained> {
        self.retained.iter().find(|r| r.id == id)
    }

    /// Every retained template predates the new prev hash.
    fn start_grace(&mut self, deadline: Instant) {
        for r in &mut self.retained {
            r.retire_at.get_or_insert(deadline);
        }
    }

    fn next_retire(&self) -> Option<Instant> {
        self.retained.iter().filter_map(|r| r.retire_at).min()
    }

    fn retire(&mut self, now: Instant) {
        self.retained
            .retain(|r| r.retire_at.is_none_or(|d| d > now));
    }
}

pub(crate) async fn serve(
    stream: TcpStream,
    responder: Box<Responder>,
    chain: Arc<ChainHub>,
    stale_grace: Duration,
) -> io::Result<()> {
    let mut conn = NoiseConn::accept(stream, responder).await?;
    let frame = conn.recv().await?;
    if !on_setup(&mut conn, frame).await? {
        return Ok(());
    }
    let (mut reader, writer) = conn.into_split();
    let (frames_tx, mut frames) = mpsc::channel(1);
    // Dropping the set aborts the reader when the session ends.
    let mut pump = JoinSet::new();
    pump.spawn(async move {
        loop {
            let f = reader.recv().await;
            let end = f.is_err();
            if frames_tx.send(f).await.is_err() || end {
                return;
            }
        }
    });
    let mut s = Session {
        conn: writer,
        chain,
        stale_grace,
        constraints: None,
        templates: Templates::default(),
        held_logged: false,
    };
    s.run(&mut frames).await
}

struct Session {
    conn: NoiseWriter,
    chain: Arc<ChainHub>,
    stale_grace: Duration,
    /// Last `CoinbaseOutputConstraints`: `(max_additional_size, sigops)`.
    constraints: Option<(u32, u16)>,
    templates: Templates,
    held_logged: bool,
}

impl Session {
    async fn run(&mut self, frames: &mut mpsc::Receiver<io::Result<Frame>>) -> io::Result<()> {
        let mut tips = self.chain.subscribe_tips();
        loop {
            let retire_at = self.templates.next_retire();
            tokio::select! {
                f = frames.recv() => {
                    let Some(f) = f else { return Ok(()) };
                    if !self.on_frame(f?).await? {
                        return Ok(());
                    }
                }
                tip = tips.recv() => match tip {
                    Ok(ev) if !self.templates.on_tip(ev.hash.to_byte_array()) => {}
                    Ok(_) | Err(RecvError::Lagged(_)) => self.publish().await?,
                    Err(RecvError::Closed) => return Ok(()),
                },
                _ = tokio::time::sleep_until(retire_at.unwrap_or_else(Instant::now)),
                    if retire_at.is_some() =>
                {
                    self.templates.retire(Instant::now());
                }
            }
        }
    }

    /// `false`: close the session.
    async fn on_frame(&mut self, mut frame: Frame) -> io::Result<bool> {
        match frame.msg_type {
            MESSAGE_TYPE_COINBASE_OUTPUT_CONSTRAINTS => {
                let Ok(c) = binary_sv2::from_bytes::<CoinbaseOutputConstraints>(&mut frame.payload)
                else {
                    rbitcoin_log::info!("sv2: undecodable CoinbaseOutputConstraints");
                    return Ok(false);
                };
                self.constraints = Some((
                    c.coinbase_output_max_additional_size,
                    c.coinbase_output_max_additional_sigops,
                ));
                self.publish().await?;
            }
            MESSAGE_TYPE_REQUEST_TRANSACTION_DATA => {
                on_request_transaction_data(&mut self.conn, frame, &self.templates).await?;
            }
            MESSAGE_TYPE_SUBMIT_SOLUTION => self.on_submit_solution(frame).await?,
            t => rbitcoin_log::info!("sv2: ignoring message {t:#x}"),
        }
        Ok(true)
    }

    /// Build on the current tip and send it. No template while in IBD:
    /// leaving IBD always comes with a new tip, which calls this again.
    async fn publish(&mut self) -> io::Result<()> {
        let Some((size, sigops)) = self.constraints else {
            return Ok(());
        };
        let c = Arc::clone(&self.chain);
        let t = tokio::task::spawn_blocking(move || {
            let _g = BlockingRegion::enter();
            (!c.in_ibd())
                .then(|| template::build(&c, size, sigops))
                .transpose()
        })
        .await
        .map_err(io::Error::other)??;
        let Some(t) = t else {
            if !self.held_logged {
                rbitcoin_log::info!("sv2: holding templates until the node leaves IBD");
                self.held_logged = true;
            }
            return Ok(());
        };
        self.templates.last_id += 1;
        let template_id = self.templates.last_id;
        // sv2-spec 07 §7.3: a template on a new prev hash is future, then activated.
        let new_prev = self.templates.current_prev != Some(t.prev_hash);
        let msg = t
            .to_message(template_id, new_prev)
            .map_err(|e| io::Error::other(format!("sv2 NewTemplate: {e:?}")))?;
        self.conn.send(MESSAGE_TYPE_NEW_TEMPLATE, msg).await?;
        self.templates.built_since_tip = true;
        if new_prev {
            self.conn
                .send(MESSAGE_TYPE_SET_NEW_PREV_HASH, t.to_prev_hash(template_id))
                .await?;
            let now = Instant::now();
            self.templates.current_prev = Some(t.prev_hash);
            self.templates.prev_sent = Some((t.header_timestamp, now));
            self.templates.start_grace(now + self.stale_grace);
        }
        let prev_sent = self
            .templates
            .prev_sent
            .ok_or_else(|| io::Error::other("sv2: template before SetNewPrevHash"))?;
        self.templates.retain(template_id, t, prev_sent);
        Ok(())
    }

    /// A bad solution is logged and dropped; a decodable one on a retained
    /// template always goes to `ChainHub::accept_block`.
    async fn on_submit_solution(&mut self, mut frame: Frame) -> io::Result<()> {
        let Ok(m) = binary_sv2::from_bytes::<SubmitSolution>(&mut frame.payload) else {
            rbitcoin_log::info!("sv2: undecodable SubmitSolution");
            return Ok(());
        };
        let Some(r) = self.templates.get(m.template_id) else {
            rbitcoin_log::info!("sv2: SubmitSolution for unknown template {}", m.template_id);
            return Ok(());
        };
        // The client may roll time forward from the sent timestamp by at
        // most the wall time since it was sent.
        let (sent_ts, sent_at) = r.prev_sent;
        let rolled =
            u64::try_from(sent_at.elapsed().as_millis().div_ceil(1000)).unwrap_or(u64::MAX);
        if u64::from(m.header_timestamp) < u64::from(sent_ts)
            || u64::from(m.header_timestamp) > u64::from(sent_ts).saturating_add(rolled)
        {
            rbitcoin_log::info!(
                "sv2: SubmitSolution header_timestamp {} outside [{sent_ts}, +{rolled}s]",
                m.header_timestamp
            );
            return Ok(());
        }
        let Ok(coinbase) = bitcoin::consensus::deserialize::<Transaction>(m.coinbase_tx.as_ref())
        else {
            rbitcoin_log::info!("sv2: undecodable SubmitSolution coinbase");
            return Ok(());
        };
        let mut txdata = Vec::with_capacity(1 + r.t.txs.len());
        txdata.push(coinbase);
        txdata.extend(r.t.txs.iter().cloned());
        let txids: Vec<[u8; 32]> = txdata
            .iter()
            .map(|tx| tx.compute_txid().to_byte_array())
            .collect();
        let block = Block {
            header: block::Header {
                version: block::Version::from_consensus(m.version as i32),
                prev_blockhash: BlockHash::from_byte_array(r.t.prev_hash),
                merkle_root: TxMerkleNode::from_byte_array(merkle_root_from_txids(&txids)),
                time: m.header_timestamp,
                bits: CompactTarget::from_consensus(r.t.n_bits),
                nonce: m.header_nonce,
            },
            txdata,
        };
        let hash = block.block_hash();
        let c = Arc::clone(&self.chain);
        let outcome = tokio::task::spawn_blocking(move || {
            let _g = BlockingRegion::enter();
            c.accept_block(block)
        })
        .await
        .map_err(io::Error::other)?;
        match outcome {
            Ok(o) => rbitcoin_log::info!("sv2: SubmitSolution {hash}: {o:?}"),
            Err(e) => rbitcoin_log::info!("sv2: SubmitSolution {hash} rejected: {e}"),
        }
        Ok(())
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

/// `false`: close the session.
async fn on_setup(conn: &mut NoiseConn, mut frame: Frame) -> io::Result<bool> {
    if frame.msg_type != MESSAGE_TYPE_SETUP_CONNECTION {
        rbitcoin_log::info!("sv2: message {:#x} before SetupConnection", frame.msg_type);
        return Ok(false);
    }
    let Ok(setup) = binary_sv2::from_bytes::<SetupConnection>(&mut frame.payload) else {
        rbitcoin_log::info!("sv2: undecodable SetupConnection");
        return Ok(false);
    };
    if let Some((flags, code)) = setup_error(&setup) {
        let error_code = Str0255::try_from(code)
            .map_err(|e| io::Error::other(format!("sv2 error code: {e:?}")))?;
        let reply = SetupConnectionError { flags, error_code };
        conn.send(MESSAGE_TYPE_SETUP_CONNECTION_ERROR, reply)
            .await?;
        return Ok(false);
    }
    let reply = SetupConnectionSuccess {
        used_version: TDP_VERSION,
        flags: 0,
    };
    conn.send(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, reply)
        .await?;
    Ok(true)
}

/// An id this session was sent but no longer retains is stale; any other
/// unknown id was never sent.
async fn on_request_transaction_data(
    conn: &mut NoiseWriter,
    mut frame: Frame,
    templates: &Templates,
) -> io::Result<()> {
    let Ok(RequestTransactionData { template_id }) = binary_sv2::from_bytes(&mut frame.payload)
    else {
        rbitcoin_log::info!("sv2: undecodable RequestTransactionData");
        return Ok(());
    };
    let Some(Retained { t, .. }) = templates.get(template_id) else {
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

#[cfg(test)]
mod tests {
    use super::Templates;

    #[test]
    fn tip_event_rebuilds_a_template_built_before_it() {
        let (old, new) = ([1; 32], [2; 32]);
        let mut t = Templates::default();
        assert!(t.on_tip(old), "first tip");
        t.current_prev = Some(old);
        assert!(!t.on_tip(old), "repeat event with no build since");
        // A build between the store tip move and its event (constraints, or
        // the previous event's rebuild) already sits on the new prev hash.
        t.current_prev = Some(new);
        t.built_since_tip = true;
        assert!(t.on_tip(new), "template may hold the block's confirmed txs");
        assert!(!t.on_tip(new), "no build since the event");
        // A failed reorg disconnects `new` without an event, then its rollback
        // reconnects `new`; a build in that window precedes the second event.
        t.built_since_tip = true;
        assert!(t.on_tip(new), "reconnect of the same hash after a build");
    }
}
