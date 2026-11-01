//! The account plane: the simulated accounts registry (COW overlay), its
//! persistence, and the self-advance automaton.
//!
//! The modules: `account` (the account math and the position totals), `seed`
//! (the served-shape patch), `funding` (the hourly interval), `order` (the
//! matching lane), `validate` (the lane's rules), `waterfall` (the liquidation
//! ladder), `tick` (the self-advance automaton), `metrics` (the registry).
//!
//! Untouched accounts are not the sim's business — the API layer passes those
//! reads through to the real venue. A *touched* account (dealt funds, seeded,
//! or otherwise registered) is simulated: its state is this registry's, its
//! figures are computed from live marks, and it persists to `sim.json` so a
//! restart does not lose a scenario.
//!
//! Time: the sim has its own clock (`wall + offset`, offset = 0 until
//! `advance` scripts a jump). Funding steps at the hour on that clock (the
//! docs' interval: whole hourly payments, nothing between — `accrue_funding`)
//! and liquidations are checked against current marks — the §14.8 evaluation
//! runs on every account-scoped read and on the 1 Hz tick — so daloc's
//! set-and-read tests see live liquidation behavior without ever calling
//! `advance`, and script a funding step by crossing the hour. The
//! wire's `time` field stays *wall* now: it is the read's freshness, which the
//! client's ≤6 s staleness gate compares against its own clock.
/// The artifact's name in errors and load messages: one home.
const ARTIFACT: &str = "sim state";

pub(crate) mod account;
pub(crate) mod funding;
pub(crate) mod metrics;
pub(crate) mod order;
pub(crate) mod seed;
pub(crate) mod tick;
pub(crate) mod validate;
pub(crate) mod waterfall;

use crate::error::{Error, Result};
use crate::feed::MarketFeed;
use crate::wire::UserAbstraction;
use account::{Account, Accounts, Liquidation};
use alloy_primitives::Address;
use diorama_devnet::store;
use diorama_devnet::Shared;
use metrics::SimMetrics;
use rust_decimal::Decimal;
use seed::AccountSeed;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Which wallet a USD-class transfer moves funds to (CoreWriter action 7's
/// `toPerp` flag, named).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    ToPerp,
    ToSpot,
}

/// Whether an action applied: a transfer's source side must hold the funds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Applied {
    Applied,
    Refused,
}

/// One USD-class transfer's inputs (CoreWriter action 7 applied): the token
/// moved, the amount, and the direction. One struct keeps the applied
/// signature inside the repo's five-argument scope.
pub(crate) struct ClassTransfer<'a> {
    pub token: TokenRef<'a>,
    pub amount: Decimal,
    pub direction: Direction,
}

/// The persisted simulator state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct SimState {
    /// The scenario epoch: `reset` bumps it (state files say which run they
    /// belong to).
    #[serde(default)]
    pub epoch: u64,
    /// The sim clock's offset from wall time (ms); `advance` scripts jumps.
    #[serde(default)]
    pub clock_offset_ms: i64,
    /// The bridge's last scanned V block (0 = not anchored yet). Persisted so
    /// a sim restart does not lose the boundary activity mined meanwhile.
    #[serde(default)]
    pub bridge_cursor: u64,
    /// The matching lane's order-id counter (monotonic across the scenario).
    #[serde(default)]
    pub next_oid: u64,
    #[serde(default)]
    pub accounts: Accounts,
    /// The devnet's resting book (the venue's open orders): keyed by oid.
    #[serde(default)]
    pub resting: std::collections::BTreeMap<u64, crate::sim::order::RestingOrder>,
    /// The bounded terminal history (`orderStatus` answers for filled and
    /// canceled orders, as the venue does).
    #[serde(default)]
    pub terminal: std::collections::BTreeMap<u64, crate::sim::order::TerminalOrder>,
    /// The nonce sets (the venue's rule: the 100 highest per signer).
    #[serde(default)]
    pub nonces: std::collections::BTreeMap<Address, std::collections::BTreeSet<u64>>,
    /// `scheduleCancel` deadlines per account (the dead-man's switch).
    #[serde(default)]
    pub scheduled_cancels: std::collections::BTreeMap<Address, u64>,
    /// The 30 s cooldown after a partial liquidation (docs: during it, market
    /// liquidation orders are for the entire position).
    #[serde(default)]
    pub liq_cooldown_until: std::collections::BTreeMap<Address, u64>,
    /// The next instant the waterfall may retry an account whose rescue found
    /// no executable action (a refused book close with an empty ADL). Without
    /// this the thin venue retries at the tick cadence forever — measured at
    /// 1 Hz for 63 minutes — and floods the log with the same refusal.
    #[serde(default)]
    pub waterfall_retry_after: std::collections::BTreeMap<Address, u64>,
    /// The previous tick's mark per coin (ADL closes at the previous mark).
    #[serde(default)]
    pub prev_marks: std::collections::BTreeMap<String, Decimal>,
}

/// One `deal`: mint funds into the account (perp wallet or spot token).
/// Absolute-additive: it raises the balance by `amount` (the harness's
/// `add_erc20` semantics; draining is `amount` negative or a `set_account`
/// seed). A **spot** drain may not take the balance below zero: a spot balance
/// is a holding, not a signed equity, and the venue cannot owe the trader a
/// negative token balance. The control route's request body is this type — one
/// shape for the wire and the call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Deal {
    pub address: Address,
    pub class: DealClass,
    /// The spot coin (`None` = USDC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coin: Option<String>,
    pub amount: Decimal,
}

/// A spot token's identity as the sim's ledger keys it: the index the spot
/// state maps by and the venue's name (served on the rows) — one argument
/// instead of two that always travel together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenRef<'a> {
    pub index: u32,
    pub coin: &'a str,
}

/// Which side of the venue `deal` mints into.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DealClass {
    /// The perp wallet (`totalRawUsd`; the enforcement's recall source).
    Perp,
    /// The spot wallet (by token; USDC is token 0).
    Spot,
}

/// The liquidator vault (docs' HLP address): the backstop that takes
/// positions over below ⅔ of the maintenance requirement.
pub const BACKSTOP: Address = Address::new([
    0xdf, 0xc2, 0x4b, 0x07, 0x7b, 0xc1, 0x42, 0x5a, 0xd1, 0xde, 0xa7, 0x5b, 0xcb, 0x6f, 0x81, 0x58,
    0xe1, 0x0d, 0xf3, 0x03,
]);

pub struct Sim {
    state: Shared<SimState>,
    path: Option<PathBuf>,
    /// The `orderStatus` terminal-history bound (the deployment's policy).
    terminal_orders: usize,
    dirty: AtomicBool,
    /// The venue chain's head timestamp (ms), observed by the bridge when V is
    /// configured. Their time policy jumps *every* chain's clock forward and
    /// the client's own rule is `chain_time = max(wall, head)` — a served
    /// `time` that stayed at wall would read as stale after a V jump.
    chain_time_ms: AtomicU64,
    pub(crate) metrics: SimMetrics,
}

/// Whether the flush wrote (the state was dirty) or found it already clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flushed {
    Written,
    Clean,
}

impl Sim {
    /// Load the registry from its state file (absent = first boot, empty).
    /// `terminal_orders` is the deployment's `orderStatus` history bound.
    pub fn load(path: PathBuf, terminal_orders: usize) -> Result<Self> {
        let state = store::load::<SimState>(&path, ARTIFACT)?.unwrap_or_default();
        Ok(Self::from_state(state, Some(path), terminal_orders))
    }

