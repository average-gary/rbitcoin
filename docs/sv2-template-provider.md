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
  it; any other set bit is `unsupported-feature-flags`, and `Error.flags`
  names those bits only (`0b101` is answered `0b100`; D19).
- **Red:** `job_validation_messages_round_trip`;
  `setup_connection_success_errors_and_session_cap` gains the flag case.
- **Green:** the structs and the mask in `setup_error`.

### D3 — Client frame cap per message type

- **Contract:** the [Constraints](#constraints-all-plans) frame cap, per
  type (D3 first capped `ProposeTemplate` at a block's worth of
  transactions; D12 moved that cap to the provide and D18 made the cap
  follow negotiation).
- **Red:** `oversized_client_frame_closes_the_session` — a 200 KiB
  `ProposeTemplate` keeps the session, a 200 KiB `SubmitSolution` and a
  `ProposeTemplate` over its cap close it.
- **Green:** `FrameCap { max_frame, payload: fn(u8) -> usize }`: read
  against the largest client message, apply the per-type cap once whole.
- **RAM trade:** see [Constraints](#constraints-all-plans).

### D4 — ProposeTemplate happy path

- **Contract:** on a session that negotiated the flag, a
  `ProposeTemplate` whose `prev_hash` is the tip (field dropped in D9),
  whose `wtxid_list` is all in the mempool, and whose placeholder coinbase
  pays ≤ subsidy + fees
  is answered `Success{request_id, template_id, fees}` (D9 adds
  `prev_hash`): `template_id` is the next id in the session's counter. The
  job is retained exactly like a template, so
  `RequestTransactionData` returns the declared txs in block order and
  `SubmitSolution` finds it. Without the flag the message is ignored
  (`unflagged_session_reads_frames_up_to_the_pre_feature_cap`, D18, pins
  that the session stays open for one under the old cap).
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

- **Contract:** `Error.error_code`, in the order the code runs (D5b first
  put the supplied-transaction checks ahead of resolution; D11, D12, D13
  and D14 moved them). On arrival, with no chain read and no mempool
  lookup: `duplicate-request-id`, `job-validation-unavailable` past the D13
  bound, and `duplicate-wtxid`. Then, on the blocking thread:
  `job-validation-unavailable` while `in_ibd()` (the template gate) or when
  the tip cannot be read (D15), `bad-cb-decode` (this TP's code; the draft
  has none for an undecodable coinbase), while resolving `bad-blk-weight`
  past the block limit and `bad-missing-tx` for a supplied blob that does not
  decode, then the proposal check's reject string (`bad-cb-amount` for an
  overpaying coinbase). `stale-prevhash` was dropped in D9. Only
  `duplicate-wtxid` and the admission refusals are decided before a node
  read; the IBD gate, the tip read and `bad-cb-decode` are not. Nothing is
  retained and no id is held on an error. A provide's `bad-missing-tx` for a
  short or unasked-for transaction is D12.
- **Red:** `propose_template_rejects_untrusted_input_in_order` — seven
  sends on one session, then `RequestTransactionData(last + 1)` answers
  `template-id-not-found`. Red was `Success` for the first send.
- **Green:** the arrival checks in `on_frame` (`Proposal::precheck`) and the
  rest in `job::validate`.
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

- `messages` is `pub` and `#[doc(hidden)]` (the socket journeys and the
  listener tests build the wire messages from it). The draft
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
  `stale-prevhash` is gone. A declaration from before the tip moved usually
  fails the proposal check on its own (`bad-cb-height` for its BIP34 push, or
  a spent input), but a same-height reorg keeps the height valid and the
  inputs may still be unspent, so such a declaration is answered `Success`;
  the JDS compares `Success.prev_hash` with `PushSolution.prev_hash`.
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
  `B064K` fields (~2.3 MB).
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
  then). How many validations run at once is the D14 node-wide bound;
  accepted proposals wait in arrival order up to the D13 bound. A session
  that ends discards its verdicts, but a validation already on a blocking
  thread runs to completion and keeps its permit until then (D14). Before D11 `on_frame` awaited the
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
  (`duplicate-wtxid`; no chain read) on arrival, replying
  at once on failure; otherwise the payload joins a FIFO and
  `start_validations` fills a `JoinSet` of `spawn_blocking` +
  `BlockingRegion` tasks up to the bound. A new `select!` arm takes each
  finished verdict and calls the one `reply_propose` path (retain on
  `Valid`, then the reply). `job::validate` stays complete on its own
  (the precheck runs again inside it).
