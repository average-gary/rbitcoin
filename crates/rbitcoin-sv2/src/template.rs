//! One TDP template: budgeted mempool selection plus the coinbase split
//! (sv2-spec 07 §7.1–7.2).

use binary_sv2::{Seq0255, B0255, B064K, U256};
use bitcoin::consensus::encode::serialize;
use bitcoin::hashes::Hash;
use bitcoin::{Amount, ScriptBuf, TxOut};
use rbitcoin_consensus::{
    bip34_height_script, block_subsidy, witness_commitment_script, MAX_BLOCK_WEIGHT,
};
use rbitcoin_net::{ChainHub, SelectBudget};
use template_distribution_sv2::NewTemplate;

/// sv2-spec 07 §7.1: coinbase weight outside the client's additional outputs,
/// and the floor on the whole reserve.
const COINBASE_BASE_WU: u64 = 1168;
const MIN_COINBASE_RESERVE_WU: u64 = 2000;

pub(crate) struct Template {
    pub version: u32,
    pub coinbase_prefix: Vec<u8>,
    pub value_remaining: u64,
    /// The witness commitment output, serialized with no count prefix.
    pub coinbase_outputs: Vec<u8>,
    pub merkle_path: Vec<[u8; 32]>,
}

impl Template {
    /// Coinbase fields not carried by the record are fixed: version 2, one
    /// final input, zero locktime.
    pub fn to_message(
        &self,
        template_id: u64,
        future_template: bool,
    ) -> Result<NewTemplate<'_>, binary_sv2::Error> {
        Ok(NewTemplate {
            template_id,
            future_template,
            version: self.version,
            coinbase_tx_version: 2,
            coinbase_prefix: B0255::try_from(&self.coinbase_prefix[..])?,
            coinbase_tx_input_sequence: u32::MAX,
            coinbase_tx_value_remaining: self.value_remaining,
            coinbase_tx_outputs_count: 1,
            coinbase_tx_outputs: B064K::try_from(&self.coinbase_outputs[..])?,
            coinbase_tx_locktime: 0,
            merkle_path: Seq0255::new(self.merkle_path.iter().map(U256::from).collect())?,
        })
    }
}

/// Template on the current tip for one client's coinbase constraints. The
/// client's sigops replace the default reserve (Core `BlockAssembler`).
/// Takes the mempool lock: blocking region only.
pub(crate) fn build(
    chain: &ChainHub,
    max_additional_size: u32,
    max_additional_sigops: u16,
) -> Template {
    let reserve =
        (COINBASE_BASE_WU + 4 * u64::from(max_additional_size)).max(MIN_COINBASE_RESERVE_WU);
    let budget = SelectBudget {
        max_weight_wu: MAX_BLOCK_WEIGHT.saturating_sub(reserve),
        reserved_sigops: u64::from(max_additional_sigops),
        min_sat_kvb: chain.block_min_tx_fee_sat_kvb(),
    };
    let height = chain.query.tip_height().map_or(0, |h| h.0 + 1);
    let selected = chain
        .mempool()
        .map(|m| m.select_block_template(budget))
        .unwrap_or_default();
    let fees: u64 = selected.iter().map(|(_, s)| s.fee_sat).sum();
    let commitment = TxOut {
        value: Amount::ZERO,
        script_pubkey: ScriptBuf::from_bytes(witness_commitment_script(
            selected
                .iter()
                .map(|(tx, _)| tx.compute_wtxid().to_byte_array()),
            &[0u8; 32],
        )),
    };
    let mut leaves = vec![[0u8; 32]];
    leaves.extend(selected.iter().map(|(_, s)| s.txid.to_byte_array()));
    Template {
        version: chain.gbt_block_version() as u32,
        coinbase_prefix: bip34_height_script(height),
        value_remaining: (block_subsidy(height, &chain.params) as u64).saturating_add(fees),
        coinbase_outputs: serialize(&commitment),
        merkle_path: rbitcoin_store::merkle_branch(&leaves, 0),
    }
}