    /// An in-memory registry (tests only: no production path builds one).
    #[cfg(test)]
    pub fn memory() -> Self {
        Self::from_state(SimState::default(), None, 1024)
    }

    fn from_state(state: SimState, path: Option<PathBuf>, terminal_orders: usize) -> Self {
        let sim = Self {
            state: Shared::new(ARTIFACT, state),
            path,
            terminal_orders,
            dirty: AtomicBool::new(false),
            chain_time_ms: AtomicU64::new(0),
            metrics: SimMetrics::default(),
        };
        sim.publish_gauges();
        sim
    }

    // ---- reads ------------------------------------------------------------

    /// The account, or `None` when it was never touched (the caller passes
    /// the read through to the real venue).
    pub(crate) fn account(&self, address: Address) -> Result<Option<Account>> {
        self.with_state(|state| Ok(state.accounts.get(&address).cloned()))
    }

    /// The sim clock (ms) at wall time `now_ms`.
    pub fn clock(&self, now_ms: u64) -> Result<u64> {
        let offset = self.with_state(|state| Ok(state.clock_offset_ms))?;
        Ok((now_ms as i64 + offset).max(0) as u64)
    }

    /// Observe the venue chain's head timestamp (the bridge calls this per
    /// pass; a no-op when V is not configured).
    pub fn observe_chain_time(&self, ms: u64) {
        self.chain_time_ms.store(ms, Ordering::Relaxed);
    }

    /// The `time` a venue read serves: never behind wall, never behind the
    /// sim's own clock (an `advance` jump), never behind the venue chain's
    /// clock (their harness jumps every chain forward).
    pub fn served_time(&self, wall_ms: u64) -> Result<u64> {
        Ok(self
            .clock(wall_ms)?
            .max(wall_ms)
            .max(self.chain_time_ms.load(Ordering::Relaxed)))
    }

    /// The scenario epoch counter (`reset` returns it to callers; `dry_run`
    /// reads it without mutating).
    pub fn epoch(&self) -> Result<u64> {
        self.with_state(|state| Ok(state.epoch))
    }

    /// The state lock is poisoned (a panic happened mid-update): every
    /// account path refuses until a restart — health/readiness must say so.
    pub fn is_poisoned(&self) -> bool {
        self.state.lock().is_err()
    }

    /// The bridge's scan cursor (0 = not anchored).
    pub fn bridge_cursor(&self) -> Result<u64> {
        self.with_state(|state| Ok(state.bridge_cursor))
    }