- **CPU and RAM trade:** full proposal checks on blocking threads, each with
  up to a block of decoded txs, bounded node-wide by D14; waiting proposals
  hold their wtxids and coinbase split, bounded by D13.
- **Verify:** `cargo test -p rbitcoin-sv2 --lib`; the journey 10 times in
  a loop.
- **Superseded in part:** D11 first ran four validations per session
  (`MAX_INFLIGHT_VALIDATIONS`); D14 replaced that count with one
  listener-wide bound.

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
  `Sv2TpConfig` field like `setup_timeout`) checked when a provide is
  consumed (D15); the retire arm only frees an expired hold early. A
  provide for an id the session does not hold, already consumed, or
  expired is `ProposeTemplate.Error unknown-request-id`; a proposal under
  an id still queued, in flight, or held is `duplicate-request-id`; a
  provide that does not cover every requested position, supplies a tx the
  TP did not ask for, or one that does not decode, is `bad-missing-tx` and
  ends that exchange (the first two before any decode or owned copy).
  These edge rules match sv2-tp #137. A validation still short of a tx
  asks again for every position the mempool lacks, the supplied ones
  included, since nothing is held
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
  into the retire timer, `Session.ids` for the ids queued, in flight, or held (D13);
  `Work { proposal, supplied }` through the FIFO and the `JoinSet`; the two
  payload caps in `transport.rs`.
