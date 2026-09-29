use crate::test_chain::{padded_chain, TestChain};
use crate::testutil::TpClient;
use crate::{run_sv2_tp, Sv2TpConfig};
use bitcoin::consensus::encode::serialize;
use bitcoin::hashes::{sha256d, Hash};
use bitcoin::{
    absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence, Transaction,
    TxIn, TxOut, Txid, Witness,
};
use bitcoin::{CompactTarget, Target};
use rbitcoin_consensus::{
    bip34_height_script, block_subsidy, expected_next_bits, median_time_past, mine_empty_regtest,
    witness_commitment_script,
};
use rbitcoin_primitives::Height;
use rbitcoin_store::merkle_root_from_txids;
use std::sync::Arc;
use std::time::Duration;
use template_distribution_sv2::{
    NewTemplate, SetNewPrevHash, MESSAGE_TYPE_NEW_TEMPLATE, MESSAGE_TYPE_SET_NEW_PREV_HASH,
};

const MAX_BLOCK_WEIGHT: u64 = 4_000_000;
const OP_TRUE: u8 = 0x51;
const OP_CHECKSIG: u8 = 0xac;

fn spend(coinbase: Txid, fee: u64, script_pubkey: ScriptBuf) -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: coinbase,
                vout: 0,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(50_0000_0000 - fee),
            script_pubkey,
        }],
    }
}

/// Coinbase is leaf 0, so every fold step hashes the running value on the left.
fn fold_coinbase_path(leaf: [u8; 32], path: &[[u8; 32]]) -> [u8; 32] {
    path.iter().fold(leaf, |acc, sibling| {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&acc);
        buf[32..].copy_from_slice(sibling);
        sha256d::Hash::hash(&buf).to_byte_array()
    })
}

async fn recv_in_time(c: &mut TpClient) -> crate::Frame {
    tokio::time::timeout(Duration::from_secs(10), c.recv())
        .await
        .expect("message in time")
        .expect("message")
}

async fn expect_template(
    c: &mut TpClient,
    tc: &TestChain,
    txs: &[&Transaction],
    future: bool,
) -> u64 {
    let f = recv_in_time(c).await;
    check_template(c, tc, f, txs, future).await
}

/// `future` templates must be followed by their `SetNewPrevHash` on the tip.
async fn check_template(
    c: &mut TpClient,
    tc: &TestChain,
    mut f: crate::Frame,
    txs: &[&Transaction],
    future: bool,
) -> u64 {
    assert_eq!(f.msg_type, MESSAGE_TYPE_NEW_TEMPLATE);
    let t: NewTemplate = binary_sv2::from_bytes(&mut f.payload).expect("decode");
    let next_h = tc.chain.query.tip_height().expect("tip").0 + 1;
    let fees: u64 = txs
        .iter()
        .map(|tx| 50_0000_0000 - tx.output[0].value.to_sat())
        .sum();

    assert_eq!(t.future_template, future);
    assert_eq!(t.version, tc.chain.gbt_block_version() as u32);
    assert_eq!(
        (
            t.coinbase_tx_version,
            t.coinbase_tx_input_sequence,
            t.coinbase_tx_locktime
        ),
        (2, u32::MAX, 0)
    );
    assert_eq!(t.coinbase_prefix.as_ref(), bip34_height_script(next_h));
    assert_eq!(
        t.coinbase_tx_value_remaining,
        block_subsidy(next_h, &tc.chain.params) as u64 + fees
    );
    let commitment = TxOut {
        value: Amount::ZERO,
        script_pubkey: ScriptBuf::from_bytes(witness_commitment_script(
            txs.iter().map(|tx| tx.compute_wtxid().to_byte_array()),
            &[0u8; 32],
        )),
    };
    assert_eq!(t.coinbase_tx_outputs_count, 1);
    assert_eq!(t.coinbase_tx_outputs.as_ref(), serialize(&commitment));

    let path: Vec<[u8; 32]> = t
        .merkle_path
        .iter()
        .map(|h| h.as_ref().try_into().expect("32-byte hash"))
        .collect();
    let coinbase_leaf = [0x11; 32];
    let mut leaves = vec![coinbase_leaf];
    leaves.extend(txs.iter().map(|tx| tx.compute_txid().to_byte_array()));
    assert_eq!(
        fold_coinbase_path(coinbase_leaf, &path),
        merkle_root_from_txids(&leaves),
        "merkle path must fold to the root over the selection order"
    );
    if future {
        expect_prev_hash(c, tc, t.template_id, next_h).await;
    }
    t.template_id
}