    /// Move the bridge's cursor (persisted with the next save).
    pub fn set_bridge_cursor(&self, block: u64) -> Result<()> {
        self.with_state(|state| {
            state.bridge_cursor = block;
            Ok(())
        })?;
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    // ---- scenario mutations ----------------------------------------------

    /// `deal`: mint funds into the account (perp wallet or spot token).
    /// Absolute-additive: it raises the balance by `amount` (the harness's
    /// `add_erc20` semantics; draining is `amount` negative or a `set_account`
    /// seed).
    /// The account plane's one write door: lock the state and run `f` on the
    /// account, creating it on first touch. Two disciplines ride here — read
    /// the clock *before* calling (`clock()` locks the same mutex, and a std
    /// mutex is not reentrant), and mark the state dirty *after* (the guard
    /// drops when this returns: `touch` for a batched writer, `mark_dirty` to
    /// persist now). A debit never creates: it reads through `get_mut`.
    fn with_account<T>(
        &self,
        address: Address,
        f: impl FnOnce(&mut Account) -> Result<T>,
    ) -> Result<T> {
        let mut state = self.state.lock()?;
        f(state.accounts.entry(address).or_default())
    }

    /// Run one critical section against the sim's state — the raw door, for
    /// the state that is not one account (orders, cooldowns, marks, nonces).
    /// The guard lives for the closure only; the account verbs
    /// (`with_account`, `with_account_now`) are the account-plane doors, and
    /// `mark_dirty`/`touch` stay at the call sites (the bridge saves per
    /// block, not per write).
    fn with_state<T>(&self, f: impl FnOnce(&mut SimState) -> Result<T>) -> Result<T> {
        let mut state = self.state.lock()?;
        f(&mut state)
    }

    /// Mutate one account at the venue's clock, funding accrued to it first:
    /// the venue settles funding at the trade (on the *pre-fill* size), so
    /// every value-changing path uses this. `with_account` is the raw form for
    /// paths that must not touch funding (baseline stamps, control-plane
    /// patches).
    fn with_account_now<T>(
        &self,
        address: Address,
        feed: &MarketFeed,
        f: impl FnOnce(&mut Account) -> Result<T>,
    ) -> Result<T> {
        let clock = self.clock(crate::now_ms())?;
        self.with_account(address, |account| {
            funding::accrue_funding(account, feed, clock)?;
            f(account)
        })
    }

    /// One account's mutation, then the dirty flag: the account verbs' shared
    /// tail (`with_account` + `touch`). The conditional paths (a refused
    /// transfer) touch themselves, and `mark_dirty` is the durable writer's
    /// flag — this is the plain mutation.
    fn mutate_account(
        &self,
        address: Address,
        f: impl FnOnce(&mut Account) -> Result<()>,
    ) -> Result<()> {
        self.with_account(address, f)?;
        self.touch();
        Ok(())
    }

    /// One state mutation, then the durable save: the writer's shared tail
    /// (`with_state` + `mark_dirty`). `touch` is the batched flag; this one
    /// saves, for the verbs whose write must survive the call.
    fn mutate_state<T>(&self, f: impl FnOnce(&mut SimState) -> Result<T>) -> Result<T> {
        let out = self.with_state(f)?;
        self.mark_dirty()?;
        Ok(out)
    }

    pub(crate) fn deal(&self, feed: &MarketFeed, deal: &Deal) -> Result<Decimal> {
        let now = crate::now_ms();
        let clock = self.clock(now)?;
        let after = self.with_account(deal.address, |account| {
            stamp_funding_baseline(account, clock);
            match deal.class {
                DealClass::Perp => {
                    account.perp_balance =
                        account::add(account.perp_balance, deal.amount, "perp deal")?;
                    Ok(account.perp_balance)
                }
                DealClass::Spot => {
                    let coin = deal.coin.as_deref().unwrap_or("USDC");
                    let index = feed.token_of(coin).ok_or_else(|| Error::bad_request(format!(
                            "spot coin `{coin}` is not in the venue's token registry yet"
                        ),))?;
                    let entry = account::spot_entry(account, TokenRef { index, coin });
                    let after = account::add(entry.total, deal.amount, "spot deal")?;
                    // A spot balance is a **holding**: the venue's own spot state cannot carry a
                    // negative token balance, and a negative row makes the indexer's series reject
                    // the tick (`hold <= total`) — the measured 2026-10-27 `2dbf72` hole. The
                    // primitive is the one place every caller reaches, so the refusal lives here
                    // rather than only on the control route.
                    if after < Decimal::ZERO {
                        return Err(Error::bad_request(format!(
                            "spot deal would overdraw {coin}: balance {}, amount {}, after {after}; a spot balance is a holding and cannot go negative",
                            entry.total, deal.amount
                        )));
                    }
                    entry.total = after;
                    Ok(after)
                }
            }
        })?;
        self.mark_dirty()?;
        Ok(after)
    }

    /// `set_account`: seed an account from the hl_mock-compatible shape. The
    /// seed is a **patch** (their `venue_mock::set_venue_account` contract and
    /// their mock's own apply): a field the caller does not name keeps its
    /// previous value — a perp-only seed must not wipe the account's spot
    /// balances. The field names are the patch shape's (`userAbstraction`,
    /// `spotBalances`). An all-empty seed is refused: it would silently
    /// register a zero account.
    pub(crate) fn set_account(&self, address: Address, seed: AccountSeed) -> Result<()> {
        if seed.is_empty() {
            return Err(Error::bad_request("the patch is empty (no field named)"));
        }
        let now = crate::now_ms();
        let clock = self.clock(now)?;
        self.with_account(address, |account| {
            let before_perp = account.perp_balance;
            let before_spot = account.spot.clone();
            let before_positions = account.positions.len();
            let patch = seed.clone();
            seed.apply_to(account, clock)?;
            // Remember the patch as the control fixture, merged over the
            // previous one (hl_mock's rule: a named field replaces, an
            // unnamed one keeps).
            let merged = seed.merged_with(account.control.take());
            account.control = Some(merged);
            // The control plane is a test surface, and a balance patch is a
            // scenario input, not a market observation. Log it with the exact
            // before/after figures and the patch id so an operator can
            // attribute the state change even when the caller's identity is
            // not authenticated.
            tracing::warn!(
                account = %address,
                patch_id = now,
                before_perp_balance = %before_perp,
                after_perp_balance = %account.perp_balance,
                before_spot = ?before_spot,
                after_spot = ?account.spot,
                before_positions,
                after_positions = account.positions.len(),
                patch = ?patch,
                "control: set_account patch applied"
            );
            Ok(())
        })?;
        self.mark_dirty()
    }

    /// Clear one account entirely: the control fixture, its balances and its
    /// positions. This is the explicit recovery door for a scenario write that
    /// must not survive into the next measurement; it is destructive by design
    /// and the route requires `{"confirm":"clear_account"}`.
    pub(crate) fn clear_account(&self, address: Address) -> Result<bool> {
        let removed = self.with_state(|state| Ok(state.accounts.remove(&address).is_some()))?;
        if removed {
            tracing::warn!(account = %address, "control: account cleared (fixture and balances removed)");
            self.mark_dirty()?;
        }
        Ok(removed)
    }

    /// Clear only the control-fixture provenance, keeping the account's
    /// balances and positions. This is the acknowledgement for a **setup**
    /// patch (a mode or a deliberate fixture) whose state is the intended live
    /// state; it must never be used to launder a balance mutation. A destructive
    /// balance patch is cleared with [`Self::clear_account`] and re-funded.
    pub(crate) fn clear_control_fixture(&self, address: Address) -> Result<bool> {
        let cleared = self.with_account(address, |account| Ok(account.control.take().is_some()))?;
        if cleared {
            tracing::warn!(
                account = %address,
                "control: fixture provenance cleared (balances and positions kept)"
            );
            self.mark_dirty()?;
        }
        Ok(cleared)
    }

    /// The account's control fixture (the merged patch), when set.
    pub(crate) fn control_fixture(&self, address: Address) -> Result<Option<AccountSeed>> {
        Ok(self
            .state
            .lock()?
            .accounts
            .get(&address)
            .and_then(|a| a.control.clone()))
    }

    // ---- the Core↔EVM bridge's effects -----------------------------------

    /// EVM→Core: credit a spot token (creates the account on first touch).
    pub(crate) fn credit_spot(
        &self,
        address: Address,
        token: TokenRef<'_>,
        amount: Decimal,
    ) -> Result<()> {
        // Read the clock *before* taking the state lock: `clock()` locks the
        // same mutex, and a std Mutex is not reentrant.
        let clock = self.clock(crate::now_ms())?;
        self.mutate_account(address, |account| {
            stamp_funding_baseline(account, clock);
            account::credit_entry(account, token, amount, "spot credit")?;
            Ok(())
        })
    }

    /// Core→EVM / spot send: debit the spot balance; `None` when the account
    /// or the balance is short (the bridge counts the refusal — a system
    /// transfer never goes negative).
    pub fn try_debit_spot(
        &self,
        address: Address,
        token: u32,
        amount: Decimal,
    ) -> Result<Option<Decimal>> {
        let debited = self.with_state(|state| {
            let Some(account) = state.accounts.get_mut(&address) else {
                return Ok(None);
            };
            let Some(entry) = account.spot.get_mut(&token) else {
                return Ok(None);
            };
            if entry.total < amount {
                return Ok(None);
            }
            entry.total = account::sub(entry.total, amount, "spot debit")?;
            Ok(Some(entry.total))
        })?;
        let Some(after) = debited else {
            return Ok(None);
        };
        self.touch();
        Ok(Some(after))
    }

    /// USD class transfer (CoreWriter action 7): spot USDC ↔ perp wallet.
    /// `Refused` when the move exceeds the mode's bound.
    ///
    /// The bound depends on the mode. A **standard** account has two ledgers:
    /// the perp view's cash bounds the draw. A **unified** account's USDC is
    /// ONE pool (the docs: "USDC unifies perps + spot"; the fold treats the
    /// collateral spot side as part of the raw balance), so a perp→spot class
    /// transfer re-splits the pool's views — the pool's cash and every served
    /// figure (`accountValue`, `withdrawable`, the identity) are
    /// view-invariant by construction — and the docs' transfer rule (the
    /// margin floor: "transferring includes ... transfer to spot wallet")
    /// bounds it. The perp view may go negative: the pool still holds the
    /// value, exactly as the venue's cross ledger runs a negative balance
    /// under water. The perp view's cash alone is not the unified bound — the
    /// product's recall lands its note on the spot side, so the perp view
    /// routinely holds a fraction of the pool, and a perp-cash bound would
    /// refuse a withdrawal the venue's own rule allows.
    pub(crate) fn usd_class_transfer(
        &self,
        address: Address,
        transfer: &ClassTransfer<'_>,
        feed: &MarketFeed,
    ) -> Result<Applied> {
        let clock = self.clock(crate::now_ms())?;
        let applied = self.with_account(address, |account| {
            stamp_funding_baseline(account, clock);
            match transfer.direction {
                Direction::ToPerp => {
                    let entry = account::spot_entry(account, transfer.token);
                    if entry.total < transfer.amount {
                        return Ok(Applied::Refused);
                    }
                    entry.total = account::sub(entry.total, transfer.amount, "usd class transfer")?;
                    account.perp_balance =
                        account::add(account.perp_balance, transfer.amount, "usd class transfer")?;
                }
                Direction::ToSpot => {
                    if account.mode == UserAbstraction::UnifiedAccount {
                        let meta = feed.meta();
                        // A mark not yet served (`view` faults) is a refusal,
                        // never a guess — the caller re-sends.
                        let Some(bound) =
                            account::unified_transfer_bound(account, feed, meta.as_ref())
                        else {
                            tracing::warn!(account = %address, amount = %transfer.amount,
                                "usd class transfer refused: the unified account's marks are not served yet");
                            return Ok(Applied::Refused);
                        };
                        if transfer.amount > bound {
                            tracing::warn!(account = %address, amount = %transfer.amount, withdrawable = %bound,
                                "usd class transfer refused: the amount exceeds the unified account's withdrawable (the docs' transfer floor)");
                            return Ok(Applied::Refused);
                        }
                    } else if account.perp_balance < transfer.amount {
                        return Ok(Applied::Refused);
                    }
                    // The unified pool re-splits: the perp view may go negative.
                    account.perp_balance =
                        account::sub(account.perp_balance, transfer.amount, "usd class transfer")?;
                    account::credit_entry(account, transfer.token, transfer.amount, "usd class transfer")?;
                }
            }
            Ok(Applied::Applied)
        })?;
        if matches!(applied, Applied::Applied) {
            self.touch();
        }
        Ok(applied)
    }

    /// Core spot send: debit the sender, credit the recipient. `Refused` when
    /// the sender is short.
    pub(crate) fn spot_send(
        &self,
        from: Address,
        to: Address,
        token: TokenRef<'_>,
        amount: Decimal,
    ) -> Result<Applied> {
        let debited = self.with_state(|state| {
            let Some(sender) = state.accounts.get_mut(&from) else {
                return Ok(Applied::Refused);
            };
            let Some(entry) = sender.spot.get_mut(&token.index) else {
                return Ok(Applied::Refused);
            };
            if entry.total < amount {
                return Ok(Applied::Refused);
            }
            entry.total = account::sub(entry.total, amount, "spot send")?;
            Ok(Applied::Applied)
        })?;
        if matches!(debited, Applied::Refused) {
            return Ok(Applied::Refused);
        }
        self.credit_spot(to, token, amount)?;
        Ok(Applied::Applied)
    }

    /// Record an API wallet (CoreWriter action 9).
    pub fn note_agent(&self, address: Address, wallet: Address, name: &str) -> Result<()> {
        self.mutate_account(address, |account| {
            if account.agents.iter().all(|a| a.wallet != wallet) {
                account.agents.push(account::AgentWallet {
                    wallet,
                    name: name.to_string(),
                });
            }
            Ok(())
        })
    }

    /// Set the account's abstraction mode (CoreWriter action 16). Portfolio
    /// margin is refused by name (see `account::refuse_unmodeled_mode`).
    pub(crate) fn set_mode(
        &self,
        address: Address,
        mode: crate::wire::UserAbstraction,
    ) -> Result<()> {
        account::refuse_unmodeled_mode(mode)?;
        self.mutate_account(address, |account| {
            account.mode = mode;
            Ok(())
        })
    }

    /// `reset`: blank the scenario, bump the epoch.
    ///
    /// A reset is an **epoch boundary**, not just an account wipe. Leaving the
    /// resting book, terminal history, nonces, cooldowns or previous marks
    /// behind makes the new epoch inherit orders and replay fences from the
    /// old one. The bridge cursor is the exception: it is a V-block anchor,
    /// not scenario state, and clearing it would re-scan (and potentially
    /// re-credit) the bridge's history. It is retained and named in the log.
    pub fn reset(&self) -> Result<u64> {
        let epoch = self.with_state(|state| {
            let accounts = state.accounts.len();
            let resting = state.resting.len();
            state.accounts.clear();
            state.resting.clear();
            state.terminal.clear();
            state.nonces.clear();
            state.scheduled_cancels.clear();
            state.liq_cooldown_until.clear();
            state.waterfall_retry_after.clear();
            state.prev_marks.clear();
            state.next_oid = 0;
            state.clock_offset_ms = 0;
            state.epoch += 1;
            tracing::info!(
                epoch = state.epoch,
                accounts,
                resting,
                bridge_cursor = state.bridge_cursor,
                "control: reset blanked the scenario (bridge cursor retained)"
            );
            Ok(state.epoch)
        })?;
        self.save()?;
        Ok(epoch)
    }

    /// `advance`: script a clock jump (`seconds`) with `blocks` liquidation
    /// checks inside it — the deterministic complement to self-advance. A
    /// zero check count is refused: silently running one would change the
    /// scenario's cadence behind the caller's back.
    pub(crate) fn advance(
        &self,
        feed: &MarketFeed,
        seconds: u64,
        blocks: u64,
    ) -> Result<AdvanceOutcome> {
        if blocks == 0 {
            return Err(Error::bad_request(
                "advance n must be at least 1 (it names the liquidation checks)",
            ));
        }
        let jump_ms = seconds.saturating_mul(1_000);
        self.with_state(|state| {
            state.clock_offset_ms = state.clock_offset_ms.saturating_add(jump_ms as i64);
            Ok(())
        })?;
        let now = crate::now_ms();
        let mut liquidations = Vec::new();
        for _ in 0..blocks {
            self.accrue_all(feed, now)?;
            liquidations.extend(self.check_all(feed, now)?);
        }
        self.save()?;
        self.metrics.advances_total.fetch_add(1, Ordering::Relaxed);
        Ok(AdvanceOutcome {
            advanced_ms: jump_ms,
            blocks,
            liquidations,
        })
    }

    // ---- the automaton ----------------------------------------------------

    /// The addresses with Core account state (the bridge's pre-block set for
    /// the docs' CoreWriter pre-existence rule).
    pub(crate) fn account_addresses(&self) -> Result<std::collections::HashSet<Address>> {
        Ok(self.state.lock()?.accounts.keys().copied().collect())
    }

    /// `updateIsolatedMargin` (docs): move USDC between the perp balance and
    /// the position's isolated margin (`ntli` is 6-decimal, signed). On a
    /// *strict isolated* asset the margin cannot be removed (docs) — a
    /// negative `ntli` is refused by name (the venue's own string for this is
    /// not published).
    pub(crate) fn update_isolated_margin(
        &self,
        address: Address,
        coin: &str,
        ntli: i64,
        feed: &MarketFeed,
    ) -> Result<()> {
        let meta = feed.meta();
        let delta = Decimal::from(ntli) / Decimal::from(1_000_000);
        if delta < Decimal::ZERO && account::strict_isolated(coin, meta.as_ref()) {
            return Err(crate::error::Error::bad_request(
                "Strict isolated margin cannot be removed.",
            ));
        }
        self.with_account(address, |account| {
            let Some(p) = account.positions.get_mut(coin) else {
                return Err(crate::error::Error::bad_request(format!(
                    "no {coin} position to margin (the docs' `updateIsolatedMargin` targets a position)"
                )));
            };
            let next = account::add(p.isolated_margin, delta, account::ISOLATED_MARGIN)?;
            if next < Decimal::ZERO {
                return Err(crate::error::Error::bad_request(
                    "isolated margin cannot go below zero",
                ));
            }
            // The docs' transfer rule (margining): "Unrealized pnl can be
            // withdrawn from isolated positions or cross account, but only if
            // the remaining margin is at least 10% of the total notional
            // position value ... transfer_margin_required =
            // max(initial_margin_required, 0.1 * total_position_value)" — and
            // "transferring" includes isolated margin transfers.
            if delta < Decimal::ZERO {
                let mark = feed.mark(coin).ok_or(crate::error::Error::Warming)?;
                let required = account::transfer_floor(p, mark)?;
                if next < required {
                    return Err(crate::error::Error::bad_request(format!(
                        "Isolated margin transfer would leave {next} below the required                          {required} (the initial margin or 10% of the position's notional;                          docs: margining)"
                    )));
                }
            }
            account.perp_balance = account::sub(account.perp_balance, delta, "isolated margin move")?;
            p.isolated_margin = next;
            p.leverage = crate::wire::LeverageType::Isolated;
            if !next.is_zero() {
                p.margin_used = next;
            }
            Ok(())
        })
    }

    /// The venue's nonce rule: the 100 highest nonces per signer,
    /// accepted in the window (T − 2 d, T + 1 d); a duplicate is refused by
    /// name. Called only when the body carries a nonce (their harness's
    /// nonce-less shape stays accepted — the devnet's impersonation analogue).
    pub(crate) fn use_nonce(&self, signer: Address, nonce: u64, now_ms: u64) -> Result<()> {
        const TWO_DAYS_MS: u64 = 2 * 24 * 60 * 60 * 1_000;
        const ONE_DAY_MS: u64 = 24 * 60 * 60 * 1_000;
        self.mutate_state(|st| {
            let set = st.nonces.entry(signer).or_default();
            if set.contains(&nonce) {
                return Err(crate::error::Error::bad_request("Nonce already used."));
            }
            // The venue keeps the 100 highest nonces per signer and a nonce
            // must be *larger* than the smallest retained (docs) — otherwise
            // an action pushed out of the set could be replayed. The venue's
            // own string is not published; named here.
            if let Some(floor) = set.first().copied() {
                if nonce <= floor {
                    return Err(crate::error::Error::bad_request(
                        "Nonce is too low (the venue keeps the 100 highest per signer).",
                    ));
                }
            }
            if nonce.saturating_add(TWO_DAYS_MS) < now_ms
                || nonce > now_ms.saturating_add(ONE_DAY_MS)
            {
                return Err(crate::error::Error::bad_request(
                    "Nonce is outside the accepted window.",
                ));
            }
            set.insert(nonce);
            while set.len() > 100 {
                set.pop_first();
            }
            Ok(())
        })
    }

    pub fn save_if_dirty(&self) -> Result<Flushed> {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return Ok(Flushed::Clean);
        }
        self.save()?;
        Ok(Flushed::Written)
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            self.dirty.store(false, Ordering::Relaxed);
            return Ok(());
        };
        let snapshot = self.with_state(|state| Ok(state.clone()))?;
        store::save(path, &snapshot, ARTIFACT)?;
        self.dirty.store(false, Ordering::Relaxed);
        self.publish_gauges();
        Ok(())
    }

    /// Mark the state dirty without saving: the bridge applies a block's
    /// effects and the cursor together, then saves once — a crash mid-block
    /// loses both (re-scan) rather than saving a half-applied block.
    fn mark_dirty(&self) -> Result<()> {
        self.dirty.store(true, Ordering::Relaxed);
        self.save()
    }

    fn touch(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn publish_gauges(&self) {
        if let Ok((accounts, epoch)) =
            self.with_state(|state| Ok((state.accounts.len() as u64, state.epoch)))
        {
            self.metrics.accounts.store(accounts, Ordering::Relaxed);
            self.metrics.epoch.store(epoch, Ordering::Relaxed);
        }
    }
}