- **RAM trade:** ≤ 8 × ~2.2 MiB held per session, no transactions; client
  frame buffering as in [Constraints](#constraints-all-plans).
- **Verify:** `cargo test -p rbitcoin-sv2 --lib`.

### D13 — Bounded admission

- **Contract:** per session, the work the TP has accepted and not yet
  answered, queued and running together, is bounded by a count
  (`MAX_ADMITTED_PROPOSALS`, 16) and by bytes (`MAX_ADMITTED_BYTES`,
  16 MiB). The bytes are 32 per wtxid, the coinbase prefix and suffix, and,
  for a `ProvideMissingTransactions.Success`, the raw transactions it
  supplies. Both entries are charged: a `ProposeTemplate` and a provide for
  a held proposal. A frame that would pass either bound is answered
  `ProposeTemplate.Error job-validation-unavailable` (the code the IBD gate
  uses; sv2-tp#137 answers the same at its bound). The session stays up and
  keeps serving `RequestTransactionData`, `SubmitSolution`, and later
  proposals once work drains. A provide past the bound consumes its hold: the
  `request_id` is free again and a second provide for it is
  `unknown-request-id`. The charge returns when the proposal is answered,
  whether `Success`, `Error`, or `ProvideMissingTransactions` (which moves it
  to the ≤ 8 held, a separate bound). Request ids in use (queued, running,
  or held) are a hash set, so a duplicate costs one lookup however deep the
  queue is.
- **Red:** `propose_template_admission_is_bounded_by_count` and
  `propose_template_admission_is_bounded_by_bytes` on a runtime with four
  blocking threads. A test thread holds the mempool write lock so the first
  validations block on a pooled wtxid; the session takes its first template
  before the lock and checks fees only a day later, so nothing else needs the
  pool. Proposals under fresh ids are sent to the bound, and the next one,
  and a provide for a held proposal, are refused at once while a
  `RequestTransactionData` on the same socket is answered. After the lock is
  released every accepted proposal is answered and fresh ids, including the
  refused provide's, succeed. Red was a reply timeout: the refused frames
  were queued.
- **Green:** `Session.admitted` (count and bytes) checked on both entries
  before the precheck or any hashing; `Work.cost` carried through the
  `JoinSet` so every completion path returns its charge; `Session.ids`
  replaces the `Vec` scan.
- **RAM trade:** 16 and 16 MiB per session, so ≤ 128 MiB at `MAX_SESSIONS`,
  on top of the ≤ 8 held proposals. A real JDS sends one job per JDC per tip
  change, each a few thousand wtxids (~100 KiB), so 16 covers a pool with
  16 JDCs declaring at once and the bytes bind only for oversized jobs. One
  worst-case proposal is ~2.2 MiB and one worst-case provide ~6 MiB, so the
  bytes admit two of the latter. A JDS past the bound sees
  `job-validation-unavailable` and declares again, the recovery it already
  runs at the IBD gate.
- **Verify:** `cargo test -p rbitcoin-sv2 --test suite propose_template_admission`.

### D14 — Node-wide validation bound

- **Contract:** one `tokio::sync::Semaphore` per listener, sized by
  `Sv2TpConfig::validation_permits` (internal, not an operator flag; the
  listener refuses 0 and anything over `Semaphore::MAX_PERMITS`). Each
  admitted proposal becomes a task in its session's `JoinSet` that awaits
  `acquire_owned` and then moves the `OwnedSemaphorePermit` into a
  `spawn_blocking` closure, so the permit is released only when the blocking
  work returns, even if the session and its `JoinSet` are gone: dropping the
  `JoinSet` aborts tasks still waiting (no permit taken) and discards
  verdicts, but cannot stop a started blocking closure, so reconnecting does
  not buy more concurrent validations. The task, not the session loop, takes
  the grant, so a session whose loop is parked in a slow send or a build
  does not hold a permit idle: waiters run in the semaphore's arrival order
  across sessions. The tasks per session are bounded by the D13 admission
  bound (16, and 16 MiB), and each holds no more than the admission already
  counted. The node sets the count to
  `max(1, blocking_pool / 4)` from the function that sizes its blocking pool
  (`cli::proposal_validation_permits` over `blocking_pool_size`), so
  proposals use at most a quarter of the pool and a four-thread node runs one
  at a time. This reserves blocking threads from proposals; it does not stop
  other users of the pool (template builds, `SubmitSolution`, Electrum, RPC)
  from filling it. `Sv2TpStats::validations_running` counts closures between
  permit and return; it is exported as `/metrics`
  `rbitcoin_sv2_validations_running` and `tip: perf` JSON `sv2_validating`.
- **Red:** `propose_template_validations_are_bounded_across_sessions` on a
  runtime with `max_blocking_threads(4)` and one permit (what the node
  computes for four). With the mempool write lock held, one session's
  proposal runs and two other sessions' wait: the gauge is 1 (Red was 3, the
  per-session path) and a `spawn_blocking` on the same runtime completes.
  The running validation's client drops, a new session proposes, and the
  gauge stays 1. On release the three live sessions each get a verdict and
  the gauge is 0. `proposal_validations_use_a_quarter_of_the_blocking_pool`
  pins the node's count; `out_of_range_timing_refuses_to_start` gains the
  permit bounds; the `sv2_tp_bootstrap` journey reads the metric.
  `propose_template_permit_does_not_wait_for_a_parked_session` (one permit,
  four blocking threads) holds the permit with a running validation, queues a
  proposal from session X, parks X's loop in a template build (the held
  template mutex stands in for a send to a slow reader), and queues a
  proposal from session Y. When the running validation ends, Y's verdict
  must arrive while X is still parked. With the permit granted to X's
  acquire polled only by X's loop, Y waited behind X's undelivered grant and
  timed out; X's verdict and templates arrive once its loop is free.
