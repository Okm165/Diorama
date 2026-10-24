//! The account plane: the simulated accounts registry (COW overlay), its
//! persistence, and the self-advance automaton (plan §5, §14.8).
//!
//! Untouched accounts are not the sim's business — the API layer passes those
//! reads through to the real venue. A *touched* account (dealt funds, seeded,
//! or otherwise registered) is simulated: its state is this registry's, its
//! figures are computed from live marks, and it persists to `sim.json` so a
//! restart does not lose a scenario.
//!
//! Time: the sim has its own clock (`wall + offset`, offset = 0 until
//! `advance` scripts a jump). Funding accrues on that clock and liquidations
//! are checked against current marks — the §14.8 evaluation runs on every
//! account-scoped read and on the 1 Hz tick — so daloc's set-and-read tests
//! see live funding/liquidation behavior without ever calling `advance`. The
//! wire's `time` field stays *wall* now: it is the read's freshness, which the
//! client's ≤6 s staleness gate compares against its own clock.
pub mod account;
pub mod order;

use crate::error::{Error, Result};
use crate::feed::MarketFeed;
use account::{Account, AccountSeed, Accounts, Liquidation};
use alloy_primitives::Address;
use mirror_devnet::store;
use mirror_devnet::Shared;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
    /// The previous tick's mark per coin (ADL closes at the previous mark).
    #[serde(default)]
    pub prev_marks: std::collections::BTreeMap<String, Decimal>,
}

/// The sim's metrics (rendered beside the feed's).
#[derive(Default)]
pub(crate) struct SimMetrics {
    /// Registered (touched) accounts.
    pub accounts: AtomicU64,
    /// The scenario epoch.
    pub epoch: AtomicU64,
    /// Liquidations the sim executed (v1: full closes at the mark).
    pub liquidations_total: AtomicU64,
    /// Funding payments applied (per position, per accrual).
    pub funding_payments_total: AtomicU64,
    /// Served states that failed the identity re-check (the client's own
    /// gate): any nonzero value is a construction regression to page on.
    pub identity_violations: AtomicU64,
    /// The scenario's explicit `advance` steps (the 1 Hz tick is the
    /// automaton; its work shows in the funding/liquidation counters).
    pub advances_total: AtomicU64,
    /// Orders the matching lane filled (the impact lane's liveness).
    pub orders_filled_total: AtomicU64,
    /// Trigger orders armed / fired by the tick.
    pub triggers_armed_total: AtomicU64,
    pub triggers_fired_total: AtomicU64,
    /// Fills that charged a fee (the fee lane's liveness).
    pub fees_charged_total: AtomicU64,
    /// The waterfall's steps: partial book closes, full book closes, backstop
    /// takeovers and ADL closes.
    pub partial_liquidations_total: AtomicU64,
    pub backstop_liquidations_total: AtomicU64,
    pub adl_closes_total: AtomicU64,
}

impl SimMetrics {
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512);
        mirror_devnet::metrics::push_metric(&mut out, "gauge", "venue_sim_accounts", "simulated (touched) accounts", self.accounts.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "gauge", "venue_sim_epoch", "scenario epoch (reset bumps it)", self.epoch.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_liquidations_total", "liquidations executed by the sim", self.liquidations_total.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_partial_liquidations_total", "waterfall step 1: partial (20%) book liquidations", self.partial_liquidations_total.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_backstop_liquidations_total", "waterfall step 2: backstop (the vault took the position)", self.backstop_liquidations_total.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_adl_closes_total", "waterfall step 3: ADL closes at the previous mark", self.adl_closes_total.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_funding_payments_total", "funding payments applied", self.funding_payments_total.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_identity_violations_total", "served states that failed the identity re-check (must stay 0)", self.identity_violations.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_advances_total", "scenario advance() steps", self.advances_total.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_orders_filled_total", "orders filled against the real book (the impact lane)", self.orders_filled_total.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_fees_charged_total", "fills that charged a fee (taker and maker sides)", self.fees_charged_total.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_triggers_armed_total", "trigger orders armed (tp/sl)", self.triggers_armed_total.load(Ordering::Relaxed));
        out
    }
}

/// One `deal`: mint funds into the account (perp wallet or spot token).
/// Absolute-additive: it raises the balance by `amount` (the harness's
/// `add_erc20` semantics; draining is `amount` negative or a `set_account`
/// seed). The control route's request body is this type — one shape for the
/// wire and the call.
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

/// The liquidation attempt's size (docs): above the $100k partial threshold
/// only 20 % goes per attempt; during the 30 s cooldown after a partial, the
/// full size. Returns (size, was_partial).
fn attempt_size(full: Decimal, notional: Decimal, in_cooldown: bool) -> Result<(Decimal, bool)> {
    const PARTIAL_THRESHOLD: Decimal = Decimal::from_parts(100_000, 0, 0, false, 0);
    if notional > PARTIAL_THRESHOLD && !in_cooldown {
        return Ok((account::mul(full, Decimal::new(2, 1), "partial size")?, true));
    }
    Ok((full, false))
}

/// The liquidator vault (docs' HLP address): the backstop that takes
/// positions over below ⅔ of the maintenance requirement.
pub const BACKSTOP: Address = Address::new([
    0xdf, 0xc2, 0x4b, 0x07, 0x7b, 0xc1, 0x42, 0x5a, 0xd1, 0xde, 0xa7, 0x5b, 0xcb, 0x6f, 0x81, 0x58, 0xe1, 0x0d,
    0xf3, 0x03,
]);

