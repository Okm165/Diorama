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
pub struct SimState {
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
    #[serde(default)]
    pub accounts: Accounts,
}

/// The sim's metrics (rendered beside the feed's).
#[derive(Default)]
pub struct SimMetrics {
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
}

impl SimMetrics {
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512);
        mirror_devnet::metrics::push_metric(&mut out, "gauge", "venue_sim_accounts", "simulated (touched) accounts", self.accounts.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "gauge", "venue_sim_epoch", "scenario epoch (reset bumps it)", self.epoch.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_liquidations_total", "liquidations executed by the sim", self.liquidations_total.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_funding_payments_total", "funding payments applied", self.funding_payments_total.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_identity_violations_total", "served states that failed the identity re-check (must stay 0)", self.identity_violations.load(Ordering::Relaxed));
        mirror_devnet::metrics::push_metric(&mut out, "counter", "venue_sim_advances_total", "scenario advance() steps", self.advances_total.load(Ordering::Relaxed));
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
pub struct Deal {
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
pub struct TokenRef<'a> {
    pub index: u32,
    pub coin: &'a str,
}

/// Which side of the venue `deal` mints into.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DealClass {
    /// The perp wallet (`totalRawUsd`; the enforcement's recall source).
    Perp,
    /// The spot wallet (by token; USDC is token 0).
    Spot,
}

pub struct Sim {
    state: Shared<SimState>,
    path: Option<PathBuf>,
    dirty: AtomicBool,
    /// The venue chain's head timestamp (ms), observed by the bridge when V is
    /// configured. Their time policy jumps *every* chain's clock forward and
    /// the client's own rule is `chain_time = max(wall, head)` — a served
    /// `time` that stayed at wall would read as stale after a V jump.
    chain_time_ms: AtomicU64,
    pub metrics: SimMetrics,
}

impl Sim {
    /// Load the registry from its state file (absent = first boot, empty).
    pub fn load(path: PathBuf) -> Result<Self> {
        let state = store::load::<SimState>(&path, "sim state")?.unwrap_or_default();
        Ok(Self::from_state(state, Some(path)))
    }

    /// An in-memory registry (tests).
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
    pub fn account(&self, address: Address) -> Result<Option<Account>> {
        Ok(self.state.lock()?.accounts.get(&address).cloned())
    }