async fn expect_prev_hash(c: &mut TpClient, tc: &TestChain, template_id: u64, next_h: u32) {
    let mut f = recv_in_time(c).await;
    assert_eq!(f.msg_type, MESSAGE_TYPE_SET_NEW_PREV_HASH);
    let p: SetNewPrevHash = binary_sv2::from_bytes(&mut f.payload).expect("decode");
    assert_eq!(p.template_id, template_id);
    let tip = tc.chain.tip_header().expect("tip header");
    assert_eq!(p.prev_hash.as_ref(), tip.block_hash().to_byte_array());
    let mtp = median_time_past(&tc.chain.query, Height(next_h - 1)).unwrap();
    assert!(p.header_timestamp > mtp, "header_timestamp above MTP");
    assert!(u64::from(p.header_timestamp) <= tc.chain.clock.now_secs());
    let bits = expected_next_bits(
        &tc.chain.query,
        &tc.chain.params,
        Height(next_h),
        p.header_timestamp,
    )
    .unwrap();
    assert_eq!(p.n_bits, bits.to_consensus());
    let target = Target::from_compact(CompactTarget::from_consensus(p.n_bits));
    assert_eq!(p.target.as_ref(), target.to_le_bytes());
}

#[tokio::test(flavor = "multi_thread")]
async fn template_budget_fees_coinbase_and_merkle_path() {
    let tc = padded_chain("sv2-template", 3);
    // Tip time as "now": the padded chain is out of IBD for this test.
    tc.chain.clock.set_mock(i64::from(tc.tip_time));
    let cheap = ScriptBuf::from_bytes(vec![OP_TRUE]);
    let a = spend(tc.coinbases[0], 3_000, cheap.clone());
    let b = spend(tc.coinbases[1], 2_000, cheap);
    // 4000 legacy sigops × 4 = 16_000 cost; Libre admits the script.
    let heavy = spend(
        tc.coinbases[2],
        20_000,
        ScriptBuf::from_bytes(vec![OP_CHECKSIG; 4_000]),
    );
    for tx in [&a, &b, &heavy] {
        tc.mempool.accept_tx(tx).expect("mempool accept");
    }

    let tp = run_sv2_tp(Sv2TpConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        chain: Arc::clone(&tc.chain),
        authority_secret: [7; 32],
        cert_validity: Duration::from_secs(3600),
    })
    .await
    .expect("listen");
    let mut c = TpClient::connect(tp.local_addr, tp.authority_pubkey)
        .await
        .expect("handshake");
    c.setup_connection(2, 2, 2, 0).await.unwrap();
    c.recv().await.expect("setup reply");

    c.coinbase_output_constraints(0, 0).await.unwrap();
    let mut last = expect_template(&mut c, &tc, &[&a, &b, &heavy], true).await;

    // The client's sigops replace the reserve: 65_535 + 16_000 ≥ 80_000.
    c.coinbase_output_constraints(0, u16::MAX).await.unwrap();
    let id = expect_template(&mut c, &tc, &[&a, &b], false).await;
    assert!(id > last, "template_id must increase");
    last = id;

    // Reserved weight 1168 + 4·size leaves exactly a + b, then 4 WU less.
    let edge = MAX_BLOCK_WEIGHT - 1168 - a.weight().to_wu() - b.weight().to_wu();
    let size = u32::try_from(edge / 4).unwrap();
    c.coinbase_output_constraints(size, 0).await.unwrap();
    let id = expect_template(&mut c, &tc, &[&a, &b], false).await;
    assert!(id > last, "template_id must increase");
    last = id;
    c.coinbase_output_constraints(size + 1, 0).await.unwrap();
    let id = expect_template(&mut c, &tc, &[&a], false).await;
    assert!(id > last, "template_id must increase");

    tp.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_gate_holds_constraints_until_a_fresh_tip() {
    let tc = padded_chain("sv2-gate", 0);
    let tp = run_sv2_tp(Sv2TpConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        chain: Arc::clone(&tc.chain),
        authority_secret: [7; 32],
        cert_validity: Duration::from_secs(3600),
    })
    .await
    .expect("listen");
    let mut c = TpClient::connect(tp.local_addr, tp.authority_pubkey)
        .await
        .expect("handshake");
    c.setup_connection(2, 2, 2, 0).await.unwrap();
    c.recv().await.expect("setup reply");
    c.coinbase_output_constraints(0, 0).await.unwrap();
    // One recv future throughout: dropping it mid-frame breaks the Noise decoder.
    let first = {
        let recv = c.recv();
        tokio::pin!(recv);
        let held = tokio::time::timeout(Duration::from_millis(500), &mut recv).await;
        assert!(held.is_err(), "no template while the stale tip keeps IBD");

        let tip = tc.chain.tip_header().expect("tip header");
        let height = tc.chain.query.tip_height().unwrap().0 + 1;
        let now = tc.chain.clock.now_secs() as u32;
        let fresh = mine_empty_regtest(tip.block_hash(), now, height);
        tc.chain.accept_block(fresh).expect("accept fresh block");
        tokio::time::timeout(Duration::from_secs(10), recv)
            .await
            .expect("template after the fresh tip")
            .expect("message")
    };
    check_template(&mut c, &tc, first, &[], true).await;

    tp.shutdown().await;
}