- **Green:** `Session.permits` replaces the per-session in-flight count;
  `start_validation` spawns the acquire-then-`spawn_blocking` task at
  admission, which moves the permit, then an `enter_validation` guard
  (increment, decrement on drop), into the closure. The per-session
  queue, pending acquire, and `want_permit` are gone: one path.
- **CPU and thread trade:** a quarter of the blocking pool for proposals
  (16 on a 64-core host, 1 on a 4-thread one). A JDS declaring for many JDCs
  at a tip change queues behind that bound (D13) and sees verdicts one
  validation at a time on a small node; validation speed, not safety, is
  what a small node gives up.
- **Verify:** `cargo test -p rbitcoin-sv2 --test suite propose_template_validations`.

### D15 — Hold expiry and retention at the tip boundary

Three review findings on the hold and the retained ring, one rule each.

- **Hold expiry (consume is the authority).** A hold is live while
  `expires_at > now`, asked of one function by both the retire arm and the
  consume path (`Pending::live`). A `ProvideMissingTransactions.Success`
  handled at or after the deadline finds no hold: `unknown-request-id`, the
  id freed, whether or not the retire arm has run. Before, a provide that
  won the select race against the timer started a validation after the
  deadline. The arm stays because it returns the memory and the id before
  anything else touches them.
- **Red:** `provide_after_the_hold_expired_is_unknown_even_when_it_is_queued_first`.
  With a zero `provide_timeout` and the session loop parked in a template
  build on the held template mutex (a proposal validating on the held pool
  write lock, the provide already sent), the verdict and the provide are
  ready in one loop turn. The old consume validated the late provide in
  about a quarter of the rounds (5 of 5 runs failed within five rounds); a test that sleeps past the timeout and lets the timer run
  could not reach it.
- **Unreadable tip (unavailable, not fatal).** `job::validate` answers
  `job-validation-unavailable` when `next_header` fails, as it does in IBD;
  it used to return the `io::Error`, and the join arm's `?` closed the
  session, dropping every retained job and held proposal. A reorg pops the
  tip block by block and signals once the new branch connects (the window
  `check_fees` already tolerates), so one failed read is a retry for the JDS.
  Only a `JoinError` (worker panic) still ends the session. The verdict is
  now infallible, so the blocking closure returns it without an `io::Result`.
- **Red:** `propose_template_tip_read_failure_is_unavailable_and_the_session_survives`.
  No session reaches the reorg window itself; the store's instance-scoped,
  one-shot header-read failure (`Query::fail_next_header_read_cancelled`,
  debug builds, the same seam the `rbitcoin-net` and `rbitcoin-rpc` journeys
  use) is armed right before a proposal, and `next_header`'s header read is
  the first to consume it. Red was the session's disconnect
  (`sv2: tip header: cancelled: header read`). Green: the proposal is
  `job-validation-unavailable`, the earlier job still answers
  `RequestTransactionData`, and the same request id then validates.
- **Retention by the job's parent.** `Templates::retain` used to leave every
  validated job unmarked and `start_grace` marked every retained entry on a
  new prev hash. A job validated on the new tip B before the session handled
  B's event was then retired by B's own grace while B was the tip; a job on
  the old tip A that finished after the session moved to B kept no deadline
  and outlived A's grace. Now `start_grace(new_prev, deadline)` marks only
  entries whose parent is another tip, clears the mark on entries of
  `new_prev` (a return to a tip, A→B→A), and records the tip it left with
  `deadline` in a ring of `MAX_RETIRED_TIPS` (8) pairs. `retain` gives a job
  on a tip with a record that deadline, which may already have passed: the
  job is retired by the next pass, never given a fresh grace. A job on
  `current_prev`, or on a tip with no record (the session's event for it is
  still to come, or the record aged out), is unmarked, and the next
  `start_grace` marks it. A job that outlives the 8-tip record therefore
  waits for one more tip event, then gets that event's grace. A session
  starts on the chain tip (`Templates::seed_tip`, read right after it
  subscribes to tip events), so a validation-only session, which never
  sends a template, records the tip its first event leaves; before, a job on
  that tip finishing after the event kept no deadline until the next tip
  event.
