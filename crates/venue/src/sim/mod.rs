//! The account plane: the simulated accounts registry (COW overlay), its
//! persistence, and the self-advance automaton (plan §5, §14.8).
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
use account::{Account, Accounts, Liquidation};
use seed::AccountSeed;
use metrics::SimMetrics;
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
        let state = store::load::<SimState>(&path, ARTIFACT)?.unwrap_or_default();
        Ok(Self::from_state(state, Some(path)))
    }

    /// An in-memory registry (tests only: no production path builds one).
    #[cfg(test)]
    pub fn memory() -> Self {
        Self::from_state(SimState::default(), None)
    }

    fn from_state(state: SimState, path: Option<PathBuf>) -> Self {
        let sim = Self {
            state: Shared::new(ARTIFACT, state),
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

    /// The scenario epoch counter (tests: `reset` returns it to callers).
    #[cfg(test)]
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
        self.mutate_account(address, |account| {
            stamp_funding_baseline(account, clock);
            let entry = account::spot_entry(account, token);
            entry.total = account::add(entry.total, amount, "spot credit")?;
            Ok(())
        })
    }

    /// Core→EVM / spot send: debit the spot balance; `None` when the account
    /// or the balance is short (the bridge counts the refusal — a system
    /// transfer never goes negative).
    pub fn try_debit_spot(&self, address: Address, token: u32, amount: Decimal) -> Result<Option<Decimal>> {
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
        let debited = self.with_state(|state| {
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
            Ok(true)
        })?;
        if !debited {
            return Ok(false);
        }
        self.credit_spot(to, token, amount)?;
        Ok(true)
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
    pub(crate) fn set_mode(&self, address: Address, mode: crate::wire::UserAbstraction) -> Result<()> {
        account::refuse_unmodeled_mode(mode)?;
        self.mutate_account(address, |account| {
            account.mode = mode;
            Ok(())
        })
    }

    /// `reset`: blank the scenario (accounts + clock offset), bump the epoch.
    pub fn reset(&self) -> Result<u64> {
        let epoch = self.with_state(|state| {
            state.accounts.clear();
            state.clock_offset_ms = 0;
            state.epoch += 1;
            Ok(state.epoch)
        })?;
        self.save()?;
        Ok(epoch)
    }

    /// `advance`: script a clock jump (`seconds`) with `blocks` liquidation
    /// checks inside it — the deterministic complement to self-advance.
    pub(crate) fn advance(&self, feed: &MarketFeed, seconds: u64, blocks: u64) -> Result<AdvanceOutcome> {
        let jump_ms = seconds.saturating_mul(1_000);
        self.with_state(|state| {
            state.clock_offset_ms = state.clock_offset_ms.saturating_add(jump_ms as i64);
            Ok(())
        })?;
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

    /// The venue's nonce rule (plan §5): the 100 highest nonces per signer,
    /// accepted in the window (T − 2 d, T + 1 d); a duplicate is refused by
    /// name. Called only when the body carries a nonce (their harness's
    /// nonce-less shape stays accepted — the devnet's impersonation analogue).
    pub(crate) fn use_nonce(&self, signer: Address, nonce: u64, now_ms: u64) -> Result<()> {
        const TWO_DAYS_MS: u64 = 2 * 24 * 60 * 60 * 1_000;
        const ONE_DAY_MS: u64 = 24 * 60 * 60 * 1_000;
        self.with_state(|st| {
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
        })?;
        self.mark_dirty()?;
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
        assert!(refused.unwrap_err().to_string().contains("portfolio margin"));
        assert_eq!(sim.account(a).unwrap().map(|acc| acc.mode), None, "nothing entered the state");
        sim.set_mode(a, crate::wire::UserAbstraction::UnifiedAccount).unwrap();
        assert_eq!(
            sim.account(a).unwrap().unwrap().mode,
            crate::wire::UserAbstraction::UnifiedAccount
        );
    }

    /// The account verbs' shared tail: a plain mutation marks the sim dirty
    /// (the bridge's per-block save is driven by this flag).
    #[test]
    fn the_account_verbs_mark_the_sim_dirty() {
        let sim = Sim::memory();
        assert!(!sim.save_if_dirty().unwrap());
        sim.note_agent(
            Address::repeat_byte(0xaa),
            Address::repeat_byte(0xbb),
            "bot",
        )
        .unwrap();
        assert!(sim.save_if_dirty().unwrap());
    }
}
