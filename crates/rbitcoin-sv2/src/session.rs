//! One TDP session: Noise handshake, `SetupConnection`, then TDP messages.

use crate::job::{self, Verdict};
use crate::messages::{
    ProposeTemplateError, ProposeTemplateMissingTransactions, ProposeTemplateSuccess,
    MESSAGE_TYPE_PROPOSE_TEMPLATE, MESSAGE_TYPE_PROPOSE_TEMPLATE_ERROR,
    MESSAGE_TYPE_PROPOSE_TEMPLATE_MISSING_TRANSACTIONS, MESSAGE_TYPE_PROPOSE_TEMPLATE_SUCCESS,
    REQUIRES_JOB_VALIDATION,
};
use crate::template;
use crate::transport::{Frame, NoiseConn, NoiseWriter};
use crate::Sv2TpStats;
use binary_sv2::{Seq064K, Str0255, B016M, B064K};
use bitcoin::hashes::Hash;
use bitcoin::{block, Block, BlockHash, CompactTarget, Transaction, TxMerkleNode, Witness};
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

/// RAM trade (docs/sv2-template-provider.md): each session keeps its
/// templates, so `RequestTransactionData` and `SubmitSolution` do not depend
/// on the mempool still holding their txs. Fee pushes add one per interval
/// on the same tip and a miner may still be on any of them, so the bound is
/// a count, oldest (replaced tips first) dropped. Bodies are the mempool's
/// `Arc`s: a template costs a pointer per tx until its txs leave the pool.
const MAX_RETAINED: usize = 64;

/// CPU trade: a `CoinbaseOutputConstraints` within this long of the last
/// build waits out the rest of it, so a client cycling budgets costs at most
/// one mempool-locked build per interval; a changed budget can lag by up to
/// this long. Tip events are not delayed.
const CONSTRAINTS_COOLDOWN: Duration = Duration::from_secs(1);

/// Budgets that replace a still-queued one before it is built. A real client
/// changes its budget minutes apart; past this many inside one cooldown the
/// session is closed so the slot goes back to a real client.
const MAX_SUPERSEDED_CONSTRAINTS: u32 = 8;

/// Fee-delta push settings (docs/sv2-template-provider.md C1).
#[derive(Clone, Copy)]
pub(crate) struct FeePush {
    pub delta: u64,
    pub interval: Duration,
}

/// The templates one session was sent. Ids are strictly increasing.
#[derive(Default)]
struct Templates {
    last_id: u64,
    current_prev: Option<[u8; 32]>,
    /// `SetNewPrevHash.header_timestamp` sent for `current_prev`, and when.
    prev_sent: Option<(u32, Instant)>,
    /// `SetNewPrevHash.n_bits` and `target` sent for `current_prev`.
    sent_bits: Option<(u32, [u8; 32])>,
    /// A template was built since the last tip event.
    built_since_tip: bool,
    /// `coinbase_tx_value_remaining` of the last `NewTemplate` sent; the fee
    /// delta is against it, not against a validated job retained after it.
    last_value_remaining: Option<u64>,
    retained: VecDeque<Retained>,
}

struct Retained {
    id: u64,
    job: template::Job,
    prev_sent: (u32, Instant),
    /// Set once the tip moves on.
    retire_at: Option<Instant>,
}