- **Red:** `crates/rbitcoin-sv2/tests/session.rs` drives the shipped
  `Templates` methods, as `tip_event_rebuilds_a_template_built_before_it`
  does: the window between `ChainHub` publishing the store tip and the
  session's tip event (`Templates::on_tip`) is not reachable
  deterministically from a socket, so a small test on the `#[doc(hidden)]`
  methods is the witness (TESTING.md "True journeys"). The B job before B's
  event survives past the deadline; the A job after B's push retires at A's
  original deadline, including a deadline already past; A→B→A keeps A's jobs
  and retires B's; a tip older than the record is classified by the next
  event. Red was the compile failure against the one-argument
  `start_grace` and the five tests failing on the old marking.
  `a_session_that_starts_on_a_tip_retires_a_late_job_of_that_tip_at_its_first_move`
  drives the seeded start the same way: a verdict and the tip event are
  ready in one loop turn and `select!` picks between them at random, so no
  socket orders the late verdict after the event, and a validation that
  straddles the move is rejected by the proposal check on the new tip anyway.
  Red was the compile failure against `seed_tip`.

### D16 — A custom job's solution is rebuilt with the bits its time needs

- **Contract:** a retained job carries how its `SubmitSolution` header gets
  `nBits` (`template::Bits`): a pushed template `Sent(n_bits)`, the ones its
  `SetNewPrevHash` announced for that prev hash (the pin stays, and a
  timestamp that needs other bits is dropped on the answer of the one owner
  below); a validated job
  `Consensus`. A JDC hashes the bits its own TP sent, possibly another TP or
  this one before a min-difficulty gap, and a connection that only
  validates never got a `SetNewPrevHash`, so the validation-time header's
  bits are not what was hashed. For `Consensus` the session asks
  `ChainHub::expected_child_bits(parent, ntime)` in the blocking
  region, builds the header with the answer, and checks proof of work against
  that header before `accept_block`. The owner is `expected_bits_off_tip`
  through `ChainHub::expected_child_bits`, which walks the parent's own
  headers; `rbitcoin_query::expected_next_bits` reads active-chain heights
  and is not used for a parent that is no longer the tip. A pushed template's
  drop check asks the same owner (it fails open when the parent is unknown,
  and `accept_block` then decides); there is no second bits path. An unknown
  parent header drops a custom job's solution like any bad one.
  `Template` now owns the announced `n_bits` and `target`; `Job` keeps
  only `Bits`, so a custom job holds no bits it never uses.
- **Red:** three journeys on the regtest pow-overlay fixture of
  `template_target_follows_the_regtest_pow_overlay`.
  `custom_job_solution_before_the_gap_uses_the_bits_its_time_needs`: the
  node clock moves past prev + 2 × spacing, the job is validated (its header
  carries the pow-limit bits), and the solution is stamped before the gap and
  hashed with the overlay bits the JDC was sent; red was `timestamp needs
  different bits; dropping`. `custom_job_solution_past_the_gap_uses_the_bits_its_time_needs`:
  validated before the gap, stamped past it, hashed at the pow limit; red was
  `misses the template target`. Both become the tip.
  `custom_job_solution_that_misses_the_consensus_target_is_dropped` is a
  guard that passed before (the old path dropped it as well): a nonce that
  meets only the easier target never reaches `accept_block`, under neither
  the retained nor the consensus header.
  `custom_job_solution_on_a_parent_that_is_no_longer_the_tip_becomes_the_tip`
  is a guard too: a block lands on the job's parent, and a heavier sibling
  of it from the job becomes the tip. A parent that a reorg took off the best
  chain cannot be reached end to end: `accept_block` finds a parent only on
  the best chain (`UnknownParent`), so no journey can show the branch-aware
  bits for it.