/// What `advance` moved.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdvanceOutcome {
    pub advanced_ms: u64,
    pub blocks: u64,
    pub liquidations: Vec<Liquidation>,
}

/// First touch of an account establishes its funding baseline (an account
/// created by the bridge must not inherit funding from the epoch's start).
fn stamp_funding_baseline(account: &mut Account, clock: u64) {
    if account.funded_at_ms == 0 {
        account.funded_at_ms = clock;
    }
}

/// The sim tests' feed fixture: one market per triple with a rate-free ctx
/// (`crate::wire::test_ctx`), stamped at `now_ms = 1`. The account tests
/// share it through `super::*`.
#[cfg(test)]
pub(crate) fn feed_with(marks: &[(&str, &str, &str)]) -> MarketFeed {
    let feed = MarketFeed::memory(marks.iter().map(|(c, _, _)| c.to_string()).collect());
    for (coin, mark, oracle) in marks {
        feed.put_ctx(coin.to_string(), crate::wire::test_ctx(mark, oracle), 1);
    }
    feed
}

/// The same fixture with the published hourly rate on every coin: the
/// funding tests opt in (see `test_ctx` for why the shared fixture is
/// rate-free).
#[cfg(test)]
pub(crate) fn feed_with_funding(marks: &[(&str, &str, &str)]) -> MarketFeed {
    let feed = feed_with(marks);
    for (coin, mark, oracle) in marks {
        feed.put_ctx(coin.to_string(), crate::wire::funding_ctx(mark, oracle), 1);
    }
    feed
}

