# SV2 template provider

Plans A–C have landed. [`quality.md`](./quality.md) keeps Q-64 off the open list. Plan D is in progress.

| Plan | Outcome | Flags |
|------|---------|-------|
| A | `getblocktemplate` and `generate` call `MempoolHub::select_block_template`. | none |
| B | A Job Declarator Client mines a block through this node's template provider. | `--sv2-tp-listen`, `--sv2-tp-authority-sec`, `--sv2-tp-authority-sec-file`, `--sv2-tp-cert-validity`, `--sv2-tp-stale-grace` |
| C | Templates refresh on a fee gain with the tip unchanged. | `--sv2-tp-fee-delta`, `--sv2-tp-template-interval` |
| D | A Job Declarator Server validates custom jobs and submits their solutions through the template provider ([`sv2-job-validation.md`](./sv2-job-validation.md)). | none (per-session `SetupConnection` flag) |

Session caps, deadlines, retained templates, and the testnet minimum-difficulty bits are [`operator/interfaces.md`](./operator/interfaces.md#stratum-v2-template-provider). The wire row is [`COMPAT.md`](../COMPAT.md). The crate is [`CRATES.md`](./CRATES.md).

`noise_sv2` 2.1.0 links `secp256k1` 0.28.2. The workspace links `secp256k1` 0.29.1. The two builds use distinct symbol prefixes.

## Plan D — Custom job validation (TDP `ValidateCustomJob`)

**Goal:** a Job Declarator Server in Full-Template mode checks a JDC's
`DeclareMiningJob` against this node over the TDP connection it already
holds, fetches the transactions the node lacks, and submits the found block
by `template_id`. Wire contract: [`sv2-job-validation.md`](./sv2-job-validation.md)
(proposed TDP messages 0x77–0x7a and `SetupConnection` flag bit 0, for
sv2-spec#217). Requires Plan B. Ships no flag: a session opts in with
`REQUIRES_JOB_VALIDATION`.

### D1 — Block proposal check on `ChainHub`

- **Contract:** `ChainHub::check_block_proposal(&Block) -> Result<u64,
  String>` is Core `TestBlockValidity` for a GBT proposal or an SV2 job:
  prev must be the tip (`inconclusive-not-best-prevblk`), `bad-diffbits`,
  `time-too-old` / `time-too-new`, every spend against the block and the
  confirmed chain, then structure (merkle, weight, sigops, BIP34 height,
  witness commitment) and the coinbase priced at subsidy + fees
  (`bad-cb-amount`, after CheckBlock as in Core's ConnectBlock). `Ok` is
  the fee total. No PoW, no scripts, no UTXO write.
- **Red:** `cargo test -p rbitcoin-net --lib check_block_proposal` — tip
  child passes, other parent rejects, `bad-cb-amount` after structure,
  `Ok(fees)`; `rbitcoin-rpc` `methods_tests` pins GBT proposal mode
  answering `bad-cb-amount`.
- **Green:** the check moves from the RPC crate onto the hub (a move:
  `check_block_proposal_with` keeps the explicit-inputs form for callers
  with a `Query` but no hub); the spend loop then returns Σ(inputs −
  outputs).
- **Verify:** both filters.

### D2 — Messages and the setup flag

- **Contract:** `messages.rs` carries `ValidateCustomJob`,
  `.MissingTransactions`, `.Success`, `.Error` (0x77–0x7a) as binary_sv2
  structs, and `REQUIRES_JOB_VALIDATION`. Setup accepts bit 0 and echoes
  it; any other set bit is still `unsupported-feature-flags`.
- **Red:** `job_validation_messages_round_trip`;
  `setup_connection_success_errors_and_session_cap` gains the flag case.
- **Green:** the structs and the mask in `setup_error`.

### D3 — Client frame cap per message type

- **Contract:** the client frame cap in
  [`operator/interfaces.md`](./operator/interfaces.md#stratum-v2-template-provider), per
  type. `ValidateCustomJob` may carry a block's worth of transactions;
  every other client type still closes past 65557 bytes.
- **Red:** `oversized_client_frame_closes_the_session` — a 200 KiB
  `ValidateCustomJob` keeps the session, a 200 KiB `SubmitSolution` and a
  `ValidateCustomJob` over its cap close it.
- **Green:** `FrameCap { max_frame, payload: fn(u8) -> usize }`: read
  against the largest client message, apply the per-type cap once whole.
- **RAM trade:** one in-flight client frame of ≤ ~6.4 MB per session,
  ≤ ~51 MB at `MAX_SESSIONS`.

### D4 — ValidateCustomJob happy path

- **Contract:** on a session that negotiated the flag, a
  `ValidateCustomJob` whose `prev_hash` is the tip, whose `wtxid_list` is
  all in the mempool, and whose placeholder coinbase pays ≤ subsidy + fees
  is answered `Success{request_id, template_id, fees}`: `template_id` is
  the next id in the session's counter, `fees` the sum over the declared
  txs. The job is retained exactly like a template, so
  `RequestTransactionData` returns the declared txs in block order and
  `SubmitSolution` finds it. Without the flag the message is ignored (the
  D3 journey already pins that).
- **Red:** `validate_custom_job_prices_and_retains_the_declared_job` —
  two legacy spends with known fees in the mempool, a JDS-shaped coinbase
  (BIP34 push + 8 placeholder bytes, payout = subsidy + fees, witness
  commitment with a zero reserved value); `Success` with `fees` = 5 000
  and `template_id` = last `NewTemplate` id + 1; the retained txs read
  back. Red was a reply timeout (the message fell through to "ignoring").
- **Green:** `job::validate` under `spawn_blocking` + `BlockingRegion`:
  decode, resolve each wtxid with `MempoolHub::get_tx_by_wtxid`, header
  `{version, tip, merkle root over the coinbase txid + declared txids,
  time = max(now, MTP + 1), expected nBits, nonce 0}`,
  `check_block_proposal`, `Ok(fees)` → retained under `last_id + 1`,
  `Success`; a reject string → `Error{error_code}`. `Session` gains
  `job_validation` from the setup flags (its first read is the dispatch
  arm).
- **Refactor:** the tip read (height, prev hash, time, bits) moves out of
  `template::build` into `template::next_header` (a move; both paths call
  it). `Template` splits into the `NewTemplate` coinbase split over a
  `Job` — what a session retains under a template id (`merkle_path`,
  `prev_hash`, `header_timestamp`, `n_bits`, `target`, `txs`) — so a
  validated job carries no dead `NewTemplate` fields. The test client
  gains a typed `send`.
- **CPU trade:** one full proposal check per request on the blocking pool
  (every spend against the chain, structure, weight, sigops, coinbase
  value; no scripts, no PoW). A JDS sends one per declaration; a flood
  costs blocking threads, not the reactor, and D3 bounds its RAM.
- **Verify:** `cargo test -p rbitcoin-sv2 --lib`.

### D5a — MissingTransactions round trip

- **Contract:** a wtxid neither the mempool nor `transaction_list`
  resolves → `MissingTransactions{request_id, unknown_tx_position_list}`
  (0-indexed positions in `wtxid_list`); the same job resent with those
  txs in `transaction_list` validates and the supplied txs are retained
  with it. The TP keeps no state across the round trip.
- **Red:** `validate_custom_job_asks_for_and_accepts_missing_transactions`
  — one of two spends not in the mempool → `[1]`; resend with it
  supplied → `Success` with the full fee sum and both txs retained. Red
  was the session closing (D4's placeholder treated an unknown wtxid as
  an error).
- **Green:** resolution consults `transaction_list` first, keyed by
  sha256d of the bytes as sent (the wtxid of that serialization, so no
  decode before the slot is known), then the mempool; unknown positions
  are collected and answered.
- **Verify:** same filter.

### D5b — Untrusted-input rejections

- **Contract:** `Error.error_code` in this order, before any mempool
  lookup or transaction decode: `job-validation-unavailable` while
  `in_ibd()` (the template gate), `stale-prevhash`, `duplicate-wtxid`,
  `bad-missing-tx` (a supplied tx whose hash is not in `wtxid_list`, or a
  declared blob that does not decode), `bad-cb-decode` (this TP's code;
  the draft has none for an undecodable coinbase), then the proposal
  check's reject string (`bad-cb-amount` for an overpaying coinbase).
  Nothing is retained and no id is taken on an error.
- **Red:** `validate_custom_job_rejects_untrusted_input_in_order` — seven
  sends on one session, then `RequestTransactionData(last + 1)` answers
  `template-id-not-found`. Red was `Success` for the first send.
- **Green:** the checks in `job::check`, ahead of resolution.
- **Verify:** same filter.

### D6 — SubmitSolution against a validated job

- **Contract:** after `Success`, a `SubmitSolution{template_id, version,
  ntime, nonce, coinbase_tx}` whose coinbase has the extranonce where the
  placeholder stood and whose header meets the target is assembled from
  the retained job, accepted through `ChainHub::accept_block`, and
  becomes the tip, exactly like a solution for a pushed template.
- **Red:** `submit_solution_for_a_validated_job_becomes_the_tip`. It
  passed with no production change (a validated job is retained as the
  same `Job` a built template is) and is kept as the pin; `first_template`
  takes the setup flags and connects through `connect_tp`.
- **Verify:** `cargo test -p rbitcoin-sv2 --lib submit_solution`.

### D7 — Docs and tidy

- `messages` is crate-private (no other crate reads it yet; the
  `rbitcoin-test` journey that will needs the re-export then). The draft
  is copied unchanged as [`sv2-job-validation.md`](./sv2-job-validation.md)
  and owns the wire contract until it lands upstream. Operator lines in
  `docs/operator/interfaces.md`, the COMPAT row, and a `changelog.d`
  fragment.

### Test budget

Units: the messages round trip, `rbitcoin-net` `check_block_proposal_*`,
`rbitcoin-rpc` GBT proposal `bad-cb-amount`. Journeys in `rbitcoin-sv2`
`template_tests`: four, each one TP and one session on the shared regtest
pad. No `rbitcoin-test` node journey yet (follow-up with a JDS client).

### Risks / follow-ups

- No script execution in the proposal check: mempool txs were
  script-checked at accept; `transaction_list` txs are not. A JDC can
  declare a job with an invalid-script tx that validates here and fails
  at `accept_block`. Core's `TestBlockValidity` runs scripts; running
  `confirm_scripts_phase` on the supplied txs only is the follow-up.
- No operator opt-out flag: any client that can reach the port may set
  the flag and cost one proposal check per request. Bind to loopback or
  firewall the port (the same advice as for templates).
- `SetupConnection.Success.flags` echoes the request flags verbatim.
- Retention shares the `MAX_RETAINED` (3) slots with templates: on one
  tip a JDS session that also receives constraints rebuilds (or Plan C
  pushes) can see a validated job evicted before its solution arrives.
  The draft asks for retention until a newer `Success` or close; a slot
  per kind, or a larger cap for flagged sessions, is a follow-up.
- A job is retained with the nBits its validation header used. On
  min-difficulty networks that can differ from the sent
  `SetNewPrevHash.n_bits` (the Plan B risk); mainnet and signet bits
  depend only on the prev hash.
- `transaction_list` matching hashes the bytes as sent: a legacy tx
  relayed in segwit-marker form hashes differently from its wtxid and is
  `bad-missing-tx`. The draft requires the bytes exactly as JDC sent
  them, which is the serialization the wtxid was computed over.
- The IBD gate (`job-validation-unavailable`) and `bad-cb-decode` are
  this TP's choices within the draft's "recommended" list and should
  follow the upstream text once it settles.
- No policy is applied (the draft forbids it); `fees` lets the pool
  price the declared coinbase.
- The JDS role itself, and a `rbitcoin-test` journey driving a JDS
  client, stay out.

---