impl Templates {
    fn retain(&mut self, id: u64, job: template::Job, prev_sent: (u32, Instant)) {
        if self.retained.len() == MAX_RETAINED {
            self.retained.pop_front();
        }
        self.retained.push_back(Retained {
            id,
            job,
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

#[allow(clippy::too_many_arguments)] // call-site args stay unbundled
pub(crate) async fn serve(
    stream: TcpStream,
    responder: Box<Responder>,
    chain: Arc<ChainHub>,
    stale_grace: Duration,
    setup_timeout: Duration,
    write_timeout: Duration,
    fee_push: FeePush,
    stats: Arc<Sv2TpStats>,
) -> io::Result<()> {
    let deadline = Instant::now() + setup_timeout;
    let setup = async {
        let mut conn = NoiseConn::accept(stream, responder, write_timeout).await?;
        let frame = conn.recv().await?;
        Ok::<_, io::Error>(on_setup(&mut conn, frame).await?.map(|flags| (conn, flags)))
    };
    let Some((conn, flags)) = tokio::time::timeout_at(deadline, setup)
        .await
        .map_err(|_| missed_setup_deadline())??
    else {
        return Ok(());
    };
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
        fee_push,
        stats,
        job_validation: flags & REQUIRES_JOB_VALIDATION != 0,
        constraints: None,
        templates: Templates::default(),
        built_at: None,
        rebuild_at: None,
        superseded: 0,
        holding: false,
        held_logged: false,
        fee_check_at: None,
        seen_updates: 0,
    };
    s.run(&mut frames, deadline).await
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn missed_setup_deadline() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "sv2: setup deadline")
}

struct Session {
    conn: NoiseWriter,
    chain: Arc<ChainHub>,
    stale_grace: Duration,
    fee_push: FeePush,
    stats: Arc<Sv2TpStats>,
    /// `SetupConnection` negotiated `REQUIRES_JOB_VALIDATION`.
    job_validation: bool,
    /// Last `CoinbaseOutputConstraints`: `(max_additional_size, sigops)`.
    constraints: Option<(u32, u16)>,
    templates: Templates,
    /// When `publish` last sent a template.
    built_at: Option<Instant>,
    /// Constraints-triggered rebuild deferred by `CONSTRAINTS_COOLDOWN`.
    rebuild_at: Option<Instant>,
    /// Budgets that replaced a queued rebuild's since the last build.
    superseded: u32,
    /// Last `publish` returned without a template because the node was in IBD.
    /// Cleared when a template is sent.
    holding: bool,
    held_logged: bool,
    /// Next fee check: one interval after the last push or check.
    fee_check_at: Option<Instant>,
    /// `MempoolHub::template_updates` read before the last build.
    seen_updates: u64,
}

impl Session {
    /// `setup_deadline` also covers the first `CoinbaseOutputConstraints`:
    /// until then the session can never get a template, so it never writes
    /// and the write deadline cannot free its slot.
    async fn run(
        &mut self,
        frames: &mut mpsc::Receiver<io::Result<Frame>>,
        setup_deadline: Instant,
    ) -> io::Result<()> {
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
                _ = tokio::time::sleep_until(self.rebuild_at.unwrap_or_else(Instant::now)),
                    if self.rebuild_at.is_some() =>
                {
                    self.publish().await?;
                }
                // CPU trade: `template_updates` is a counter, not a notifier,
                // so each session polls it once per interval and a moved
                // counter costs one build under the mempool read lock: at
                // most MAX_SESSIONS builds per interval.
                _ = tokio::time::sleep_until(self.fee_check_at.unwrap_or_else(Instant::now)),
                    if self.fee_check_at.is_some() =>
                {
                    self.check_fees().await?;
                }
                _ = tokio::time::sleep_until(setup_deadline), if self.constraints.is_none() => {
                    return Err(missed_setup_deadline());
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
                let c = Some((
                    c.coinbase_output_max_additional_size,
                    c.coinbase_output_max_additional_sigops,
                ));
                // Same budget: the last publish already applied it. A new
                // budget while holding for IBD waits for the tip that leaves
                // IBD; that tip calls `publish` and must not find a queued
                // rebuild or a flood close in its place.
                let same = c == self.constraints;
                self.constraints = c;
                if same || self.holding {
                    return Ok(true);
                }
                match self.built_at.map(|t| t + CONSTRAINTS_COOLDOWN) {
                    Some(at) if at > Instant::now() => {
                        if self.rebuild_at.replace(at).is_some() {
                            self.superseded += 1;
                            if self.superseded > MAX_SUPERSEDED_CONSTRAINTS {
                                rbitcoin_log::info!(
                                    "sv2: CoinbaseOutputConstraints flood, closing"
                                );
                                return Ok(false);
                            }
                        }
                    }
                    _ => self.publish().await?,
                }
            }
            MESSAGE_TYPE_REQUEST_TRANSACTION_DATA => {
                on_request_transaction_data(&mut self.conn, frame, &self.templates).await?;
            }
            MESSAGE_TYPE_SUBMIT_SOLUTION => self.on_submit_solution(frame).await?,
            MESSAGE_TYPE_PROPOSE_TEMPLATE if self.job_validation => {
                self.on_propose_template(frame).await?;
            }
            t => rbitcoin_log::info!("sv2: ignoring message {t:#x}"),
        }
        Ok(true)
    }

    /// Build on the current tip and send it.
    async fn publish(&mut self) -> io::Result<()> {
        self.rebuild_at = None;
        self.superseded = 0;
        match self.build().await? {
            Some(t) => self.send(t).await,
            None => Ok(()),
        }
    }

    /// With the tip unchanged, send a rebuild whose fees reach the last sent
    /// template's plus the delta. A rebuild on a new prev hash is dropped:
    /// its tip event publishes. A queued constraints rebuild publishes the
    /// latest mempool anyway, so the check leaves it alone.
    async fn check_fees(&mut self) -> io::Result<()> {
        self.fee_check_at = Some(Instant::now() + self.fee_push.interval);
        self.stats.fee_checks.note(0);
        let updates = self.chain.mempool().map_or(0, |m| m.template_updates());
        if updates == self.seen_updates || self.rebuild_at.is_some() {
            return Ok(());
        }
        // A check races ChainHub with no tip event in flight: a reorg pops the
        // tip block by block and only signals once the new branch connects,
        // so a tip read mid-disconnect can fail. The event that follows
        // publishes; one skipped check does not end the session, and the next
        // one retries the same mempool generation. No session reaches that
        // window deterministically.
        let t = match self.build().await {
            Ok(Some(t)) => t,
            Ok(None) => return Ok(()),
            Err(e) => {
                rbitcoin_log::info!("sv2: fee check skipped ({e})");
                return Ok(());
            }
        };
        let Some(last) = self.templates.last_value_remaining else {
            return Ok(());
        };
        // Same prev hash, same height: value_remaining differs by fees only.
        if self.templates.current_prev == Some(t.job.prev_hash)
            && t.value_remaining >= last.saturating_add(self.fee_push.delta)
        {
            self.send(t).await?;
        }
        Ok(())
    }

    /// Template for the session's constraints. `None` before constraints or
    /// while in IBD: leaving IBD always comes with a new tip, which publishes.
    async fn build(&mut self) -> io::Result<Option<template::Template>> {
        let Some((size, sigops)) = self.constraints else {
            return Ok(None);
        };
        // Read before the build: a tx admitted during it moves the counter
        // past this, so the next check rebuilds. Stored only once the build
        // returns: a failed one leaves the generation for the next check.
        let updates = self.chain.mempool().map_or(0, |m| m.template_updates());
        let c = Arc::clone(&self.chain);
        let stats = Arc::clone(&self.stats);
        let t = tokio::task::spawn_blocking(move || {
            let _g = BlockingRegion::enter();
            if c.in_ibd() {
                return Ok(None);
            }
            let started = std::time::Instant::now();
            let t = template::build(&c, size, sigops)?;
            stats.builds.note(micros(started.elapsed()));
            Ok::<_, io::Error>(Some(t))
        })
        .await
        .map_err(io::Error::other)??;
        self.seen_updates = updates;
        if t.is_none() {
            self.holding = true;
            if !self.held_logged {
                rbitcoin_log::info!("sv2: holding templates until the node leaves IBD");
                self.held_logged = true;
            }
        }
        Ok(t)
    }

    async fn send(&mut self, mut t: template::Template) -> io::Result<()> {
        self.holding = false;
        self.templates.last_id += 1;
        let template_id = self.templates.last_id;
        // sv2-spec 07 §7.3: a template on a new prev hash is future, then activated.
        let new_prev = self.templates.current_prev != Some(t.job.prev_hash);
        // §7.4: nBits comes once per prev hash. On min-difficulty networks the
        // build's bits follow the clock; a solution on this template is hashed
        // with the bits the client was sent.
        if let Some((n_bits, target)) = self.templates.sent_bits.filter(|_| !new_prev) {
            t.job.n_bits = n_bits;
            t.job.target = target;
        }
        let msg = t
            .to_message(template_id, new_prev)
            .map_err(|e| io::Error::other(format!("sv2 NewTemplate: {e:?}")))?;
        self.conn.send(MESSAGE_TYPE_NEW_TEMPLATE, msg).await?;
        self.templates.last_value_remaining = Some(t.value_remaining);
        let sent = Instant::now();
        self.built_at = Some(sent);
        self.fee_check_at = Some(sent + self.fee_push.interval);
        self.templates.built_since_tip = true;
        if new_prev {
            self.conn
                .send(MESSAGE_TYPE_SET_NEW_PREV_HASH, t.to_prev_hash(template_id))
                .await?;
            let now = Instant::now();
            self.templates.current_prev = Some(t.job.prev_hash);
            self.templates.prev_sent = Some((t.job.header_timestamp, now));
            self.templates.sent_bits = Some((t.job.n_bits, t.job.target));
            self.templates.start_grace(now + self.stale_grace);
        }
        let prev_sent = self
            .templates
            .prev_sent
            .ok_or_else(|| io::Error::other("sv2: template before SetNewPrevHash"))?;
        self.templates.retain(template_id, t.job, prev_sent);
        Ok(())
    }

    /// A bad solution is logged and dropped; a decodable one on a retained
    /// template that meets its target always goes to `ChainHub::accept_block`.
    async fn on_submit_solution(&mut self, mut frame: Frame) -> io::Result<()> {
        let Ok(m) = binary_sv2::from_bytes::<SubmitSolution>(&mut frame.payload) else {
            rbitcoin_log::info!("sv2: undecodable SubmitSolution");
            return Ok(());
        };
        let Some(r) = self.templates.get(m.template_id) else {
            rbitcoin_log::info!("sv2: SubmitSolution for unknown template {}", m.template_id);
            return Ok(());
        };
        let Ok(mut coinbase) =
            bitcoin::consensus::deserialize::<Transaction>(m.coinbase_tx.as_ref())
        else {
            rbitcoin_log::info!("sv2: undecodable SubmitSolution coinbase");
            return Ok(());
        };
        // BIP141: a committed block's coinbase witness is exactly the reserved
        // value. A client may omit it; the commitment used ours, so fill it.
        // Without a commitment output a coinbase witness is invalid, so a bare
        // coinbase stays bare. The txid (and the merkle root) does not cover
        // the witness.
        let committed = rbitcoin_consensus::witness_commitment_vout_index(&coinbase).is_some();
        if let Some(input) = coinbase.input.first_mut() {
            if committed && input.witness.is_empty() {
                input.witness = Witness::from_slice(&[template::WITNESS_RESERVED_VALUE]);
            }
        }
        // CPU trade: the coinbase is leaf 0 (always the left child), so its
        // txid folded over the template's path is the root the full txid
        // list would give. A miss on the target costs one fold, not a clone
        // and hash of every template tx plus ChainHub's connect path.
        let root = rbitcoin_store::merkle_root_from_branch(
            coinbase.compute_txid().to_byte_array(),
            &r.job.merkle_path,
            0,
        );
        let header = block::Header {
            version: block::Version::from_consensus(m.version as i32),
            prev_blockhash: BlockHash::from_byte_array(r.job.prev_hash),
            merkle_root: TxMerkleNode::from_byte_array(root),
            time: m.header_timestamp,
            bits: CompactTarget::from_consensus(r.job.n_bits),
            nonce: m.header_nonce,
        };
        if header.validate_pow(header.target()).is_err() {
            rbitcoin_log::info!(
                "sv2: SubmitSolution {} misses the template target",
                header.block_hash()
            );
            return Ok(());
        }
        // Diagnostic only: a miner clock ahead of ours rolls past the wall
        // time since SetNewPrevHash. accept_block enforces the consensus
        // bounds (> MTP, < now + 2h), so the block is still submitted.
        let (sent_ts, sent_at) = r.prev_sent;
        let rolled =
            u64::try_from(sent_at.elapsed().as_millis().div_ceil(1000)).unwrap_or(u64::MAX);
        if u64::from(m.header_timestamp) < u64::from(sent_ts)
            || u64::from(m.header_timestamp) > u64::from(sent_ts).saturating_add(rolled)
        {
            rbitcoin_log::info!(
                "sv2: SubmitSolution template {} header_timestamp {} outside [{sent_ts}, +{rolled}s]; submitting",
                m.template_id,
                m.header_timestamp
            );
        }
        let mut txdata = Vec::with_capacity(1 + r.job.txs.len());
        txdata.push(coinbase);
        txdata.extend(r.job.txs.iter().map(|tx| Transaction::clone(tx)));
        let block = Block { header, txdata };
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

    /// docs/sv2-job-validation.md §4: a valid job is retained under the next
    /// template id and answered `Success`; a rejected one answers `Error`.
    async fn on_propose_template(&mut self, frame: Frame) -> io::Result<()> {
        let c = Arc::clone(&self.chain);
        let verdict = tokio::task::spawn_blocking(move || {
            let _g = BlockingRegion::enter();
            job::validate(&c, frame.payload)
        })
        .await
        .map_err(io::Error::other)??;
        let Some((request_id, verdict)) = verdict else {
            rbitcoin_log::info!("sv2: undecodable ProposeTemplate");
            return Ok(());
        };
        let wire = |e: binary_sv2::Error| io::Error::other(format!("sv2 ProposeTemplate: {e:?}"));
        match verdict {
            Verdict::Missing(positions) => {
                let reply = ProposeTemplateMissingTransactions {
                    request_id,
                    unknown_tx_position_list: Seq064K::new(positions).map_err(wire)?,
                };
                self.conn
                    .send(MESSAGE_TYPE_PROPOSE_TEMPLATE_MISSING_TRANSACTIONS, reply)
                    .await
            }
            Verdict::Valid { fees, job } => {
                self.templates.last_id += 1;
                let template_id = self.templates.last_id;
                let prev_sent = self
                    .templates
                    .prev_sent
                    .unwrap_or((job.header_timestamp, Instant::now()));
                self.templates.retain(template_id, job, prev_sent);
                let reply = ProposeTemplateSuccess {
                    request_id,
                    template_id,
                    fees,
                };
                self.conn
                    .send(MESSAGE_TYPE_PROPOSE_TEMPLATE_SUCCESS, reply)
                    .await
            }
            Verdict::Rejected(code) => {
                let reply = ProposeTemplateError {
                    request_id,
                    error_code: Str0255::try_from(code.as_str()).map_err(wire)?,
                    error_details: B064K::try_from(&[][..]).map_err(wire)?,
                };
                self.conn
                    .send(MESSAGE_TYPE_PROPOSE_TEMPLATE_ERROR, reply)
                    .await
            }
        }
    }
}

fn setup_error(m: &SetupConnection) -> Option<(u32, &'static str)> {
    if m.protocol != Protocol::TemplateDistributionProtocol {
        return Some((0, ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL));
    }
    if !(m.min_version..=m.max_version).contains(&TDP_VERSION) {
        return Some((0, ERROR_CODE_SETUP_CONNECTION_PROTOCOL_VERSION_MISMATCH));
    }
    // docs/sv2-job-validation.md §3 defines bit 0; every other set bit is
    // unsupported.
    if m.flags & !REQUIRES_JOB_VALIDATION != 0 {
        return Some((
            m.flags,
            ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_FEATURE_FLAGS,
        ));
    }
    None
}

/// The negotiated flags; `None`: close the session.
async fn on_setup(conn: &mut NoiseConn, mut frame: Frame) -> io::Result<Option<u32>> {
    if frame.msg_type != MESSAGE_TYPE_SETUP_CONNECTION {
        rbitcoin_log::info!("sv2: message {:#x} before SetupConnection", frame.msg_type);
        return Ok(None);
    }
    let Ok(setup) = binary_sv2::from_bytes::<SetupConnection>(&mut frame.payload) else {
        rbitcoin_log::info!("sv2: undecodable SetupConnection");
        return Ok(None);
    };
    if let Some((flags, code)) = setup_error(&setup) {
        let error_code = Str0255::try_from(code)
            .map_err(|e| io::Error::other(format!("sv2 error code: {e:?}")))?;
        let reply = SetupConnectionError { flags, error_code };
        conn.send(MESSAGE_TYPE_SETUP_CONNECTION_ERROR, reply)
            .await?;
        return Ok(None);
    }
    let reply = SetupConnectionSuccess {
        used_version: TDP_VERSION,
        flags: setup.flags,
    };
    conn.send(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, reply)
        .await?;
    Ok(Some(setup.flags))
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
    let Some(Retained { job, .. }) = templates.get(template_id) else {
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
    let raw: Vec<Vec<u8>> = job
        .txs
        .iter()
        .map(|tx| bitcoin::consensus::encode::serialize(tx.as_ref()))
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