pub struct Sim {
    state: Shared<SimState>,
    path: Option<PathBuf>,
    dirty: AtomicBool,
    /// The venue chain's head timestamp (ms), observed by the bridge when V is
    /// configured. Their time policy jumps *every* chain's clock forward and
    /// the client's own rule is `chain_time = max(wall, head)` — a served
    /// `time` that stayed at wall would read as stale after a V jump.
    chain_time_ms: AtomicU64,
    pub(crate) metrics: SimMetrics,
}

impl Sim {
    /// Load the registry from its state file (absent = first boot, empty).
    pub fn load(path: PathBuf) -> Result<Self> {
        let state = store::load::<SimState>(&path, "sim state")?.unwrap_or_default();
        Ok(Self::from_state(state, Some(path)))
    }

    /// An in-memory registry (tests only: no production path builds one).
    #[cfg(test)]
    pub fn memory() -> Self {
        Self::from_state(SimState::default(), None)
    }

    fn from_state(state: SimState, path: Option<PathBuf>) -> Self {
        let sim = Self {
            state: Shared::new("sim state", state),
            path,
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
        Ok(self.state.lock()?.accounts.get(&address).cloned())
    }

    /// The sim clock (ms) at wall time `now_ms`.
    pub fn clock(&self, now_ms: u64) -> Result<u64> {
        let offset = self.state.lock()?.clock_offset_ms;
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

    /// The scenario epoch counter (tests: `reset` returns it to callers).
    #[cfg(test)]
    pub fn epoch(&self) -> Result<u64> {
        Ok(self.state.lock()?.epoch)
    }

    /// The state lock is poisoned (a panic happened mid-update): every
    /// account path refuses until a restart — health/readiness must say so.
    pub fn is_poisoned(&self) -> bool {
        self.state.lock().is_err()
    }

    /// The bridge's scan cursor (0 = not anchored).
    pub fn bridge_cursor(&self) -> Result<u64> {
        Ok(self.state.lock()?.bridge_cursor)
    }

    /// Move the bridge's cursor (persisted with the next save).
    pub fn set_bridge_cursor(&self, block: u64) -> Result<()> {
        self.state.lock()?.bridge_cursor = block;
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
                    entry.total = account::add(entry.total, deal.amount, "spot deal")?;
                    Ok(entry.total)
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
            return Err(Error::bad_request("the patch is empty (no field named)",));
        }
        let now = crate::now_ms();
        let clock = self.clock(now)?;
        self.with_account(address, |account| {
            seed.apply_to(account, clock)?;
            // Remember the patch as the control fixture, merged over the
            // previous one (hl_mock's rule: a named field replaces, an
            // unnamed one keeps).
            let merged = seed.merged_with(account.control.take());
            account.control = Some(merged);
            Ok(())
        })?;
        self.mark_dirty()
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
    pub(crate) fn credit_spot(&self, address: Address, token: TokenRef<'_>, amount: Decimal) -> Result<()> {
        // Read the clock *before* taking the state lock: `clock()` locks the
        // same mutex, and a std Mutex is not reentrant.
        let clock = self.clock(crate::now_ms())?;
        self.with_account(address, |account| {
            stamp_funding_baseline(account, clock);
            let entry = account::spot_entry(account, token);
            entry.total = account::add(entry.total, amount, "spot credit")?;
            Ok(())
        })?;
        self.touch();
        Ok(())
    }

    /// Core→EVM / spot send: debit the spot balance; `None` when the account
    /// or the balance is short (the bridge counts the refusal — a system
    /// transfer never goes negative).
    pub fn try_debit_spot(&self, address: Address, token: u32, amount: Decimal) -> Result<Option<Decimal>> {
        let mut state = self.state.lock()?;
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
        let after = entry.total;
        drop(state);
        self.touch();
        Ok(Some(after))
    }

    /// USD class transfer (CoreWriter action 7): spot USDC ↔ perp wallet.
    /// `false` when the source side is short.
    pub(crate) fn usd_class_transfer(&self, address: Address, token: TokenRef<'_>, amount: Decimal, to_perp: bool) -> Result<bool> {
        let clock = self.clock(crate::now_ms())?;
        let applied = self.with_account(address, |account| {
            stamp_funding_baseline(account, clock);
            if to_perp {
                let entry = account::spot_entry(account, token);
                if entry.total < amount {
                    return Ok(false);
                }
                entry.total = account::sub(entry.total, amount, "usd class transfer")?;
                account.perp_balance =
                    account::add(account.perp_balance, amount, "usd class transfer")?;
            } else {
                if account.perp_balance < amount {
                    return Ok(false);
                }
                account.perp_balance =
                    account::sub(account.perp_balance, amount, "usd class transfer")?;
                let entry = account::spot_entry(account, token);
                entry.total = account::add(entry.total, amount, "usd class transfer")?;
            }
            Ok(true)
        })?;
        if applied {
            self.touch();
        }
        Ok(applied)
    }

    /// Core spot send: debit the sender, credit the recipient. `false` when
    /// the sender is short.
    pub(crate) fn spot_send(&self, from: Address, to: Address, token: TokenRef<'_>, amount: Decimal) -> Result<bool> {
        {
            let mut state = self.state.lock()?;
            let Some(sender) = state.accounts.get_mut(&from) else {
                return Ok(false);
            };
            let Some(entry) = sender.spot.get_mut(&token.index) else {
                return Ok(false);
            };
            if entry.total < amount {
                return Ok(false);
            }
            entry.total = account::sub(entry.total, amount, "spot send")?;
        }
        self.credit_spot(to, token, amount)?;
        Ok(true)
    }

    /// Record an API wallet (CoreWriter action 9).
    pub fn note_agent(&self, address: Address, wallet: Address, name: &str) -> Result<()> {
        self.with_account(address, |account| {
            if account.agents.iter().all(|a| a.wallet != wallet) {
                account.agents.push(account::AgentWallet {
                    wallet,
                    name: name.to_string(),
                });
            }
            Ok(())
        })?;
        self.touch();
        Ok(())
    }

    /// Set the account's abstraction mode (CoreWriter action 16).
    pub(crate) fn set_mode(&self, address: Address, mode: crate::wire::UserAbstraction) -> Result<()> {
        self.with_account(address, |account| {
            account.mode = mode;
            Ok(())
        })?;
        self.touch();
        Ok(())
    }

    /// `reset`: blank the scenario (accounts + clock offset), bump the epoch.
    pub fn reset(&self) -> Result<u64> {
        let epoch = {
            let mut state = self.state.lock()?;
            state.accounts.clear();
            state.clock_offset_ms = 0;
            state.epoch += 1;
            state.epoch
        };
        self.save()?;
        Ok(epoch)
    }

    /// `advance`: script a clock jump (`seconds`) with `blocks` liquidation
    /// checks inside it — the deterministic complement to self-advance.
    pub(crate) fn advance(&self, feed: &MarketFeed, seconds: u64, blocks: u64) -> Result<AdvanceOutcome> {
        let jump_ms = seconds.saturating_mul(1_000);
        {
            let mut state = self.state.lock()?;
            state.clock_offset_ms = state.clock_offset_ms.saturating_add(jump_ms as i64);
        }
        let now = crate::now_ms();
        let mut liquidations = Vec::new();
        let blocks = blocks.max(1);
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

    /// One tick of the self-advance loop: accrue funding to the sim clock and
    /// run the liquidation checks; persist when something moved. A warming
    /// market plane (no meta yet) is a no-op, not a fault storm.
    /// The §14.8 evaluation, one implementation for the 1 Hz tick and every
    /// account-scoped read: accrue funding to now, check every account against
    /// its maintenance requirement (closing the ones below it at the mark),
    /// and persist if anything moved — so a set-and-read test sees funding and
    /// liquidation behavior without ever calling `advance`.
    pub fn evaluate(&self, feed: &MarketFeed) -> Result<Vec<Liquidation>> {
        if feed.meta().is_none() {
            return Ok(Vec::new());
        }
        let now = crate::now_ms();
        self.accrue_all(feed, now)?;
        self.run_resting(feed, now)?;
        let liquidated = self.check_all(feed, now)?;
        self.record_marks(feed)?;
        self.save_if_dirty()?;
        Ok(liquidated)
    }

    /// The resting-book pass (the 1 Hz automaton): scheduled cancels fire,
    /// armed triggers whose mark crossed execute, and resting orders the live
    /// book has crossed fill at their own price (they are the maker; the live
    /// levels they take are consumed — the impact lane).
    fn run_resting(&self, feed: &MarketFeed, now_ms: u64) -> Result<()> {
        self.fire_scheduled_cancels(now_ms)?;
        self.fire_triggers(feed)?;
        self.passive_fills(feed, now_ms)
    }

    /// (a) `scheduleCancel`: the dead-man's switch cancels everything due.
    fn fire_scheduled_cancels(&self, now_ms: u64) -> Result<()> {
        let due: Vec<Address> = {
            let st = self.state.lock()?;
            st.scheduled_cancels
                .iter()
                .filter(|(_, t)| **t <= now_ms)
                .map(|(a, _)| *a)
                .collect()
        };
        for account in due {
            let mine = self.resting_orders(Some(account))?;
            let n = mine.len();
            for r in mine {
                let _ = self.cancel(account, &r.coin, r.oid);
            }
            {
                let mut st = self.state.lock()?;
                st.scheduled_cancels.remove(&account);
            }
            tracing::info!(account = %account, canceled = n, "scheduled cancel fired (dead-man's switch)");
        }
        Ok(())
    }

    /// (b) Triggers: fire when the mark crosses the trigger price; the
    /// trigger leaves the book first (a failed execution cancels it, as the
    /// venue's does — never a silent re-arm).
    fn fire_triggers(&self, feed: &MarketFeed) -> Result<()> {
        for r in self.resting_orders(None)? {
            let Some(t) = r.trigger else { continue };
            let Some((mark, _)) = feed.required_prices(&r.coin) else {
                continue;
            };
            let fires = match (r.is_buy, t.tpsl) {
                (false, crate::sim::order::Tpsl::Tp) => mark >= t.trigger_px,
                (false, crate::sim::order::Tpsl::Sl) => mark <= t.trigger_px,
                (true, crate::sim::order::Tpsl::Tp) => mark <= t.trigger_px,
                (true, crate::sim::order::Tpsl::Sl) => mark >= t.trigger_px,
            };
            if !fires {
                continue;
            }
            let _ = self.cancel(r.account, &r.coin, r.oid);
            let order = crate::sim::order::Order {
                coin: r.coin.clone(),
                is_buy: r.is_buy,
                size: r.sz,
                limit_px: if t.is_market { None } else { Some(r.px) },
                tif: crate::sim::order::Tif::Ioc,
                reduce_only: r.reduce_only,
                cloid: r.cloid,
                trigger: None,
            };
            match self.place(feed, r.account, &order) {
                Ok(placed) => {
                    self.metrics.triggers_fired_total.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(account = %r.account, coin = %r.coin, oid = r.oid, filled = placed.fill.as_ref().map(|f| f.total_sz.to_string()).unwrap_or_default(), "trigger fired");
                }
                Err(e) => {
                    tracing::warn!(account = %r.account, coin = %r.coin, oid = r.oid, error = %e, "trigger execution refused; the trigger is canceled");
                }
            }
        }
        Ok(())
    }

    /// (c) Passive fills: a resting order the live book has crossed fills at
    /// its own price (the maker side), consuming the crossed live levels.
    fn passive_fills(&self, feed: &MarketFeed, now_ms: u64) -> Result<()> {
        for r in self.resting_orders(None)? {
            if r.trigger.is_some() {
                continue;
            }
            let Some(book) = feed.book(&r.coin, now_ms) else {
                continue;
            };
            let opposite = usize::from(r.is_buy);
            let Some(best) = book.levels[opposite].first() else {
                continue;
            };
            let crossed = if r.is_buy { best.px <= r.px } else { best.px >= r.px };
            if !crossed {
                continue;
            }
            let take = r.sz.min(best.sz);
            if take <= Decimal::ZERO {
                continue;
            }
            // The same re-margin rule as the taker path: a maker that cannot
            // carry the fill is canceled, not filled.
            if let Err(e) = self.remargin_one(feed, &r) {
                tracing::warn!(account = %r.account, oid = r.oid, error = %e, "passive re-margin check failed");
                continue;
            }
            if let Err(e) = self.passive_fill(&r, take, best.px, feed) {
                tracing::warn!(account = %r.account, coin = %r.coin, oid = r.oid, error = %e, "passive fill refused");
            }
        }
        Ok(())
    }

    /// The passive path's re-margin check: probe the maker's post-fill
    /// account; on breach, cancel the resting order (the venue's
    /// `marginCanceled`) and report it so the caller skips the fill.
    fn remargin_one(&self, feed: &MarketFeed, r: &crate::sim::order::RestingOrder) -> Result<()> {
        if self.remargin_ok(feed, r)? {
            return Ok(());
        }
        tracing::warn!(account = %r.account, oid = r.oid, "resting order canceled at match time (marginCanceled)");
        self.margin_cancel(r.account, &r.coin, r.oid)
    }

    /// One passive fill: the maker's account update (their price, the maker
    /// fee) plus the live level's consumption and the resting size's reduce.
    fn passive_fill(
        &self,
        r: &crate::sim::order::RestingOrder,
        take: Decimal,
        level_px: Decimal,
        feed: &MarketFeed,
    ) -> Result<()> {
        let leverage = self.leverage_for(r.account, &r.coin, feed.meta().as_ref())?;
        let notional = crate::sim::account::mul(take, r.px, "maker notional")?;
        let facts = crate::sim::account::FillFacts {
            coin: r.coin.clone(),
            is_buy: r.is_buy,
            size: take,
            px: r.px,
            leverage,
            reduce_only: r.reduce_only,
            fee: crate::sim::account::mul(notional, crate::sim::order::maker_fee(), "maker fee")?,
        };
        let clock = self.clock(crate::now_ms())?;
        self.with_account(r.account, |account| {
            crate::sim::account::accrue_funding(account, feed, clock)?;
            crate::sim::account::apply_fill(account, &facts)
        })?;
        feed.consume(crate::feed::Consumption {
            coin: r.coin.clone(),
            side: usize::from(r.is_buy) as u8,
            price: level_px,
            size: take,
        })?;
        self.fill_resting_pub(r.oid, take, r.px)?;
        tracing::info!(account = %r.account, coin = %r.coin, oid = r.oid, sz = %take, px = %r.px, "resting order filled passively");
        Ok(())
    }

    /// `updateIsolatedMargin` (docs): move USDC between the perp balance and
    /// the position's isolated margin (`ntli` is 6-decimal, signed).
    pub(crate) fn update_isolated_margin(&self, address: Address, coin: &str, ntli: i64) -> Result<()> {
        let delta = Decimal::from(ntli) / Decimal::from(1_000_000);
        self.with_account(address, |account| {
            let Some(p) = account.positions.get_mut(coin) else {
                return Err(crate::error::Error::bad_request(format!(
                    "no {coin} position to margin (the docs' `updateIsolatedMargin` targets a position)"
                )));
            };
            let next = account::add(p.isolated_margin, delta, "isolated margin")?;
            if next < Decimal::ZERO {
                return Err(crate::error::Error::bad_request(
                    "isolated margin cannot go below zero",
                ));
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

    /// The venue's nonce rule (plan §5): the 100 highest nonces per signer,
    /// accepted in the window (T − 2 d, T + 1 d); a duplicate is refused by
    /// name. Called only when the body carries a nonce (their harness's
    /// nonce-less shape stays accepted — the devnet's impersonation analogue).
    pub(crate) fn use_nonce(&self, signer: Address, nonce: u64, now_ms: u64) -> Result<()> {
        const TWO_DAYS_MS: u64 = 2 * 24 * 60 * 60 * 1_000;
        const ONE_DAY_MS: u64 = 24 * 60 * 60 * 1_000;
        let mut st = self.state.lock()?;
        let set = st.nonces.entry(signer).or_default();
        if set.contains(&nonce) {
            return Err(crate::error::Error::bad_request("Nonce already used."));
        }
        if nonce.saturating_add(TWO_DAYS_MS) < now_ms || nonce > now_ms.saturating_add(ONE_DAY_MS) {
            return Err(crate::error::Error::bad_request(
                "Nonce is outside the accepted window.",
            ));
        }
        set.insert(nonce);
        while set.len() > 100 {
            set.pop_first();
        }
        drop(st);
        self.mark_dirty()?;
        Ok(())
    }

    /// Accrue funding for every account (the caller holds no lock).
    pub fn accrue_all(&self, feed: &MarketFeed, now_ms: u64) -> Result<()> {
        let clock = self.clock(now_ms)?;
        let mut state = self.state.lock()?;
        let mut payments = 0u64;
        for account in state.accounts.values_mut() {
            payments += account::accrue_funding(account, feed, clock)? as u64;
        }
        if payments > 0 {
            self.metrics.funding_payments_total.fetch_add(payments, Ordering::Relaxed);
            self.dirty.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// The venue's liquidation waterfall (docs): an account below its
    /// maintenance requirement is first closed by market orders to the book
    /// (20 % per attempt above $100k, the full size during the 30 s cooldown
    /// that follows a partial); below ⅔ of the maintenance requirement the
    /// backstop (the liquidator vault) takes the positions and margin over
    /// (the maintenance is not returned); a negative account value goes to
    /// ADL — the opposite side, ranked by `(mark/entry)·(notional/account_value)`
    /// at the previous mark, is closed against the underwater account.
    pub fn check_all(&self, feed: &MarketFeed, now_ms: u64) -> Result<Vec<Liquidation>> {
        // The maintenance is tiered from the live meta; with none (the feed
        // still warming) the requirement reads zero and no liquidation fires —
        // the pass fails open, never liquidating on an unknown requirement.
        let accounts = self.account_snapshot()?;
        let mut out = Vec::new();
        for (address, account) in accounts {
            if let Some(record) = self.check_account(feed, address, &account, now_ms)? {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// The accounts snapshot (the decision loop mutates state, so it iterates
    /// a copy).
    fn account_snapshot(&self) -> Result<Vec<(Address, Account)>> {
        let st = self.state.lock()?;
        Ok(st
            .accounts
            .iter()
            .map(|(a, acc)| (*a, acc.clone()))
            .collect())
    }

    /// One account's waterfall decision: solvent → nothing; underwater → ADL;
    /// below the backstop ratio → the vault; otherwise the book.
    fn check_account(
        &self,
        feed: &MarketFeed,
        address: Address,
        account: &Account,
        now_ms: u64,
    ) -> Result<Option<Liquidation>> {
        if account.positions.is_empty() {
            return Ok(None);
        }
        let meta = feed.meta();
        let v = account::view(account, feed, meta.as_ref())?;
        let maintenance = account::add(
            v.cross_maintenance_margin_used,
            account::isolated_maintenance(account, &v)?,
            "maintenance total",
        )?;
        let equity = v.margin_summary.account_value;
        if equity >= maintenance {
            return Ok(None);
        }
        match rescue_step(equity, maintenance)? {
            Rescue::Adl => self.adl(feed, address, account, meta.as_ref()),
            Rescue::Backstop => self.backstop(feed, address, account, meta.as_ref()),
            Rescue::Book => self.liquidate_via_book(feed, address, account, now_ms),
        }
    }

    /// Step 1: market orders to the book. Above the $100k partial threshold
    /// only 20 % goes per attempt; after any partial, a 30 s cooldown makes
    /// every attempt the full size (docs). Partial fills by liquidity are
    /// expected — the remainder stays with the account.
    fn liquidate_via_book(
        &self,
        feed: &MarketFeed,
        address: Address,
        account: &Account,
        now_ms: u64,
    ) -> Result<Option<Liquidation>> {
        const COOLDOWN_MS: u64 = 30_000;
        let cooldown = self
            .state
            .lock()?
            .liq_cooldown_until
            .get(&address)
            .copied()
            .unwrap_or(0);
        let in_cooldown = now_ms < cooldown;
        let mut closed = Vec::new();
        let mut partial = false;
        for (coin, p) in &account.positions {
            let Some(mark) = feed.mark(coin) else { continue };
            let notional = account::mul(p.szi.abs(), mark, "liquidation notional")?;
            let (size, was_partial) = attempt_size(p.szi.abs(), notional, in_cooldown)?;
            partial |= was_partial;
            if size.is_zero() {
                continue;
            }
            match self.liquidate_position(feed, address, coin, size) {
                Ok(fill) => {
                    tracing::warn!(account = %address, coin = %coin, sz = %fill.total_sz, avg_px = %fill.avg_px, partial = was_partial, "liquidation: market close");
                    closed.push(coin.clone());
                }
                Err(e) => {
                    tracing::warn!(account = %address, coin = %coin, error = %e, "liquidation close refused (no liquidity?)");
                }
            }
        }
        if partial {
            let until = now_ms.saturating_add(COOLDOWN_MS);
            self.state.lock()?.liq_cooldown_until.insert(address, until);
            self.metrics.partial_liquidations_total.fetch_add(1, Ordering::Relaxed);
        }
        if closed.is_empty() {
            return Ok(None);
        }
        self.metrics.liquidations_total.fetch_add(1, Ordering::Relaxed);
        self.mark_dirty()?;
        Ok(Some(Liquidation {
            positions_closed: closed,
            account_value: Decimal::ZERO,
            maintenance: Decimal::ZERO,
        }))
    }

    /// Step 2: the backstop (the liquidator vault) takes the positions and
    /// margin over — the maintenance is not returned, so the trader ends at
    /// zero equity (docs). The vault's entry is the mark.
    fn backstop(
        &self,
        feed: &MarketFeed,
        address: Address,
        account: &Account,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<Option<Liquidation>> {
        let v = account::view(account, feed, meta)?;
        let closed = self.vault_take(feed, address)?;
        self.metrics.backstop_liquidations_total.fetch_add(1, Ordering::Relaxed);
        self.metrics.liquidations_total.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(account = %address, closed = closed.len(), "backstop liquidation: the vault took the positions");
        self.mark_dirty()?;
        Ok(Some(Liquidation {
            positions_closed: closed,
            account_value: v.margin_summary.account_value,
            maintenance: Decimal::ZERO,
        }))
    }

    /// The vault takes every position over at the mark (same side, entered
    /// there — its own book starts flat) and the account's margin with it; the
    /// trader ends flat. Returns the coins taken; a refused transfer stays
    /// with the account and is named.
    fn vault_take(&self, feed: &MarketFeed, address: Address) -> Result<Vec<String>> {
        let mut closed = Vec::new();
        let mut st = self.state.lock()?;
        let mut me = st.accounts.remove(&address).unwrap_or_default();
        let mut vault = st.accounts.remove(&BACKSTOP).unwrap_or_default();
        let coins: Vec<String> = me.positions.keys().cloned().collect();
        for coin in coins {
            let Some(p) = me.positions.remove(&coin) else {
                continue;
            };
            let mark = feed.mark(&coin).unwrap_or(p.entry_px);
            let facts = account::FillFacts {
                coin: coin.clone(),
                is_buy: p.is_long(),
                size: p.szi.abs(),
                px: mark,
                leverage: p.leverage_value,
                reduce_only: false,
                fee: Decimal::ZERO,
            };
            if let Err(e) = account::apply_fill(&mut vault, &facts) {
                tracing::warn!(account = %address, coin = %coin, error = %e, "backstop transfer refused");
                continue;
            }
            closed.push(coin);
        }
        vault.perp_balance = account::add(vault.perp_balance, me.perp_balance, "backstop balance")?;
        me.perp_balance = Decimal::ZERO;
        st.accounts.insert(address, me);
        st.accounts.insert(BACKSTOP, vault);
        Ok(closed)
    }

    /// Step 3: ADL — the platform must stay solvent. The opposite side is
    /// ranked by `(mark/entry)·(notional/account_value)` and closed at the
    /// *previous* mark against the underwater account (docs).
    fn adl(
        &self,
        feed: &MarketFeed,
        address: Address,
        account: &Account,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<Option<Liquidation>> {
        let mut closed = Vec::new();
        for (coin, p) in &account.positions {
            let Some(prev_mark) = self.state.lock()?.prev_marks.get(coin).copied() else {
                tracing::warn!(account = %address, coin = %coin, "ADL skipped: no previous mark recorded yet");
                continue;
            };
            let remaining = p.szi.abs();
            let my_side_long = p.is_long();
            let mut ranked = self.adl_ranking(feed, address, (coin, prev_mark, my_side_long), meta)?;
            ranked.sort_by_key(|r| std::cmp::Reverse(r.0));
            let mut left = remaining;
            for (_, other, size, _entry) in ranked {
                if left <= Decimal::ZERO {
                    break;
                }
                let take = left.min(size);
                // The counterparty closes at the previous mark; the underwater
                // account's side closes against it at the same price.
                self.with_account(address, |a| {
                    account::apply_fill(
                        a,
                        &account::FillFacts::close(coin, !my_side_long, take, prev_mark, p.leverage_value),
                    )
                })?;
                self.with_account(other, |a| {
                    account::apply_fill(
                        a,
                        &account::FillFacts::close(coin, my_side_long, take, prev_mark, 1),
                    )
                })?;
                self.metrics.adl_closes_total.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(account = %address, other = %other, coin = %coin, sz = %take, px = %prev_mark, "ADL close at the previous mark");
                left = left.saturating_sub(take);
            }
            closed.push(coin.clone());
        }
        self.metrics.liquidations_total.fetch_add(1, Ordering::Relaxed);
        self.mark_dirty()?;
        Ok(Some(Liquidation {
            positions_closed: closed,
            account_value: Decimal::ZERO,
            maintenance: Decimal::ZERO,
        }))
    }

    /// The ADL ranking: the opposite side, `(mark/entry)·(notional/account_value)`
    /// at the previous mark, descending (docs).
    fn adl_ranking(
        &self,
        feed: &MarketFeed,
        address: Address,
        target: (&str, Decimal, bool), // (coin, previous mark, the underwater side)
        meta: Option<&crate::wire::Meta>,
    ) -> Result<Vec<(Decimal, Address, Decimal, Decimal)>> {
        let (coin, prev_mark, my_side_long) = target;
        let mut ranked: Vec<(Decimal, Address, Decimal, Decimal)> = Vec::new();
        let st = self.state.lock()?;
        for (other, oa) in st.accounts.iter() {
            if *other == address {
                continue;
            }
            let Some(op) = oa.positions.get(coin) else { continue };
            if op.is_long() == my_side_long || op.szi.is_zero() {
                continue;
            }
            let ov = account::view(oa, feed, meta)?;
            if ov.margin_summary.account_value <= Decimal::ZERO {
                continue;
            }
            let ratio = account::mul(
                account::ratio_or(prev_mark, op.entry_px, prev_mark),
                account::mul(op.szi.abs(), prev_mark, "adl notional")?
                    / ov.margin_summary.account_value,
                "adl rank",
            )?;
            ranked.push((ratio, *other, op.szi.abs(), op.entry_px));
        }
        Ok(ranked)
    }

    /// Record the marks the ADL step reads as "the previous mark".
    fn record_marks(&self, feed: &MarketFeed) -> Result<()> {
        let mut st = self.state.lock()?;
        for coin in feed.markets.clone() {
            if let Some(mark) = feed.mark(&coin) {
                st.prev_marks.insert(coin, mark);
            }
        }
        Ok(())
    }

    pub fn save_if_dirty(&self) -> Result<bool> {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            self.dirty.store(false, Ordering::Relaxed);
            return Ok(());
        };
        let snapshot = self.state.lock()?.clone();
        store::save(path, &snapshot, "sim state")?;
        self.dirty.store(false, Ordering::Relaxed);
        self.publish_gauges();
        Ok(())
    }

    fn mark_dirty(&self) -> Result<()> {
        self.dirty.store(true, Ordering::Relaxed);
        self.save()
    }

    /// Mark the state dirty without saving: the bridge applies a block's
    /// effects and the cursor together, then saves once — a crash mid-block
    /// loses both (re-scan) rather than saving a half-applied block.
    fn touch(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn publish_gauges(&self) {
        if let Ok(state) = self.state.lock() {
            self.metrics.accounts.store(state.accounts.len() as u64, Ordering::Relaxed);
            self.metrics.epoch.store(state.epoch, Ordering::Relaxed);
        }
    }}

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

/// The waterfall's step for one underwater account.
enum Rescue {
    /// Negative equity: ADL against the opposite side.
    Adl,
    /// Below the backstop ratio (`3·equity < 2·maintenance`): the vault takes
    /// the positions over.
    Backstop,
    /// Otherwise: market orders to the book.
    Book,
}

/// The waterfall's step from the two readings (docs: ADL when underwater, the
/// vault below the backstop ratio, the book otherwise).
fn rescue_step(equity: Decimal, maintenance: Decimal) -> Result<Rescue> {
    if equity < Decimal::ZERO {
        return Ok(Rescue::Adl);
    }
    let backstop = account::mul(equity, Decimal::from(3), "backstop check")?
        < account::mul(maintenance, Decimal::from(2), "backstop check")?;
    Ok(if backstop {
        Rescue::Backstop
    } else {
        Rescue::Book
    })
}

/// The sim tests' feed fixture: one market per triple with a funding ctx
/// (`crate::wire::test_ctx`), fresh at `now_ms = 1`. The account tests share
/// it through `super::*`.
#[cfg(test)]
pub(crate) fn feed_with(marks: &[(&str, &str, &str)]) -> MarketFeed {
    let feed = MarketFeed::memory(marks.iter().map(|(c, _, _)| c.to_string()).collect());
    for (coin, mark, oracle) in marks {
        feed.put_ctx(coin.to_string(), crate::wire::test_ctx(mark, oracle), 1);
    }
    feed
}

#[cfg(test)]
mod tests {
    use super::*;

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
            sim.credit_spot(a, TokenRef { index: 0, coin: "USDC" }, Decimal::from(5)).unwrap();
            sim.usd_class_transfer(a, TokenRef { index: 0, coin: "USDC" }, Decimal::ONE, true).unwrap();
            sim.spot_send(a, Address::repeat_byte(0xbb), TokenRef { index: 0, coin: "USDC" }, Decimal::ONE).unwrap();
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
            .deal(&feed, &Deal { address: a, class: DealClass::Perp, coin: None, amount: "100.5".parse().unwrap() })
            .unwrap();
        assert_eq!(after, "100.5".parse::<Decimal>().unwrap());
        let after = sim
            .deal(&feed, &Deal { address: a, class: DealClass::Perp, coin: None, amount: "0.5".parse().unwrap() })
            .unwrap();
        assert_eq!(after, "101".parse::<Decimal>().unwrap());
    }

    /// The verifier's critical find: an amount that overflows must be refused
    /// cleanly (a panic under the state lock poisons every account path).
    #[test]
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
        sim.deal(&feed, &Deal { address: a, class: DealClass::Perp, coin: None, amount: max }).unwrap();
        let err = sim.deal(&feed, &Deal { address: a, class: DealClass::Perp, coin: None, amount: Decimal::ONE }).unwrap_err();
        assert!(matches!(err, Error::Overflow { .. }), "{err}");
        // The state is intact and readable (no poison).
        assert!(!sim.is_poisoned());
        assert_eq!(sim.account(a).unwrap().unwrap().perp_balance, max);
        let err = sim
            .deal(&feed, &Deal { address: a, class: DealClass::Spot, coin: Some("USDC".into()), amount: max })
            .unwrap();
        assert_eq!(err, max);
        let err = sim
            .deal(&feed, &Deal { address: a, class: DealClass::Spot, coin: Some("USDC".into()), amount: Decimal::ONE })
            .unwrap_err();
        assert!(matches!(err, Error::Overflow { .. }), "{err}");
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
        sim.deal(&feed, &Deal { address: a, class: DealClass::Spot, coin: Some("USDC".into()), amount: Decimal::from(250) })
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
        assert_eq!(account.perp_balance, Decimal::from(100), "the named summary landed");
        assert_eq!(
            account.spot.get(&0).map(|s| s.total),
            Some(Decimal::from(250)),
            "the unnamed spot balance survived the perp-only seed"
        );

        // A named balances list replaces the spot side (and a named empty one clears).
        sim.set_account(
            a,
            serde_json::from_str(r#"{"spotBalances": []}"#).unwrap(),
        )
        .unwrap();
        assert!(sim.account(a).unwrap().unwrap().spot.is_empty());
    }

    #[test]
    fn reset_blanks_and_bumps_the_epoch() {
        let feed = feed_with(&[("USDC", "1", "1")]);
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        sim.deal(&feed, &Deal { address: a, class: DealClass::Perp, coin: None, amount: Decimal::ONE }).unwrap();
        assert_eq!(sim.epoch().unwrap(), 0);
        let epoch = sim.reset().unwrap();
        assert_eq!(epoch, 1);
        assert!(sim.account(a).unwrap().is_none());
    }

    #[test]
    fn advance_jumps_the_clock_and_accrues() {
        let feed = feed_with(&[("BTC", "100", "100")]);
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

    /// The waterfall's attempt sizes (docs): 20 % above the $100k partial
    /// threshold, the full size below it and during the 30 s cooldown.
    #[test]
    fn liquidation_attempts_follow_the_partial_rules() {
        let full = Decimal::from(10);
        let (sz, partial) = attempt_size(full, Decimal::from(120_000), false).unwrap();
        assert!(partial, "above the threshold the attempt is partial");
        assert_eq!(sz, Decimal::from(2));
        let (sz, partial) = attempt_size(full, Decimal::from(120_000), true).unwrap();
        assert!(!partial, "the cooldown switches to the full size");
        assert_eq!(sz, full);
        let (sz, partial) = attempt_size(full, Decimal::from(50_000), false).unwrap();
        assert!(!partial, "below the threshold the attempt is full");
        assert_eq!(sz, full);
    }

    /// A meta with one asset (the waterfall's maintenance tiers read it).
    fn meta_with(coin: &str, max_leverage: u32) -> crate::wire::Meta {
        let mut m: crate::wire::Meta = serde_json::from_str(r#"{"universe": []}"#).unwrap();
        m.universe.push(crate::wire::AssetInfo {
            name: coin.into(),
            sz_decimals: 4,
            max_leverage,
            margin_table_id: None,
            only_isolated: None,
            is_delisted: None,
        });
        m
    }

    /// The waterfall's fixture: one account holding a single ETH position.
    fn put_account(sim: &Sim, address: Address, facts: (Decimal, Decimal, Decimal, u32)) {
        let (balance, entry, szi, leverage) = facts;
        sim.with_account(address, |acc| {
            acc.perp_balance = balance;
            acc.positions.insert(
                "ETH".into(),
                crate::sim::account::Position {
                    szi,
                    entry_px: entry,
                    leverage: crate::wire::LeverageType::Cross,
                    leverage_value: leverage,
                    margin_used: Decimal::ZERO,
                    isolated_margin: Decimal::ZERO,
                    funding_all_time: Decimal::ZERO,
                    funding_since_open: Decimal::ZERO,
                    funding_since_change: Decimal::ZERO,
                },
            );
            Ok(())
        })
        .unwrap();
    }

    /// Below ⅔ of the maintenance requirement the backstop takes the position
    /// and the margin over: the vault holds the same side, the trader ends at
    /// zero equity (docs).
    #[test]
    fn the_backstop_takes_the_position_below_two_thirds() {
        let feed = feed_with(&[("ETH", "40", "40")]);
        feed.put_meta(meta_with("ETH", 10), crate::now_ms());
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        // Equity 0 (10 + (40 − 50)), maintenance 40/10/2 = 2, ⅔ of it 1.33.
        put_account(&sim, a, (Decimal::from(10), Decimal::from(50), Decimal::ONE, 10));
        let out = sim.check_all(&feed, crate::now_ms()).unwrap();
        assert_eq!(out.len(), 1, "one liquidation record");
        let a_acc = sim.account(a).unwrap().unwrap();
        assert!(a_acc
            .positions
            .get("ETH")
            .map(|p| p.szi.is_zero())
            .unwrap_or(true));
        assert_eq!(a_acc.perp_balance, Decimal::ZERO, "the maintenance is not returned");
        let vault = sim.account(BACKSTOP).unwrap().expect("the vault is touched");
        assert_eq!(
            vault.positions.get("ETH").map(|p| p.szi),
            Some(Decimal::ONE),
            "the vault takes the same side over"
        );
        assert_eq!(
            sim.metrics
                .backstop_liquidations_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    /// A negative account value goes to ADL: the opposite side (ranked by
    /// (mark/entry)·(notional/account_value)) is closed at the previous mark,
    /// and the platform stays solvent (docs).
    #[test]
    fn adl_closes_the_opposite_side_at_the_previous_mark() {
        let feed = feed_with(&[("ETH", "100", "100")]);
        feed.put_meta(meta_with("ETH", 10), crate::now_ms());
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa); // long 1 @ 100, thin
        let b = Address::repeat_byte(0xbb); // short 1 @ 100, in profit
        put_account(&sim, a, (Decimal::from(10), Decimal::from(100), Decimal::ONE, 10));
        put_account(&sim, b, (Decimal::from(10), Decimal::from(100), -Decimal::ONE, 10));
        // The previous mark is 90; the mark then falls to 80: A's equity = −10.
        feed.put_ctx("ETH".into(), crate::wire::test_ctx("90", "90"), crate::now_ms());
        sim.record_marks(&feed).unwrap();
        feed.put_ctx("ETH".into(), crate::wire::test_ctx("80", "80"), crate::now_ms());
        let out = sim.check_all(&feed, crate::now_ms()).unwrap();
        assert_eq!(out.len(), 1, "one liquidation record");
        let a_acc = sim.account(a).unwrap().unwrap();
        assert!(a_acc
            .positions
            .get("ETH")
            .map(|p| p.szi.is_zero())
            .unwrap_or(true));
        // A closed at 90: (90 − 100)·1 = −10 → balance 0 (solvent, no bad debt).
        assert_eq!(a_acc.perp_balance, Decimal::ZERO);
        let b_acc = sim.account(b).unwrap().unwrap();
        assert!(b_acc
            .positions
            .get("ETH")
            .map(|p| p.szi.is_zero())
            .unwrap_or(true));
        // B bought back at the previous mark 90: (100 − 90)·1 = +10.
        assert_eq!(b_acc.perp_balance, Decimal::from(20));
        assert_eq!(
            sim.metrics
                .adl_closes_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }
}
