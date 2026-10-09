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

## Plan D — Custom job validation (TDP `ProposeTemplate`)

**Goal:** a Job Declarator Server in Full-Template mode checks a JDC's
`DeclareMiningJob` against this node over the TDP connection it already
holds, fetches the transactions the node lacks, and submits the found block
by `template_id`. Wire contract: [`sv2-job-validation.md`](./sv2-job-validation.md)
(proposed TDP messages 0x77–0x7b and `SetupConnection` flag bit 0, for
sv2-spec discussion #239, which supersedes #217). Requires Plan B and Plan C
(merged as #949 and #951: the shared `Arc<Transaction>` bodies and the
64-slot same-tip ring that retains validated jobs) and the proposal-check
stack on `ChainHub`, merged as #959, #961, #963, #967, #968, and #969. That
check is the job-validation hot path: a JDS asks once per JDC declaration,
dozens of times a minute per pool, so before this plan could ship it had
to become one decode per parent (#961, #968), bounded in memory (#968),
right about spent coins after a reorg (#968, #969), and complete (immature
coinbases and structure before spends in #963, script execution and the
fee-overflow checks in #967). The Core-IPC sibling implementation is
stratum-mining/sv2-tp PR #137. Ships no flag: a session opts in with
`REQUIRES_JOB_VALIDATION`.

### D1 — Block proposal check on `ChainHub`

- **Contract:** `ChainHub::check_block_proposal(&Block) -> Result<u64,
  String>` is Core `TestBlockValidity` for a GBT proposal or an SV2 job:
  prev must be the tip (`inconclusive-not-best-prevblk`), `bad-diffbits`,
  `time-too-old` / `time-too-new`, every spend against the block and the
  confirmed chain, then structure (merkle, weight, sigops, BIP34 height,
  witness commitment) and the coinbase priced at subsidy + fees
  (`bad-cb-amount`, after CheckBlock as in Core's ConnectBlock), then the
  scripts of every spend (#967). `Ok` is the fee total. No PoW, no UTXO
  write. Merged as #959; #963, #967, #968, and #969 extended it (see the
  Plan D intro).
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

- **Contract:** `messages.rs` carries `ProposeTemplate`,
  `.MissingTransactions` (the `ProvideMissingTransactions` pair since D12),
  `.Success`, `.Error` (0x77–0x7a) as binary_sv2
  structs, and `REQUIRES_JOB_VALIDATION`. Setup accepts bit 0 and echoes
  it; any other set bit is still `unsupported-feature-flags`.
- **Red:** `job_validation_messages_round_trip`;
  `setup_connection_success_errors_and_session_cap` gains the flag case.
- **Green:** the structs and the mask in `setup_error`.

### D3 — Client frame cap per message type

- **Contract:** the client frame cap in
  [`operator/interfaces.md`](./operator/interfaces.md#stratum-v2-template-provider), per
  type. `ProposeTemplate` may carry a block's worth of transactions;
  every other client type still closes past 65557 bytes.
- **Red:** `oversized_client_frame_closes_the_session` — a 200 KiB
  `ProposeTemplate` keeps the session, a 200 KiB `SubmitSolution` and a
  `ProposeTemplate` over its cap close it.
- **Green:** `FrameCap { max_frame, payload: fn(u8) -> usize }`: read
  against the largest client message, apply the per-type cap once whole.
- **RAM trade:** one in-flight client frame of ≤ ~6.5 MB per session,
  ≤ ~52 MB at `MAX_SESSIONS`.

### D4 — ProposeTemplate happy path

- **Contract:** on a session that negotiated the flag, a
  `ProposeTemplate` whose `prev_hash` is the tip (field dropped in D9),
  whose `wtxid_list` is all in the mempool, and whose placeholder coinbase
  pays ≤ subsidy + fees
  is answered `Success{request_id, template_id, fees}` (D9 adds
  `prev_hash`): `template_id` is the next id in the session's counter. The
  job is retained exactly like a template, so
  `RequestTransactionData` returns the declared txs in block order and
  `SubmitSolution` finds it. Without the flag the message is ignored (the
  D3 journey already pins that).
- **Red:** `propose_template_prices_and_retains_the_declared_job` —
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
  value, scripts; no PoW). A JDS sends one per declaration; a flood
  costs blocking threads, not the reactor, and D3 bounds its RAM.
- **Verify:** `cargo test -p rbitcoin-sv2 --lib`.

### D5a — MissingTransactions round trip

- **Contract:** a wtxid neither the mempool nor `transaction_list`
  resolves → `MissingTransactions{request_id, unknown_tx_position_list}`
  (0-indexed positions in `wtxid_list`); the same job resent with those
  txs in `transaction_list` validates and the supplied txs are retained
  with it. The TP keeps no state across the round trip (reversed in D12:
  the TP holds the proposal and the txs arrive in their own message).
- **Red:** `propose_template_asks_for_and_accepts_missing_transactions`
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
  lookup or transaction decode: `duplicate-wtxid` and `bad-missing-tx` (a
  supplied tx whose hash is not in `wtxid_list`) on arrival, with no chain
  read (D11 moved them ahead of the gate); then, on the blocking thread,
  `job-validation-unavailable` while `in_ibd()` (the template gate),
  `stale-prevhash` (dropped in D9), `bad-missing-tx` for a declared blob
  that does not decode, `bad-cb-decode` (this TP's code; the draft has none
  for an undecodable coinbase), then the proposal check's reject string
  (`bad-cb-amount` for an overpaying coinbase). Nothing is retained and no
  id is taken on an error.
- **Red:** `propose_template_rejects_untrusted_input_in_order` — seven
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

### D8 — Rename to `ProposeTemplate`

- **Contract:** sv2-spec discussion #239 (plebhash, 2026-10-06) supersedes
  #217 and names the message `ProposeTemplate`; `.MissingTransactions`,
  `.Success`, `.Error` follow. Pure rename: structs, message-type consts,
  the frame cap, the session handler, journey names, log strings, this
  plan, the draft, the operator doc, the COMPAT row, the changelog
  fragment. No behaviour change.
- **Verify:** `cargo test -p rbitcoin-sv2 --lib`; `rg ValidateCustomJob`
  empty.

### D9 — The `DeclareMiningJob` subset; `Success` names the tip

- **Contract:** `ProposeTemplate` carries what a JDS has from
  `DeclareMiningJob` and nothing it would have to invent: `request_id`,
  `version`, `coinbase_tx_prefix`, `coinbase_tx_suffix`, `wtxid_list`,
  `excess_data` (opaque here), plus `transaction_list` (dropped in D12). No `prev_hash`:
  the TP validates on its current tip and `Success` carries that tip as
  `prev_hash` next to `template_id`. The TP builds the
  placeholder coinbase: `E = L − P` from the prefix (version, BIP144
  marker and flag when present, an input count that MUST be 1, the
  prevout, scriptSig length `L`, `P` bytes present; `2 ≤ L ≤ 100`,
  `P ≤ L`), coinbase = prefix ‖ zeros(E) ‖ suffix. A prefix that does not
  parse, or a result that does not decode, is `bad-cb-decode`.
  `stale-prevhash` is gone: a declaration from before the tip moved fails
  the proposal check on its own (`bad-cb-height` for its BIP34 push).
- **Red:** the four journeys send prefix/suffix and `expect_job_success`
  asserts `Success.prev_hash == tip`;
  `propose_template_rejects_untrusted_input_in_order` swaps the stale case
  for an old-height declaration (`bad-cb-height`) and adds a prefix with
  more scriptSig bytes than its length declares, a two-input prefix, and a
  truncated suffix (all `bad-cb-decode`); `job_validation_messages_round_trip`
  carries the new fields. Red was the compile error on the new fields.
- **Green:** `messages.rs` fields; `job::extranonce_len` (unit-tested for
  the legacy and segwit-marker prefix and each rejection) and the
  placeholder assembly ahead of the coinbase decode; the request
  `prev_hash` comparison dropped; `Success.prev_hash` from the retained
  job's header. `MAX_PROPOSE_TEMPLATE_PAYLOAD` re-priced for three
  `B064K` fields (~6.5 MB).
- **Verify:** `cargo test -p rbitcoin-sv2 --lib`.

### D10 — Return `fees` in `Success`; retain jobs like templates

- **Contract:** `ProposeTemplate.Success` is `{request_id, template_id,
  prev_hash, fees}`; `fees` is the fee total of the declared transactions as
  `ChainHub::check_block_proposal` computed it. Only the validating node can
  derive it (sum of inputs minus sum of outputs per transaction needs the
  UTXO set) and it computes it anyway for `bad-cb-amount`; the Pool
  otherwise has only the coinbase's claimed value, which that check makes a
  lower bound. Bitcoin Core's IPC does not expose it today (`checkBlock`
  returns only reason, debug and result; a `TxCollection.makeTemplate`
  template throws on `getTxFees`); the gap is raised on bitcoin/bitcoin#35671
  rather than designed around. A validated job is retained exactly as a
  template is: the same
  `MAX_RETAINED` ring (oldest first) and the same stale grace after a tip
  change. A JDS multiplexes many JDCs over one connection, so "the latest
  validated job" is not a unit worth pinning; the 64-slot same-tip ring from
  C1 is the guarantee, and the draft (§4.3) now says so.
- **Red:** `job_validation_messages_round_trip` and `expect_job_success`
  with `fees`; Red was `E0560` on the struct literal.
- **Green:** the field, `Verdict::Valid { fees, job }` and the reply
  plumbing. `check_block_proposal` returns `Ok(fees)`: D1 cross-crate API
  that GBT proposal mode also reads.
- **Verify:** `cargo test -p rbitcoin-sv2 --lib`.

### D11 — Validation off the session loop

- **Contract:** a `ProposeTemplate` validation never delays the other
  frames or loop branches of its session. `SubmitSolution`,
  `RequestTransactionData`, tip pushes, fee checks, and retire timers run
  while a validation is in flight; `ProposeTemplate.Success` / `.Error` is
  sent when the validation completes, paired by `request_id`, and the job
  is retained when that reply is sent (so it takes the template id current
  then). Up to `MAX_INFLIGHT_VALIDATIONS` (4) validations run at once per
  session; further proposals wait in arrival order and are not refused.
  Session end aborts the in-flight set. Before D11 `on_frame` awaited the
  validation inline inside the session `select!`, so a JDS multiplexing
  many JDCs over one connection could have a found block's
  `SubmitSolution` sit behind another JDC's validation.
- **Red:** `propose_template_validation_does_not_delay_other_frames` —
  400 supplied spends of one confirmed fan-out make the validation the
  costly frame (about 85 ms on the parent-once proposal check) while every
  template stays coinbase-only. The proposal alone measures that wall. A
  `RequestTransactionData` for the empty template is sent right behind a
  second copy; its reply must arrive before that copy's `Success`, in under
  a quarter of the validation it overlapped: the request has no blocking
  work behind it, so a slower answer means the loop waited and the order
  was luck. A `SubmitSolution` behind a third copy is accepted while it
  validates and pushes the solved tip's `NewTemplate`; its accept ends in a
  tip write (an fsync), so the journey does not order it against that
  copy's reply, which is the straddle named under Risks: `Success` on the
  tip the validation started on, or the proposal check's reject once the
  tip moved first. The first shape (1200 inputs, the solution ordered
  before the reply) owed its margin to the per-input parent decode that
  #968 removed; on the merged check it failed 10 of 10 runs (`Success`
  before the solved tip's template, or `inconclusive-not-best-prevblk`
  when the tip moved first), which was Red for this shape. The original
  Red was `Success` as the first frame, the solution 527 ms behind a
  696 ms validation.
- **Green:** `on_frame` decodes the proposal and runs `job::precheck`
  (`duplicate-wtxid`, `bad-missing-tx`; no chain read) on arrival, replying
  at once on failure; otherwise the payload joins a FIFO and
  `start_validations` fills a `JoinSet` of `spawn_blocking` +
  `BlockingRegion` tasks up to the bound. A new `select!` arm takes each
  finished verdict and calls the one `reply_propose` path (retain on
  `Valid`, then the reply). `job::validate` stays complete on its own
  (the precheck runs again inside it).
- **CPU and RAM trade:** four full proposal checks at once per session,
  each on a blocking thread with up to a block of decoded txs; waiting
  proposals hold their payloads (≤ `MAX_PROPOSE_TEMPLATE_PAYLOAD` each),
  depth bounded by the client's burst (one job per JDC per tip change for a
  JDS), not by this TP: see Risks.
- **Verify:** `cargo test -p rbitcoin-sv2 --lib`; the journey 10 times in
  a loop.

### D12 — Missing transactions as a request/provide pair

- **Contract:** sv2-spec#239 (Sjors, 2026-10-08; agreed): the missing leg
  is proper messages reusing JDP 6.4.7 and 6.4.8 under TDP numbers.
  `ProposeTemplate` carries no `transaction_list`; the TP answers
  `ProvideMissingTransactions {request_id, unknown_tx_position_list}` (0x78)
  and the client returns `ProvideMissingTransactions.Success {request_id,
  transaction_list}` (0x7b). A JDS copies both payloads between the TP and
  its JDC, changing only the message type. The TP holds a proposal it asked
  about under its `request_id`: `job::Proposal` (wtxids and the coinbase
  split, no txs), at most `MAX_PENDING_PROPOSALS` (8) per session with the
  oldest dropped, for `provide_timeout` (`PROVIDE_TIMEOUT` 30 s, a
  `Sv2TpConfig` field like `setup_timeout`) checked in the retire arm. A
  provide for an id the session does not hold, already consumed, or
  expired is `ProposeTemplate.Error unknown-request-id`; a proposal under
  an id still queued, in flight, or held is `duplicate-request-id`; a
  provide that does not cover every requested position, supplies a tx the
  TP did not ask for, or one that does not decode, is `bad-missing-tx` and
  ends that exchange (the first two before any decode). These edge rules
  match sv2-tp #137. A validation still short of a tx asks again for every position
  the mempool lacks, the supplied ones included, since nothing is held
  across the round trip but the proposal. The block-sized client frame cap
  moves to `ProvideMissingTransactions.Success` (~4.2 MB);
  `ProposeTemplate` is wtxids plus coinbase (~2.3 MB).
- **Red:** `propose_template_asks_for_and_accepts_missing_transactions`
  rewritten around the round trip: `unknown-request-id` before, after, and
  past a 1 s `provide_timeout`; `duplicate-request-id` while held;
  `bad-missing-tx` for a tx not asked for, a provide short of a requested
  position (answered `ProvideMissingTransactions` again before), and a
  blob under the declared wtxid; nine holds dropping the first; only the
  completed jobs take ids. The D11 journey completes each copy through its
  provide and refuses a `ProposeTemplate` under the id in flight
  (`duplicate-request-id`; before, it was validated a second time);
  `oversized_client_frame_closes_the_session` gains a provide past the
  `ProposeTemplate` cap that keeps the session. Red was the compile
  failure against the one-message shape (no table, no 0x7b handler; the
  old session logged `ignoring message 0x7b`).
- **Green:** `job::Proposal` (owned decode, `precheck`, `accept_supplied`
  with the coverage count) and `job::validate(&Proposal, &Supplied)`;
  `Session.pending` as a `VecDeque<Pending>` with `hold` and expiry folded
  into the retire timer, `Session.open` for the ids queued or in flight;
  `Work { proposal, supplied }` through the FIFO and the `JoinSet`; the two
  payload caps in `transport.rs`.
- **RAM trade:** ≤ 8 × ~2.2 MiB held per session, no transactions; one
  in-flight client frame ≤ ~4.2 MB, ≤ ~34 MB at `MAX_SESSIONS`.
- **Verify:** `cargo test -p rbitcoin-sv2 --lib`.

### Test budget

Units: the messages round trip, `job::extranonce_len`, `rbitcoin-net`
`check_block_proposal_*`, `rbitcoin-rpc` GBT proposal `bad-cb-amount`.
Journeys in `rbitcoin-sv2`
`template_tests`: five, each one TP and one session on the shared regtest
pad; the D11 journey is the heaviest (a fan-out block, then three
validations of 400 supplied spends). No `rbitcoin-test` node journey yet
(follow-up with a JDS client).

### Risks / follow-ups

- No operator opt-out flag: any client that can reach the port may set
  the flag and cost one proposal check per request. Bind to loopback or
  firewall the port (the same advice as for templates).
- The waiting-proposal queue (D11) has no depth cap: a client that sends
  well-formed, expensive proposals faster than four blocking threads
  validate them grows it by one payload each (≤ ~6.5 MB). A refusal or a
  flood close would cut off a JDS at a tip change, when every JDC declares
  at once; the trust model above (who reaches the port) is the bound. A
  byte cap with an `Error` past it is the follow-up if that model changes.
- A job whose validation straddles a tip change ends one of two ways.
  Past the proposal check's tip read it is retained after the push for the
  new tip, so it misses that tip's stale grace and stays in the ring until
  pushed out; `Success.prev_hash` names the old tip, so the JDS discards
  it, and a solution on it is a side block, not the tip. Before that read
  the check rejects it (`inconclusive-not-best-prevblk`, or
  `bad-cb-height` when the header was read after the move). The D11
  journey accepts either.
- `SetupConnection.Success.flags` echoes the request flags verbatim.
- Retention is the template rule (D10): a validated job shares the
  session's `MAX_RETAINED` (64) ring with templates and the same stale
  grace. Past 64 same-tip pushes and validations the oldest goes; a
  `SubmitSolution` for it is dropped and JDC's own propagation (JDP 6.4.9)
  covers the block.
- A job is retained with the nBits its validation header used. On
  min-difficulty networks that can differ from the sent
  `SetNewPrevHash.n_bits` (the Plan B risk); mainnet and signet bits
  depend only on the prev hash.
- `ProvideMissingTransactions.Success` matching hashes the bytes as sent:
  a legacy tx relayed in segwit-marker form hashes differently from its
  wtxid and is `bad-missing-tx`. The draft requires the bytes exactly as
  JDC sent them, which is the serialization the wtxid was computed over.
- A held proposal dropped by the bound (D12) or the timeout surfaces only
  as `unknown-request-id` on its provide; the JDS proposes again. A JDS
  with more than eight JDCs short of transactions at one tip change loses
  the oldest round trips to that bound; raise `MAX_PENDING_PROPOSALS` if a
  pool reports it (each hold is wtxids and a coinbase, not a block).
- The message numbers 0x78 and 0x7b for the provide pair are this draft's
  proposal; #239 has assigned none yet.
- The IBD gate (`job-validation-unavailable`) and `bad-cb-decode` are
  this TP's choices within the draft's "recommended" list and should
  follow the upstream text once it settles.
- The extranonce-tail assumption (the suffix starts at `nSequence`) now
  lives in this TP's `extranonce_len`, not in the JDS. A JDC whose
  extranonce is not the scriptSig tail gets `bad-cb-decode` or a coinbase
  check failure here; the draft asks the Job Declaration Protocol to state
  the split (§6).
- No policy is applied (the draft forbids it); the Pool prices the
  declared coinbase from the coinbase itself.
- The JDS role itself, and a `rbitcoin-test` journey driving a JDS
  client, stay out.