    /// Every touched address (control-side diagnostics).
    pub fn addresses(&self) -> Result<Vec<Address>> {
        Ok(self.state.lock()?.accounts.keys().copied().collect())
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
    pub fn deal(&self, feed: &MarketFeed, deal: &Deal) -> Result<Decimal> {
        let now = crate::now_ms();
        let clock = self.clock(now)?;
        let mut state = self.state.lock()?;
        let account = state.accounts.entry(deal.address).or_default();
        stamp_funding_baseline(account, clock);
        let after = match deal.class {
            DealClass::Perp => {
                account.perp_balance = account::add(account.perp_balance, deal.amount, "perp deal")?;
                account.perp_balance
            }
            DealClass::Spot => {
                let coin = deal.coin.as_deref().unwrap_or("USDC");
                let index = feed.token_of(coin).ok_or_else(|| Error::BadRequest {
                    reason: format!("spot coin `{coin}` is not in the venue's token registry yet"),
                })?;
                let entry = account::spot_entry(account, TokenRef { index, coin });
                entry.total = account::add(entry.total, deal.amount, "spot deal")?;
                entry.total
            }
        };
        drop(state);
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
    pub fn set_account(&self, address: Address, seed: AccountSeed) -> Result<()> {
        if seed.is_empty() {
            return Err(Error::BadRequest {
                reason: "the patch is empty (no field named)".into(),
            });
        }
        let now = crate::now_ms();
        let clock = self.clock(now)?;
        let mut state = self.state.lock()?;
        let account = state.accounts.entry(address).or_default();
        seed.apply_to(account, clock)?;
        // Remember the patch as the control fixture, merged over the previous
        // one (hl_mock's rule: a named field replaces, an unnamed one keeps).
        let merged = seed.merged_with(account.control.take());
        account.control = Some(merged);
        drop(state);
        self.mark_dirty()
    }

    /// The account's control fixture (the merged patch), when set.
    pub fn control_fixture(&self, address: Address) -> Result<Option<AccountSeed>> {
        Ok(self
            .state
            .lock()?
            .accounts
            .get(&address)
            .and_then(|a| a.control.clone()))
    }

    // ---- the Core↔EVM bridge's effects -----------------------------------

    /// EVM→Core: credit a spot token (creates the account on first touch).
    pub fn credit_spot(&self, address: Address, token: TokenRef<'_>, amount: Decimal) -> Result<()> {
        // Read the clock *before* taking the state lock: `clock()` locks the
        // same mutex, and a std Mutex is not reentrant.
        let clock = self.clock(crate::now_ms())?;
        let mut state = self.state.lock()?;
        let account = state.accounts.entry(address).or_default();
        stamp_funding_baseline(account, clock);
        let entry = account::spot_entry(account, token);
        entry.total = account::add(entry.total, amount, "spot credit")?;
        drop(state);
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
    pub fn usd_class_transfer(&self, address: Address, token: TokenRef<'_>, amount: Decimal, to_perp: bool) -> Result<bool> {
        let clock = self.clock(crate::now_ms())?;
        let mut state = self.state.lock()?;
        let account = state.accounts.entry(address).or_default();
        stamp_funding_baseline(account, clock);
        if to_perp {
            let entry = account::spot_entry(account, token);
            if entry.total < amount {
                return Ok(false);
            }
            entry.total = account::sub(entry.total, amount, "usd class transfer")?;
            account.perp_balance = account::add(account.perp_balance, amount, "usd class transfer")?;
        } else {
            if account.perp_balance < amount {
                return Ok(false);
            }
            account.perp_balance = account::sub(account.perp_balance, amount, "usd class transfer")?;
            let entry = account::spot_entry(account, token);
            entry.total = account::add(entry.total, amount, "usd class transfer")?;
        }
        drop(state);
        self.touch();
        Ok(true)
    }

    /// Core spot send: debit the sender, credit the recipient. `false` when
    /// the sender is short.
    pub fn spot_send(&self, from: Address, to: Address, token: TokenRef<'_>, amount: Decimal) -> Result<bool> {
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
        let mut state = self.state.lock()?;
        let account = state.accounts.entry(address).or_default();
        if account.agents.iter().all(|a| a.wallet != wallet) {
            account.agents.push(account::AgentWallet {
                wallet,
                name: name.to_string(),
            });
        }
        drop(state);
        self.touch();
        Ok(())
    }

    /// Set the account's abstraction mode (CoreWriter action 16).
    pub fn set_mode(&self, address: Address, mode: crate::wire::UserAbstraction) -> Result<()> {
        let mut state = self.state.lock()?;
        let account = state.accounts.entry(address).or_default();
        account.mode = mode;
        drop(state);
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
    pub fn advance(&self, feed: &MarketFeed, seconds: u64, blocks: u64) -> Result<AdvanceOutcome> {
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
        let liquidated = self.check_all(feed, now)?;
        self.save_if_dirty()?;
        Ok(liquidated)
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

    /// Check every account against its maintenance requirement and close the
    /// ones below it (v1: full close at the mark).
    pub fn check_all(&self, feed: &MarketFeed, now_ms: u64) -> Result<Vec<Liquidation>> {
        let clock = self.clock(now_ms)?;
        let meta = feed.meta();
        let mut state = self.state.lock()?;
        let mut liquidated = Vec::new();
        for (address, account) in state.accounts.iter_mut() {
            account::accrue_funding(account, feed, clock)?;
            if let Some(liq) = account::liquidate_if_needed(account, feed, meta.as_ref())? {
                self.metrics.liquidations_total.fetch_add(1, Ordering::Relaxed);
                self.dirty.store(true, Ordering::Relaxed);
                tracing::warn!(
                    account = %address,
                    positions = ?liq.positions_closed,
                    account_value = %liq.account_value,
                    maintenance = %liq.maintenance,
                    "sim liquidation: the account fell below maintenance; positions closed at the mark"
                );
                liquidated.push(liq);
            }
        }
        Ok(liquidated)
    }

    // ---- persistence ------------------------------------------------------

    /// Persist if anything moved since the last save.
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
    }
}

/// What `advance` moved.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvanceOutcome {
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
        // 10 s: a deadlock never answers, so the budget only has to outlast a
        // loaded parallel test run (5 s flaked once on a full-workspace run).
        rx.recv_timeout(std::time::Duration::from_secs(10))
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
}
