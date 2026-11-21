# Venue port assessment: upstream `HerodotusDev/Diorama` vs our `port/herodotus-venue-parity`

Measured 2026-11-21 against `herodotus/develop` (`f362017`, 512 commits) — an
**independent lineage** of the same codebase (unrelated git histories; a merge
is impossible, this is a file-level port). The two trees are far closer than the
commit counts suggest.

## Correction of the first pass (measured, feature by feature)

Already present in **both** trees — do **not** port:

- L1 read precompile mirror (`bridge/precompiles.rs`, `system.rs`,
  `PrecompileMock.sol`): both serve `0x0801 spotBalance`, `0x0803 withdrawable`,
  `0x080f accountMarginSummary`, `0x0810 coreUserExists` from the sim.
- `destinationDex` on EVM→Core deposits, the activation fee on an account's
  first outbound transfer, API `spotSend` parity, base-fee handling,
  the spot **hold** lane (resting orders), the COW book composition, the
  control plane, `walk_served_book`, the sim's `touch`/mark-dirty discipline.

What we have that upstream lacks (keep):

- `spot_markets` config gate + the readiness contract built on it.
- `print_sync.rs` (the on-chain print follow; upstream deleted it — the
  deployment still uses it for the xyz:* oracle follow).
- Our k8s/deployment facts (Funnel door, NodePort 31090, freshness runbook).

## The genuine gap (port these, in order)

1. **`feed/l2.rs`** (223 lines, standalone): rebuild books from the venue's
   fast `l2` channel — one snapshot then ~0.5 s diffs, deflate-compressed
   (`{"c": base64}`) or plain (`{"u": diff}`); apply = remove-then-upsert by
   price, keep 20; **reproduced 42/42 real `l2Book` snapshots**. Our books
   come from the throttled `l2Book` channel (20 levels every 5–12 s) — the
   root cause of the mirror's "books stale half the time".
2. **`feed/relay.rs`** (397 lines, standalone): the demand-driven subscription
   relay. Context streams are permanent for every live coin of every dex
   (~1 push/coin/s, ~130 KiB/s measured); **books are demand-driven** with a
   lease (downstream WS subscription) / touch (order placed, held position,
   resting order, `l2Book` read) + 30 s grace; budgets under the venue's
   1000-sub / 2000-msg-per-minute limits (`MAX_STREAMS` 850, `MAX_BOOKS` 300,
   `SendBudget` 1200/min, 5 min refusal cooldown). Deps (`base64`, `flate2`)
   are already in our Cargo.toml.
3. **`feed/ws.rs` sync loop**: once a second, diff `feed.desired_streams(now)`
   against the socket's live set (`relay::plan`), send within the budget,
   resubscribe everything on reconnect (a socket starts empty), handle `l2`
   frames through `l2.rs`, and `confirm_books` while delivering (a quiet book
   on a live stream is current — push-on-change, not staleness).
4. **`api/ws.rs` (downstream)**: take a `lease_coin` per market-data
   subscription (dropped on unsubscribe/disconnect).
5. **`rest.rs`**: the poller backfills books for `wanted_coins()` only, not
   the configured market list; a coin with no stream and no cache is served by
   `ensure_book` (one on-demand REST read, budgeted).
6. **`service.rs` tick**: `feed.want_books(sim.held_coins())` each tick so a
   held coin's book never idles out.
7. **Push-on-change for the downstream WS** (25 ms coalescing,
   per-subscription dedup; `market_push_interval_ms` deprecated) — last, and
   independently of 1–6.

## Integration map (call sites, measured on both trees)

- `feed/mod.rs`: add `relay: Arc<relay::Relay>` (both constructors
  `Arc::default()`), `mod l2; mod relay;`; methods `want_book` /
  `want_book_at` (refuses a coin no dex lists, then `relay.touch`),
  `want_books`, `lease_coin`, `wanted_coins` (sweep + `evict_book`:
  remove `books`/`book_at`/`book_live_at`, then `notify()`),
  `permanent_ctx_coins` (every non-delisted coin of every dex's universe),
  `desired_streams` (relay.desired(permanent, wanted, now)),
  `confirm_books`, `set_relay_streams` (metrics), `ensure_book`.
- `api/info.rs`: `l2Book` reads go through `want_book` (keeps the coin
  wanted) and may hit `ensure_book`.
- `sim`: `held_coins()` — verify ours has it (upstream's tick uses it).
- `book()` needs **no change**: identical in both trees.

## Discipline

Unrelated histories — port file by file, each step green
(`cargo test -p diorama-venue`, `cargo clippy -p diorama-venue --all-targets
-- -D warnings`) and verified against the live mirror (`/readyz`, freshness,
a `venue-watch` beat on a coin outside the configured set) before the next.
The venue image rolls separately from the daloc line, never multi-image.
