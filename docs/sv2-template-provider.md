# SV2 template provider (TDP server)

Plan for **Q-64**. Cycle and step shape:
[`how-we-plan.md`](./how-we-plan.md#agent-contract). One plan, one PR. Do
not start a step before the previous slice is committed.

## Goal

Operator runs `rbitcoin-node --sv2-tp-listen 127.0.0.1:8442
--sv2-tp-authority-sec <key>`. A Stratum v2 Job Declarator Client or pool
connects over Noise_NX TCP, completes `SetupConnection(protocol=2)`, sends
`CoinbaseOutputConstraints`, and from then on is **pushed** `NewTemplate` /
`SetNewPrevHash` when the tip changes and when template fees rise by a
configured delta. `RequestTransactionData` returns the retained template's
transactions; `SubmitSolution` assembles the full block and submits it
through the normal accept path. TDP replaces `getblocktemplate` polling for
these clients (sv2-spec 07).

Reference state machine: sv2-apps `bitcoin-core-sv2` (Bitcoin Core IPC →
TDP). This TP is in-process: the template source is the node's own mempool
and tip, no IPC hop. Wire and Noise are the published `stratum-core`
crates, unchanged.

## Constraints

- New crate `crates/rbitcoin-sv2`, service pattern of electrum/esplora:
  depends on `rbitcoin-query` / `rbitcoin-net` / `rbitcoin-mempool` /
  `rbitcoin-consensus`; wired in `rbitcoin-node` `run.rs` behind flags.
  Nothing starts without `--sv2-tp-listen`.
- Reuse `stratum-core` wire crates: `noise_sv2`, `codec_sv2`,
  `framing_sv2`, `binary_sv2`, `template_distribution_sv2`, `parsers_sv2`,
  `common_messages_sv2`. They must pass `cargo deny` (musl operator
  binary). Hand-rolling the TDP codec is the fallback, not the default.
- `binary_sv2` byte-buffer types stay at the session boundary; consensus
  decode uses the workspace `rust-bitcoin` inside the crate. No second
  `bitcoin` version's types in other crates' APIs.
- Reactor rule: template builds and solution assembly run in a blocking
  region, never on tokio workers. `MempoolHub` accessors assert
  not-reactor.
- Named RAM trade (CONTRIBUTING 9): each session retains, per live
  template, the full witness-serialized non-coinbase txs (≤ ~4 MB × ~3
  templates × sessions). Retention is required: the mempool may evict a tx
  before `RequestTransactionData` or `SubmitSolution` arrives. 10 s stale
  grace after a tip change, then drop (mirrors sv2-tp).
- Per-session reservation: selection budget is
  `MAX_BLOCK_WEIGHT − max(1168 + 4·coinbase_output_max_additional_size,
  2000)` WU (sv2-spec 07 §7.1) and `coinbase_output_max_additional_sigops`
  off the sigops budget. Each connected client sizes its own templates.
- Noise is the only mode (mandatory for remote TDP). No plaintext operator
  flag; tests drive the shipped Noise path.
- TDP defines no `SetupConnection` flags: nonzero `flags` →
  `SetupConnection.Error` echoing the full unsupported set; `protocol != 2`
  or no version-2 overlap → Error and close.
- `template_id` strictly increasing per session.
- Coinbase split (sv2-spec 07 §7.2): `coinbase_prefix` is the BIP34 height
  push (≤ 8 bytes, start of scriptSig); `coinbase_tx_value_remaining` =
  subsidy + Σ fees; `coinbase_tx_outputs` is the raw concatenation (no
  CompactSize prefix) with the witness-commitment OP_RETURN **last**;
  commitment computed from the template wtxids with a 32-byte zero reserved
  value (coinbase-independent, BIP141).
- `SetNewPrevHash.target` == nBits target here (no weak blocks).
- No templates before sync: same refusal gate as `getblocktemplate` during
  IBD.
- `SubmitSolution` has no error message in TDP: undecodable or
  unknown-template solutions are logged and dropped; decodable ones are
  always attempted through `ChainHub::accept_block` (the TP MUST try to
  broadcast work on its templates).
- `SubmitSolution.header_timestamp` pre-check: ≥ the sent
  `SetNewPrevHash.header_timestamp` and ≤ that plus wall-clock elapsed
  (sv2-spec 07 §7.7).
- OPERATOR / COMPAT / NixOS options land in the step that ships the flag
  (same PR), not in this plan commit.

## Out of scope

Mining Protocol server (channels), Job Declaration **Server**, SV1↔SV2
translator proxy, Job Declarator Client, weak-block targets below nBits,
extension negotiation, per-IP metering/rate limits. None of these ship;
nothing here precludes a later JD-server plan.

## Steps

### Step 0 — Spike: stratum-core dependency check

Time-boxed; throwaway manifest, no production change. Add the seven wire
crates to a scratch crate; run `cargo deny check`; inspect the lock delta
for a second `bitcoin` / `secp256k1` and record versions. Output: a written
go / no-go finding (reuse, or hand-roll and name the dep that forced it)
carried into step 1's commit message and this file's follow-ups.

### Step 1 — Crate skeleton, Noise responder, SetupConnection

- **Contract:** a `noise_sv2` initiator completing the NX handshake against
  the listener and sending `SetupConnection{protocol=2, min_version=2,
  max_version=2, flags=0}` receives `SetupConnection.Success{used_version=2,
  flags=0}`. Nonzero flags → `SetupConnection.Error` echoing them.
  `protocol != 2` or no version-2 overlap → Error and the connection
  closes.
- **Red:** `cargo test -p rbitcoin-sv2 setup_connection_` — loopback TCP,
  in-crate test initiator; success, bad-flags, bad-protocol cases.
- **Green:** `crates/rbitcoin-sv2` (workspace member): authority-keypair
  config, listener task, per-connection session task driving `codec_sv2`
  handshake then the common-message branch.
- **Refactor:** session state as an enum (`Handshake`,
  `AwaitingConstraints`, `Active`), not nested ifs.
- **Verify:** `cargo test -p rbitcoin-sv2 setup_`

### Step 2 — Merkle path helper in consensus

- **Contract:** `coinbase_merkle_path(txids)` returns the leftmost-branch
  hashes deepest-first; folding them with the coinbase txid reproduces
  `compute_merkle_root` for the same list. Edge cases: single tx (empty
  path), odd counts at every level.
- **Red:** `cargo test -p rbitcoin-consensus merkle_path_` — small known
  vectors.
- **Green:** helper next to the merkle-root code
  (`crates/rbitcoin-consensus/src/block/`).
- **Refactor:** share the level-pairing loop with the root computation if
  it dedupes without obscuring.
- **Verify:** `cargo test -p rbitcoin-consensus merkle_path_`

### Step 3 — Template builder with TDP coinbase

- **Contract:** `build(hub, tip, constraints) -> TemplateRecord` selects
  via `MempoolHub::select_block_txs` with budget
  `MAX_BLOCK_WEIGHT − max(1168 + 4·size, 2000)` WU; `coinbase_prefix` is
  the BIP34 height push; `value_remaining` = subsidy + Σ selected fees;
  outputs = witness commitment last; commitment matches independent
  recomputation; the record carries the serialized non-coinbase txs in
  selection order.
- **Red:** `cargo test -p rbitcoin-sv2 template_` — synthetic mempool
  (reuse `rbitcoin-mempool` accept fixtures): weight bound at the reserved
  edge, fee sum, prefix bytes, commitment, tx order.
- **Green:** builder module in `rbitcoin-sv2`; subsidy/params from
  `rbitcoin-consensus`.
- **Refactor:** one owner for witness-commitment construction shared with
  `rbitcoin-rpc` `methods/mine.rs` (lowest owning crate; GBT tests stay
  green).
- **Verify:** `cargo test -p rbitcoin-sv2 template_` plus
  `cargo test -p rbitcoin-rpc --lib`

### Step 4 — Node wiring + bootstrap flow

- **Contract:** with `--sv2-tp-listen` set, the node serves the listener; a
  client completing setup and sending `CoinbaseOutputConstraints`
  immediately receives `NewTemplate{future_template: true}` then
  `SetNewPrevHash` with the same `template_id`, the current tip as
  `prev_hash`, and matching nBits/target. While the GBT sync gate says
  not-synced, the session holds the constraints and sends the first
  template when the gate clears.
- **Red:** `cargo test -p rbitcoin-test sv2_tp_bootstrap` — one regtest
  node, full handshake → setup → constraints; assert NewTemplate fields
  against the node tip and mempool, and SetNewPrevHash consistency. Gate
  predicate unit in `rbitcoin-sv2`.
- **Green:** `run.rs` service start behind `--sv2-tp-listen` /
  `--sv2-tp-authority-sec` / `--sv2-tp-cert-validity`; session loop calls
  the builder on first constraints; per-session template map.
- **Refactor:** flag plumbing follows the `esplora_block_template` config
  pattern.
- **Verify:** `cargo test -p rbitcoin-test sv2_tp_bootstrap`,
  `cargo test -p rbitcoin-node sv2_tp_`

### Step 5 — RequestTransactionData

- **Contract:** a live `template_id` →
  `RequestTransactionData.Success{template_id, excess_data: "",
  transaction_list}` with the witness-serialized txs in template order;
  unknown id → `RequestTransactionData.Error{error_code:
  "template-id-not-found"}`.
- **Red:** extend the step-4 journey: request the served template's data,
  assert count/order/bytes against the mempool txs; unknown-id error.
- **Green:** session cache read path.
- **Refactor:** none expected.
- **Verify:** same journey filter.

### Step 6 — Tip-change push + stale grace

- **Contract:** after the node accepts a new tip block, every active
  session receives `NewTemplate{future_template: true}` then
  `SetNewPrevHash` on the new `prev_hash`, `template_id` still increasing.
  During the stale grace the old template still answers
  `RequestTransactionData`; after the grace it answers
  `"stale-template-id"`. Future templates for the old prev hash retire the
  same way.
- **Red:** journey: generate a block via the harness RPC, assert the push
  pair arrives without client polling; stale-id behavior before and after
  the grace (grace is a shipped config field; the harness sets it small).
- **Green:** `ChainHub::subscribe_tips()` consumer; rebuild per session
  with its constraints; retire on the grace timer.
- **Refactor:** one "publish template" path shared by bootstrap, tip, and
  fee triggers.
- **Verify:** journey filter; `cargo test -p rbitcoin-sv2 tip_`

### Step 7 — Fee-delta push

- **Contract:** with the tip unchanged, when `MempoolHub::template_updates`
  advances and a rebuilt template's total fees exceed the last sent by
  `--sv2-tp-fee-delta` sats, and at least `--sv2-tp-template-interval`
  seconds passed since the last push, the session sends
  `NewTemplate{future_template: false}` with **no** `SetNewPrevHash`.
  Below the delta or inside the interval: nothing.
- **Red:** journey: submit higher-fee txs via the harness RPC, assert the
  push; submit a fee-trivial tx, assert silence; assert the interval
  throttle.
- **Green:** watch task on the counter; per-session last-sent fee/instant.
- **Refactor:** share the throttle predicate between the decision and its
  unit.
- **Verify:** journey filter.

### Step 8 — SubmitSolution → accept_block

- **Contract:** a client solving the served template sends
  `SubmitSolution{template_id, version, header_timestamp, header_nonce,
  coinbase_tx}` (full witness coinbase); the node assembles header (prev +
  recomputed merkle + message fields) + coinbase + retained txs and runs
  `ChainHub::accept_block`; the tip advances. Unknown/stale template or
  undecodable coinbase → log and drop. Timestamp-window pre-check per the
  constraints above.
- **Red:** journey: grind a regtest nonce on the served template, submit,
  assert the new tip hash; a garbage-coinbase submission leaves tip and
  session healthy.
- **Green:** assembly + pre-checks in a blocking region; accept via
  ChainHub.
- **Refactor:** solution assembly shares the step-2 merkle fold.
- **Verify:** journey filter.

### Step 9 — Operator surface

- **Contract:** [`OPERATOR.md`](../OPERATOR.md) documents
  `--sv2-tp-listen`, `--sv2-tp-authority-sec`, `--sv2-tp-cert-validity`,
  `--sv2-tp-fee-delta`, `--sv2-tp-template-interval`.
  [`COMPAT.md`](../COMPAT.md) gains the SV2 TDP row (the "no stratum" row
  stays; that row is v1 stratum/pool). First-class
  `services.rbitcoin.sv2.tp.*` options in
  [`nix/modules/rbitcoin.nix`](../nix/modules/rbitcoin.nix) with argv
  asserts in `nixos-module-eval.nix`.
- **Red:** eval assert for the flags; docs need no test.
- **Green:** options + docs.
- **Refactor:** `extraArgs` still appends last.
- **Verify:** `nix build .#checks.x86_64-linux.nixos-module-eval --no-link`

## Test budget

Units in `rbitcoin-consensus` (merkle path) and `rbitcoin-sv2` (builder,
throttle, gate). **One** regtest integration journey in `rbitcoin-test`,
opened in step 4 and extended per step — one node open, per
[`TESTING.md`](../TESTING.md) budgets. No live pool/JDC, no mainnet
datadir, no plaintext mode.

## Risks / follow-ups

- stratum-core publishes the wire crates on crates.io while sv2-apps
  git-pins the meta-crate; crates.io versions may lag sv2-tp behavior.
  Record the pinned set in step 1. Interop against the sv2-apps
  integration-tests (their JDC against this TP) is a host follow-up, not
  default CI.
- `cargo deny` failures on new transitive deps (a second secp256k1, AEAD
  crates) — step 0 decides; fallback is a hand-rolled TDP codec (name the
  dep that forced it).
- A second `rust-bitcoin` in the tree costs compile time, not correctness;
  keep it crate-local. If versions align, dedupe.
- Authority-cert rotation: certs are short-lived
  (`--sv2-tp-cert-validity`); rotation is restart-with-new-cert in
  OPERATOR. Hot rotation is a follow-up.
- After ship: JD-server mode, weak blocks, extension negotiation, per-IP
  connection limits → quality.md rows, not this plan.