- **Verify:** `cargo test -p rbitcoin-sv2 --test suite custom_job_solution`.

### D17 — Validation-only sessions need no constraints

- **Contract:** sv2-spec#239 thread 3: a JDS that only validates sets
  `REQUIRES_JOB_VALIDATION`, sends no `CoinbaseOutputConstraints`, and is
  sent no `NewTemplate` or `SetNewPrevHash`. A session that negotiated the
  flag is set up once `SetupConnection.Success` is sent: the setup-deadline
  arm of `Session::run` no longer applies to it (`Session::set_up`), and the
  opt-in `--sv2-tp-idle-timeout` (default off, #1311) starts there instead
  of at the first constraints. Constraints stay optional: sent later, they
  start the template stream as they always did. A session without the flag
  keeps the setup deadline (`session_without_constraints_is_dropped_at_the_setup_deadline`).
  The handshake deadline in `serve` is unchanged. No flag, no message, no
  wire field. Retention follows the tip for it too: `Templates` keeps `tip`
  (where retention last moved, `start_grace`) apart from `current_prev` (the
  prev hash `send` announced), and `Session::run` calls `start_grace` on a
  tip event when the session has no constraints, so its retained jobs, private
  supplied bodies included, retire by their own parent after `stale_grace`
  like a template's. Constraints sent later still get a future template and
  `SetNewPrevHash`, since the event never moved `current_prev`. A configured idle timeout shorter than the 30 s provide hold
  closes a quiet JDS that is waiting for its JDC to answer a
  `ProvideMissingTransactions`; the operator doc says so.
- **Red:** `validation_only_session_validates_and_solves_past_the_setup_timeout`:
  a flagged session with a 300 ms setup timeout sends nothing for three of
  them, then `ProposeTemplate` is answered `Success` under template id 1,
  `RequestTransactionData` returns the job, and the solution becomes the
  tip (red was `early eof` at the setup deadline); constraints sent after
  that start the stream. `validation_only_session_silent_is_dropped_at_the_idle_deadline`:
  a flagged session with the default 10 s setup timeout and a 300 ms idle timeout
  that sends nothing is closed (red: it stayed open, as the idle arm waited
  for constraints).
  `validation_only_session_retires_jobs_on_a_tip_change`: a job validated on
  tip A answers `stale-template-id` after a block moves the tip and
  `stale_grace` passes (red: still retained five seconds later), a job
  validated on the new tip stays, and constraints sent afterwards get
  `NewTemplate(future=true)` and `SetNewPrevHash`.
- **Verify:** `cargo test -p rbitcoin-sv2 --test suite validation_only`.

### D18 — The client frame cap follows negotiation

- **Contract:** the reader's frame cap is the pre-feature
  `MAX_CLIENT_PAYLOAD` until `SetupConnection.Success` carries
  `REQUIRES_JOB_VALIDATION`, and for a session without that flag; only a
  negotiated session reads up to the `ProposeTemplate` and provide caps
  (see [Constraints](#constraints-all-plans) for the numbers and the
  buffering). The first post-Noise read, `SetupConnection`, is under the
  small cap too.
- **Red:** `reader_before_negotiation_refuses_a_frame_over_the_pre_feature_cap`
  (a `recv` on the accept side of a real Noise socket read a
  `ProposeTemplate`-typed frame one byte over the cap);
  `unflagged_session_reads_frames_up_to_the_pre_feature_cap` (a session
  without the flag stayed open for a `ProposeTemplate` over the old cap).
  `frame_over_the_pre_feature_cap_before_setup_closes_the_session` closed
  before this change too, after buffering the frame; it pins the outcome.
  `flagged_session_reads_the_job_validation_pair_up_to_their_caps` was
  green throughout and pins a max-size `ProposeTemplate` and provide read
  and one byte over each closed. `oversized_client_frame_closes_the_session`
  is replaced by these.
- **Green:** `FrameCap::CLIENT` starts at the old cap;
  `NoiseReader::allow_job_validation` raises `max_frame`, called from
  `serve` before `into_split`. No second Noise reader.

### D19 — SetupConnection.Error names only the unsupported bits

- **Contract:** `setup_error` returns `m.flags & !REQUIRES_JOB_VALIDATION`
  as `Error.flags`, the bits the TP cannot serve (sv2-spec#239 thread 3
  reads `Error.flags` as the unsupported features). A flagged request that
  also sets an unknown bit no longer names the supported bit 0.
- **Red:** `setup_connection_success_errors_and_session_cap`: `0b101` is
  answered `unsupported-feature-flags` with `0b100` (red: `0b101`); `0b1` is
  accepted and echoed.
- **Green:** the mask in `setup_error`.

### Test budget

Units: the messages round trip, `job::extranonce_len`, `rbitcoin-net`
`check_block_proposal_*`, `rbitcoin-rpc` GBT proposal `bad-cb-amount`.
Journeys in `rbitcoin-sv2`
`template_tests`: five, each one TP and one session on the shared regtest
pad; the D11 journey is the heaviest (a fan-out block, then three
validations of 400 supplied spends); D13 adds two more and D14 one, each with
the mempool write lock held. No `rbitcoin-test` node journey yet
(follow-up with a JDS client).

### Risks / follow-ups

- No operator opt-out flag: any client that can reach the port may set
  the flag and cost one proposal check per request. Bind to loopback or
  firewall the port (the same advice as for templates).
- The waiting-proposal queue is bounded per session by D13 (16 proposals,
  16 MiB). A JDS that declares for more than 16 JDCs at one tip change sees
  `job-validation-unavailable` on the rest and declares again; raise
  `MAX_ADMITTED_PROPOSALS` if a pool reports it.
- A job whose validation straddles a tip change ends one of two ways.
  Past the proposal check's tip read it is retained under its own parent
  (D15): current when the session has not moved off that tip yet, or retired
  at the deadline recorded when it did. `Success.prev_hash` names the
  validated tip, so the JDS discards a stale one, and a solution on it is a
  side block, not the tip. Before that read the check rejects it
  (`inconclusive-not-best-prevblk`, or `bad-cb-height` when the header was
  read after the move). The D11 journey accepts either.
- `SetupConnection.Success.flags` echoes the request flags, which are only
  ever `0` or `REQUIRES_JOB_VALIDATION` (any other bit is an `Error`).
- Retention is the template rule (D10): a validated job shares the
  session's `MAX_RETAINED` (64) ring with templates and retires with the
  same stale grace after its parent stops being the tip (D15). Past 64
  same-tip pushes and validations the oldest goes; a
  `SubmitSolution` for it is dropped and JDC's own propagation (JDP 6.4.9)
  covers the block.
- A validation-only session (D17) retires its jobs on the tip event: it
  is sent no `SetNewPrevHash`, so its grace starts when the session handles
  the event, where a session with constraints starts it when it sends the
  new prev hash (`Session::announces_prev` picks which). A session with
  constraints that is holding (IBD, or a pool that is not maintained) has
  sent no new prev hash, so its jobs keep their parent's mark until the
  first template goes out; the 64-slot ring (D10) bounds that. A missed
  event (`Lagged`) leaves retention on the old tip until the next one. A
  solution on a job of an old tip is a side block, as before.
- A validated job's solution is hashed by a JDC with the nBits its own TP
  sent, which this session may never have sent (D16). The header is rebuilt
  with the consensus nBits of a child of the job's parent at the submitted
  time; a parent whose header the node does not have drops the solution.
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