/// A `Meta` with the given assets (the sim's fixtures: the waterfall's
/// maintenance tiers and the order tests' book both read it).
#[cfg(test)]
pub(crate) fn meta_of(assets: &[(&str, u32)]) -> crate::wire::Meta {
    let mut m: crate::wire::Meta = serde_json::from_str(r#"{"universe": []}"#).unwrap();
    for (coin, max_leverage) in assets {
        m.universe.push(crate::wire::AssetInfo {
            name: (*coin).into(),
            sz_decimals: 4,
            max_leverage: *max_leverage,
            margin_table_id: None,
            only_isolated: None,
            is_delisted: None,
            margin_mode: None,
        });
    }
    m
}

/// A `Meta` with one asset.
#[cfg(test)]
pub(crate) fn meta_with(coin: &str, max_leverage: u32) -> crate::wire::Meta {
    meta_of(&[(coin, max_leverage)])
}

/// A `Meta` whose one asset carries a margin table (the account tests'
/// leverage tiers).
#[cfg(test)]
pub(crate) fn meta_with_table(coin: &str, max_leverage: u32) -> crate::wire::Meta {
    crate::wire::Meta {
        universe: vec![crate::wire::AssetInfo {
            name: coin.to_string(),
            sz_decimals: 5,
            max_leverage,
            margin_table_id: Some(50),
            only_isolated: None,
            is_delisted: None,
            margin_mode: None,
        }],
        margin_tables: vec![(
            50,
            crate::wire::MarginTable {
                description: String::new(),
                margin_tiers: vec![crate::wire::MarginTier {
                    lower_bound: Decimal::ZERO,
                    max_leverage,
                }],
            },
        )],
        collateral_token: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unified account's USD-class transfer is bounded by the docs'
    /// transfer rule (the margin floor — `withdrawable`), not by the perp
    /// view's cash: the unified USDC is ONE pool and the class transfer
    /// re-splits its views, so the pool's cash, `accountValue` and
    /// `withdrawable` must be unchanged by the move. This is the R5 recall
    /// shape (run 2026-10-31 11:40Z): the note lands on the spot side, the
    /// perp view holds only the trading cash, and the venue's own rule allows
    /// withdrawing up to `withdrawable` — the old perp-cash bound refused a
    /// legal recall and stranded the recovery sweep.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn unified_class_transfer_is_bounded_by_the_transfer_rule_not_the_perp_view() {
        let feed = feed_with(&[("HYPE", "87.87", "87.87")]);
        let meta = meta_with_table("HYPE", 10);
        // The account plane's maintenance reads the live meta; the fixture's
        // feed carries it (a warming feed would fail closed by design).
        feed.put_meta(meta.clone(), crate::now_ms());
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xb0);
        // The R5 fixture shape: a unified account short −0.08 HYPE @ 87.87
        // (cross 10×, margin 0.1×positionValue), the note (7.865) on the spot
        // side, the perp view funded by the deal (7.82 − 7.0296 = 0.7904).
        let seed: AccountSeed = serde_json::from_str(
            r#"{"userAbstraction": "unifiedAccount",
                "marginSummary": {"accountValue": "-7.0296", "totalNtlPos": "7.0296", "totalRawUsd": "0", "totalMarginUsed": "0.70296"},
                "assetPositions": [{"type": "oneWay", "position": {"coin": "HYPE", "entryPx": "87.87",
                    "leverage": {"type": "cross", "value": 10}, "marginUsed": "0.70296", "maxLeverage": 10,
                    "positionValue": "7.0296", "returnOnEquity": "0", "szi": "-0.08", "unrealizedPnl": "0",
                    "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}}}]}"#,
        )
        .unwrap();
        sim.set_account(a, seed).unwrap();
        sim.deal(
            &feed,
            &Deal {
                address: a,
                class: DealClass::Perp,
                coin: None,
                amount: "7.82".parse().unwrap(),
            },
        )
        .unwrap();
        sim.credit_spot(
            a,
            TokenRef {
                index: account::COLLATERAL_TOKEN,
                coin: "USDC",
            },
            "7.865".parse().unwrap(),
        )
        .unwrap();

        let acct = sim
            .account(a)
            .unwrap()
            .expect("the seed registers the account");
        let before = account::clearinghouse(&acct, &feed, Some(&meta), 1).unwrap();
        // cash 0.7904 + 7.865 = 8.6554; raw = cash + |cost basis| = 15.685;
        // accountValue = raw − positionValue = 8.6554.
        assert_eq!(
            before.margin_summary.account_value,
            "8.6554".parse::<Decimal>().unwrap()
        );
        assert_eq!(
            before.margin_summary.total_raw_usd,
            "15.685".parse::<Decimal>().unwrap()
        );
        let withdrawable = before.withdrawable;
        let transfer = |amount: Decimal| ClassTransfer {
            token: TokenRef {
                index: account::COLLATERAL_TOKEN,
                coin: "USDC",
            },
            amount,
            direction: Direction::ToSpot,
        };

        // The recall's clamped figure (the venue's own withdrawable) moves —
        // the perp view goes negative while the pool's cash is unchanged.
        let applied = sim
            .usd_class_transfer(a, &transfer(withdrawable), &feed)
            .unwrap();
        assert!(
            matches!(applied, Applied::Applied),
            "the venue's own rule allows the move"
        );
        let acct = sim.account(a).unwrap().unwrap();
        let after = account::clearinghouse(&acct, &feed, Some(&meta), 1).unwrap();
        assert_eq!(
            after.margin_summary.account_value, before.margin_summary.account_value,
            "the pool is view-invariant"
        );
        assert_eq!(after.withdrawable, withdrawable, "the floor did not move");
        assert_eq!(
            after.margin_summary.total_raw_usd, before.margin_summary.total_raw_usd,
            "raw is view-invariant"
        );
        assert!(
            account::identity_holds(&after),
            "the identity survives the re-split"
        );

        // More than the rule allows is refused — the venue never pays out
        // past its transfer floor.
        let refused = sim
            .usd_class_transfer(a, &transfer(withdrawable + Decimal::ONE), &feed)
            .unwrap();
        assert!(matches!(refused, Applied::Refused));
    }

    /// A *standard* account keeps its two ledgers: the perp side's cash bounds
    /// the class transfer, exactly as before.
    #[test]
    fn standard_class_transfer_still_bounded_by_the_perp_view() {
        let feed = feed_with(&[("HYPE", "87.87", "87.87")]);
        let meta = meta_with_table("HYPE", 10);
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xb1);
        sim.deal(
            &feed,
            &Deal {
                address: a,
                class: DealClass::Perp,
                coin: None,
                amount: "0.79".parse().unwrap(),
            },
        )
        .unwrap();
        let transfer = ClassTransfer {
            token: TokenRef {
                index: account::COLLATERAL_TOKEN,
                coin: "USDC",
            },
            amount: "7.95".parse().unwrap(),
            direction: Direction::ToSpot,
        };
        let applied = sim.usd_class_transfer(a, &transfer, &feed).unwrap();
        assert!(
            matches!(applied, Applied::Refused),
            "the perp view's cash still bounds a standard account"
        );
        let _ = &meta;
    }

    /// The bridge's effects take the state lock; a re-entrant lock (calling
    /// `clock()` while holding it) deadlocks the whole service. Guarded with
    /// a timeout so a regression fails the test instead of hanging CI.
    #[test]
    fn bridge_effects_do_not_self_deadlock() {
        let feed = feed_with(&[("USDC", "1", "1")]);
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let sim = sim;
            sim.credit_spot(
                a,
                TokenRef {
                    index: 0,
                    coin: "USDC",
                },
                Decimal::from(5),
            )
            .unwrap();
            sim.usd_class_transfer(
                a,
                &ClassTransfer {
                    token: TokenRef {
                        index: 0,
                        coin: "USDC",
                    },
                    amount: Decimal::ONE,
                    direction: Direction::ToPerp,
                },
                &feed,
            )
            .unwrap();
            sim.spot_send(
                a,
                Address::repeat_byte(0xbb),
                TokenRef {
                    index: 0,
                    coin: "USDC",
                },
                Decimal::ONE,
            )
            .unwrap();
            let _ = &feed;
            tx.send(()).unwrap();
        });
        // 20 s: a deadlock never answers, so the budget only has to outlast a
        // loaded parallel test run (5 s flaked once on a full-workspace run,
        // 10 s once under a full-workspace run + a build).
        rx.recv_timeout(std::time::Duration::from_secs(20))
            .expect("the bridge effects must not self-deadlock");
    }

    #[test]
    fn deal_creates_and_raises() {
        let feed = feed_with(&[("USDC", "1", "1")]);
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        assert!(sim.account(a).unwrap().is_none());
        let after = sim
            .deal(
                &feed,
                &Deal {
                    address: a,
                    class: DealClass::Perp,
                    coin: None,
                    amount: "100.5".parse().unwrap(),
                },
            )
            .unwrap();
        assert_eq!(after, "100.5".parse::<Decimal>().unwrap());
        let after = sim
            .deal(
                &feed,
                &Deal {
                    address: a,
                    class: DealClass::Perp,
                    coin: None,
                    amount: "0.5".parse().unwrap(),
                },
            )
            .unwrap();
        assert_eq!(after, "101".parse::<Decimal>().unwrap());
    }

    /// A zero check count is a caller error, not one liquidation check: the
    /// old `max(1)` silently changed the scenario's cadence.
    #[test]
    fn advance_refuses_a_zero_check_count() {
        let feed = feed_with(&[("USDC", "1", "1")]);
        let sim = Sim::memory();
        let err = sim.advance(&feed, 60, 0).unwrap_err();
        assert!(matches!(err, Error::BadRequest { .. }), "{err}");
    }

    /// The verifier's critical find: an amount that overflows must be refused
    /// cleanly (a panic under the state lock poisons every account path).
    #[test]
    #[allow(clippy::too_many_lines)] // one transition table: splitting it would hide the rule
    fn overflow_is_refused_not_panicked() {
        let feed = feed_with(&[("USDC", "1", "1")]);
        feed.put_spot_meta(crate::wire::SpotMeta {
            tokens: vec![crate::wire::SpotToken {
                name: "USDC".into(),
                sz_decimals: 8,
                wei_decimals: 8,
                index: 0,
                is_canonical: true,
                evm_contract: None,
                token_id: None,
                full_name: None,
                deployer_trading_fee_share: Decimal::ZERO,
            }],
            universe: vec![],
        });
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        let max = Decimal::MAX;
        sim.deal(
            &feed,
            &Deal {
                address: a,
                class: DealClass::Perp,
                coin: None,
                amount: max,
            },
        )
        .unwrap();
        let err = sim
            .deal(
                &feed,
                &Deal {
                    address: a,
                    class: DealClass::Perp,
                    coin: None,
                    amount: Decimal::ONE,
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::Overflow { .. }), "{err}");
        // The state is intact and readable (no poison).
        assert!(!sim.is_poisoned());
        assert_eq!(sim.account(a).unwrap().unwrap().perp_balance, max);
        let err = sim
            .deal(
                &feed,
                &Deal {
                    address: a,
                    class: DealClass::Spot,
                    coin: Some("USDC".into()),
                    amount: max,
                },
            )
            .unwrap();
        assert_eq!(err, max);
        let err = sim
            .deal(
                &feed,
                &Deal {
                    address: a,
                    class: DealClass::Spot,
                    coin: Some("USDC".into()),
                    amount: Decimal::ONE,
                },
            )
            .unwrap_err();
        assert!(matches!(err, Error::Overflow { .. }), "{err}");
    }

    /// A spot drain may not take the holding below zero — the measured
    /// 2026-10-27 `2dbf72` artifact (42.646688 − 48.9 = −6.253312). The
    /// negative row made the indexer's `hold <= total` `CHECK` reject the
    /// tick and left the series with an unrecorded hole.
    #[test]
    fn a_spot_deal_cannot_overdraw_the_balance() {
        let feed = feed_with(&[("USDC", "1", "1")]);
        feed.put_spot_meta(crate::wire::SpotMeta {
            tokens: vec![crate::wire::SpotToken {
                name: "USDC".into(),
                sz_decimals: 8,
                wei_decimals: 8,
                index: 0,
                is_canonical: true,
                evm_contract: None,
                token_id: None,
                full_name: None,
                deployer_trading_fee_share: Decimal::ZERO,
            }],
            universe: vec![],
        });
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        let deal = |amount: &str| Deal {
            address: a,
            class: DealClass::Spot,
            coin: Some("USDC".into()),
            amount: amount.parse().unwrap(),
        };

        assert_eq!(sim.deal(&feed, &deal("10")).unwrap(), Decimal::from(10));
        let err = sim.deal(&feed, &deal("-11")).unwrap_err();
        assert!(matches!(err, Error::BadRequest { .. }), "{err}");
        assert_eq!(
            sim.account(a).unwrap().unwrap().spot.get(&0).unwrap().total,
            Decimal::from(10),
            "the refused drain must not move the balance"
        );
        assert_eq!(
            sim.deal(&feed, &deal("-10")).unwrap(),
            Decimal::ZERO,
            "an exact drain to zero is allowed"
        );
    }

    /// The arithmetic family's division half: a zero divisor is a named fault
    /// (`Decimal`'s `/` panics on it, the same measured hazard as the
    /// overflow above), and `ratio_or` is the domain's fallback reading.
    #[test]
    fn division_by_zero_is_named_and_ratios_fall_back() {
        let err = super::account::div(Decimal::ONE, Decimal::ZERO, "test ratio").unwrap_err();
        assert!(matches!(err, Error::DivideByZero { .. }), "{err}");
        assert_eq!(
            super::account::ratio_or(Decimal::ONE, Decimal::ZERO, Decimal::ZERO),
            Decimal::ZERO
        );
        assert_eq!(
            super::account::ratio_or(Decimal::from(6), Decimal::from(3), Decimal::ZERO),
            Decimal::from(2)
        );
    }

    /// The served `time` never lags wall, the sim clock (`advance`), or the
    /// venue chain's clock (their harness jumps every chain forward).
    #[test]
    fn served_time_never_lags() {
        let feed = MarketFeed::memory(vec![]);
        let sim = Sim::memory();
        assert_eq!(sim.served_time(1_000).unwrap(), 1_000);
        sim.advance(&feed, 60, 1).unwrap();
        assert_eq!(sim.served_time(1_000).unwrap(), 61_000, "advance wins");
        assert_eq!(
            sim.served_time(500_000).unwrap(),
            560_000,
            "the sim clock tracks wall + offset"
        );
        sim.observe_chain_time(700_000);
        assert_eq!(
            sim.served_time(500_000).unwrap(),
            700_000,
            "the venue chain wins when it is ahead"
        );
    }

    #[test]
    fn empty_seed_is_refused() {
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xbb);
        let seed: AccountSeed = serde_json::from_str("{}").unwrap();
        let err = sim.set_account(a, seed).unwrap_err();
        assert!(matches!(err, Error::BadRequest { .. }), "{err}");
        assert!(sim.account(a).unwrap().is_none());
    }

    /// `set_account` is a patch, not a replace: a perp-only seed keeps the
    /// account's spot balances (their `venue_mock` contract; measured
    /// divergence fixed 2026-10-23 — the deal-spot-then-seed sequence used to
    /// lose the deal).
    #[test]
    #[allow(clippy::too_many_lines)] // one transition table: splitting it would hide the rule
    fn set_account_is_a_patch_not_a_replace() {
        let feed = feed_with(&[("USDC", "1", "1")]);
        feed.put_spot_meta(crate::wire::SpotMeta {
            tokens: vec![crate::wire::SpotToken {
                name: "USDC".into(),
                sz_decimals: 8,
                wei_decimals: 8,
                index: 0,
                is_canonical: true,
                evm_contract: None,
                token_id: None,
                full_name: None,
                deployer_trading_fee_share: Decimal::ZERO,
            }],
            universe: vec![],
        });
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        sim.deal(
            &feed,
            &Deal {
                address: a,
                class: DealClass::Spot,
                coin: Some("USDC".into()),
                amount: Decimal::from(250),
            },
        )
        .unwrap();
        sim.set_account(
            a,
            serde_json::from_str(
                r#"{
                    "marginSummary": {"accountValue": "100", "totalNtlPos": "0", "totalRawUsd": "100", "totalMarginUsed": "0"}
                }"#,
            )
            .unwrap(),
        )
        .unwrap();
        let account = sim.account(a).unwrap().unwrap();
        assert_eq!(
            account.perp_balance,
            Decimal::from(100),
            "the named summary landed"
        );
        assert_eq!(
            account.spot.get(&0).map(|s| s.total),
            Some(Decimal::from(250)),
            "the unnamed spot balance survived the perp-only seed"
        );

        // A named balances list replaces the spot side (and a named empty one clears).
        sim.set_account(a, serde_json::from_str(r#"{"spotBalances": []}"#).unwrap())
            .unwrap();
        assert!(sim.account(a).unwrap().unwrap().spot.is_empty());
    }

    /// A control write is durable provenance: the served `controlOwned` stays
    /// true after the mark clears, and only an explicit fixture/account clear
    /// removes it. The measured 5195/4a ticks were served as ordinary market
    /// observations because the flag was derived from marks alone.
    #[test]
    fn a_control_write_marks_the_account_and_clears_explicitly() {
        let feed = feed_with(&[("USDC", "1", "1")]);
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xcc);
        sim.set_account(
            a,
            serde_json::from_str(
                r#"{
                    "marginSummary": {"accountValue": "0.5", "totalNtlPos": "0", "totalRawUsd": "0.5", "totalMarginUsed": "0"},
                    "spotBalances": [{"coin": "USDC", "token": 0, "hold": "0", "total": "0.5", "entryNtl": "0"}],
                    "userAbstraction": "unifiedAccount"
                }"#,
            )
            .unwrap(),
        )
        .unwrap();
        let account = sim.account(a).unwrap().unwrap();
        let served = super::account::clearinghouse(&account, &feed, None, 1).unwrap();
        assert!(served.control_owned, "a balance patch is a control write");
        assert_eq!(served.margin_summary.total_raw_usd, "0.5".parse().unwrap());

        assert!(sim.clear_control_fixture(a).unwrap());
        let account = sim.account(a).unwrap().unwrap();
        let served = super::account::clearinghouse(&account, &feed, None, 1).unwrap();
        assert!(
            !served.control_owned,
            "an explicit fixture clear restores market provenance"
        );
        assert_eq!(
            served.margin_summary.total_raw_usd,
            "0.5".parse().unwrap(),
            "balances survive the fixture clear"
        );

        assert!(sim.clear_account(a).unwrap());
        assert!(
            sim.account(a).unwrap().is_none(),
            "the destructive clear removes the account"
        );
        assert!(!sim.clear_account(a).unwrap(), "a second clear is a no-op");

        // A mode-only setup patch is not a balance write: it must not poison
        // the account's market provenance.
        sim.set_account(
            a,
            serde_json::from_str(r#"{"userAbstraction": "unifiedAccount"}"#).unwrap(),
        )
        .unwrap();
        let account = sim.account(a).unwrap().unwrap();
        let served = super::account::clearinghouse(&account, &feed, None, 1).unwrap();
        assert!(
            !served.control_owned,
            "a mode-only patch keeps market provenance"
        );
    }

    /// The waterfall must not turn a control-plane balance patch into a venue
    /// liquidation. The measured 2026-10-31 10:53 patch on `0x6807…` drove an
    /// active account below maintenance; the backstop took the position and the
    /// operator enforced it. The fixture is the provenance: skip the account
    /// while it is set, and let the ordinary waterfall act once it is cleared.
    #[test]
    fn the_waterfall_skips_a_control_patched_account() {
        let feed = feed_with(&[("HYPE", "100", "100")]);
        feed.put_meta(meta_with_table("HYPE", 10), 1);
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xdd);
        sim.set_account(
            a,
            serde_json::from_str(
                r#"{
                    "marginSummary": {"accountValue": "0.1", "totalNtlPos": "8", "totalRawUsd": "8.1", "totalMarginUsed": "0.8"},
                    "assetPositions": [{
                        "type": "oneWay",
                        "position": {
                            "coin": "HYPE", "entryPx": "100", "leverage": {"type": "cross", "value": 10},
                            "marginUsed": "0.8", "maxLeverage": 10, "positionValue": "8",
                            "returnOnEquity": "0", "szi": "-0.08", "unrealizedPnl": "0",
                            "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
                        }
                    }],
                    "spotBalances": [{"coin": "USDC", "token": 0, "hold": "0", "total": "0.1", "entryNtl": "0"}],
                    "userAbstraction": "unifiedAccount"
                }"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            sim.check_all(&feed, 1_000).unwrap().is_empty(),
            "a control-patched account is a scenario input, not a liquidation candidate"
        );
        sim.clear_control_fixture(a).unwrap();
        assert!(
            !sim.check_all(&feed, 1_000).unwrap().is_empty(),
            "once the fixture is cleared, the ordinary waterfall acts on the same book"
        );
    }

    #[test]
    fn reset_blanks_and_bumps_the_epoch() {
        let feed = feed_with(&[("USDC", "1", "1")]);
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        sim.deal(
            &feed,
            &Deal {
                address: a,
                class: DealClass::Perp,
                coin: None,
                amount: Decimal::ONE,
            },
        )
        .unwrap();
        assert_eq!(sim.epoch().unwrap(), 0);
        let epoch = sim.reset().unwrap();
        assert_eq!(epoch, 1);
        assert!(sim.account(a).unwrap().is_none());
    }

    #[test]
    fn advance_jumps_the_clock_and_accrues() {
        let feed = feed_with_funding(&[("BTC", "100", "100")]);
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        sim.set_account(
            a,
            serde_json::from_str(
                r#"{
                    "marginSummary": {"accountValue": "100", "totalNtlPos": "100", "totalRawUsd": "0", "totalMarginUsed": "10"},
                    "assetPositions": [{
                        "type": "oneWay",
                        "position": {
                            "coin": "BTC", "entryPx": "100", "leverage": {"type": "cross", "value": 10},
                            "marginUsed": "10", "maxLeverage": 20, "positionValue": "100",
                            "returnOnEquity": "0.0", "szi": "1.0", "unrealizedPnl": "0.0",
                            "cumFunding": {"allTime": "0.0", "sinceOpen": "0.0", "sinceChange": "0.0"}
                        }
                    }]
                }"#,
            )
            .unwrap(),
        )
        .unwrap();
        let before = sim.account(a).unwrap().unwrap().perp_balance;
        let outcome = sim.advance(&feed, 3600, 1).unwrap();
        assert_eq!(outcome.advanced_ms, 3_600_000);
        let after = sim.account(a).unwrap().unwrap().perp_balance;
        // One hour of funding at rate 1.25e-5 on oracle 100, long 1.0: paid.
        assert_eq!(before - after, Decimal::new(125, 5));
    }

    /// Portfolio margin is refused by name (CoreWriter action 16's value 3
    /// and the control patch alike): its cross-asset portfolio is the venue's
    /// own accounting; `unifiedAccount` is the modeled single balance.
    #[test]
    fn portfolio_margin_is_refused_by_name() {
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        let refused = sim.set_mode(a, crate::wire::UserAbstraction::PortfolioMargin);
        assert!(refused.is_err(), "portfolio margin is not modeled");
        assert!(refused
            .unwrap_err()
            .to_string()
            .contains("portfolio margin"));
        assert_eq!(
            sim.account(a).unwrap().map(|acc| acc.mode),
            None,
            "nothing entered the state"
        );
        sim.set_mode(a, crate::wire::UserAbstraction::UnifiedAccount)
            .unwrap();
        assert_eq!(
            sim.account(a).unwrap().unwrap().mode,
            crate::wire::UserAbstraction::UnifiedAccount
        );
    }

    /// The state round trip: what a save writes is what a load serves — the
    /// durability the per-block writer and the restart legs rely on.
    #[test]
    fn the_state_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("venue-sim-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sim.json");
        let a = Address::repeat_byte(0xaa);
        let sim = Sim::load(path.clone(), 1024).unwrap();
        sim.credit_spot(
            a,
            TokenRef {
                index: 0,
                coin: "USDC",
            },
            Decimal::from(250),
        )
        .unwrap();
        sim.save().unwrap();
        let reloaded = Sim::load(path, 1024).unwrap();
        let total = reloaded
            .account(a)
            .unwrap()
            .unwrap()
            .spot
            .get(&0)
            .map(|e| e.total);
        assert_eq!(total, Some(Decimal::from(250)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two tails are distinct: `mutate_account` batches (`touch` leaves
    /// the dirty flag for the block writer), `mutate_state` saves
    /// (`mark_dirty` clears it).
    #[test]
    fn the_durable_writer_saves_and_the_batched_one_defers() {
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        sim.note_agent(a, Address::repeat_byte(0xbb), "bot")
            .unwrap();
        assert_eq!(sim.save_if_dirty().unwrap(), Flushed::Written); // the batched tail left it dirty
        let now = crate::now_ms();
        sim.use_nonce(a, now, now).unwrap();
        assert_eq!(sim.save_if_dirty().unwrap(), Flushed::Clean); // the durable tail already saved
    }

    /// The account verbs' shared tail: a plain mutation marks the sim dirty
    /// (the bridge's per-block save is driven by this flag).
    #[test]
    fn the_account_verbs_mark_the_sim_dirty() {
        let sim = Sim::memory();
        assert_eq!(sim.save_if_dirty().unwrap(), Flushed::Clean);
        sim.note_agent(
            Address::repeat_byte(0xaa),
            Address::repeat_byte(0xbb),
            "bot",
        )
        .unwrap();
        assert_eq!(sim.save_if_dirty().unwrap(), Flushed::Written);
    }
}
