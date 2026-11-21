//! The per-account fill and funding ledger: what the venue's `userFills` and
//! `userFunding` reads (and the `userFills` / `userFundings` streams) serve.
//!
//! The ledger is **history, never state**: it is written beside every fill and
//! every funding payment the sim already makes and read by nothing in the
//! account math, so no balance, position or served figure depends on it. It
//! lives inside the [`Account`] (persisted with it, bounded at
//! [`LEDGER_BOUND`] entries per list, oldest dropped) behind an `Arc`: the
//! account is cloned on many reads and the waterfall's snapshot, and a bounded
//! 10k-entry ledger must not be copied each time (a write copies it only when
//! a snapshot is still alive).
//!
//! Touched is the registry's notion: an address the sim holds an account for.
//! An untouched address answers `None` (the API layer passes the read through
//! to the real venue, the same copy-on-write rule as every account read).
//!
//! Time is the **sim clock**, like funding's boundaries: a scripted `advance`
//! moves fills and funding payments together, so a reader orders them on one
//! axis.
use super::account::{Account, FillFacts};
use super::order::Side;
use super::Sim;
use crate::wire::decimal_nano;
use alloy_primitives::{Address, B128};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;

/// The most fills (and, separately, funding payments) kept per account; the
/// oldest entries drop first.
pub(crate) const LEDGER_BOUND: usize = 10_000;

/// One fill, as the venue's `userFills` serves it (camelCase; decimals are
/// the wire's nano-trimmed strings).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserFill {
    pub coin: String,
    #[serde(serialize_with = "decimal_nano::serialize")]
    pub px: Decimal,
    #[serde(serialize_with = "decimal_nano::serialize")]
    pub sz: Decimal,
    /// `"B"` (buy) or `"A"` (sell).
    pub side: String,
    /// Milliseconds on the sim clock.
    pub time: u64,
    /// The signed position size before this fill.
    #[serde(serialize_with = "decimal_nano::serialize")]
    pub start_position: Decimal,
    /// `Open Long` | `Close Long` | `Open Short` | `Close Short` |
    /// `Long > Short` | `Short > Long`.
    pub dir: String,
    /// The realized PnL of the closed part (zero for a pure open), the amount
    /// the fill realized into the balance before the fee.
    #[serde(serialize_with = "decimal_nano::serialize")]
    pub closed_pnl: Decimal,
    /// `0x` + 64 hex digits, deterministic from `tid`.
    pub hash: String,
    pub oid: u64,
    /// Taker true, maker (passive) false.
    pub crossed: bool,
    /// Signed; negative is a rebate.
    #[serde(serialize_with = "decimal_nano::serialize")]
    pub fee: Decimal,
    /// The builder fee the fill charged on top of `fee` (the order's
    /// `builder`); omitted when the fill carried none, as the venue's is.
    /// The sim does not charge builder fees yet (the order lane's builder
    /// delta lands with `/exchange`), so this is `None` on every fill.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_opt_nano"
    )]
    pub builder_fee: Option<Decimal>,
    /// Unique and strictly increasing across the sim.
    pub tid: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloid: Option<String>,
    /// Present on a liquidation's fills (the liquidated user's own closes and
    /// the backstop's takeover), absent on every other fill, as the venue's is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liquidation: Option<FillLiquidation>,
    pub fee_token: String,
    /// Always `None` (no TWAP orders), served as `null`.
    pub twap_id: Option<u64>,
}

/// How a liquidation was executed (the venue's `liquidation.method`): a market
/// order to the book, or the backstop liquidator taking the position over.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LiquidationMethod {
    Market,
    Backstop,
}

/// The `liquidation` object of a liquidation's fill (measured live):
/// `{liquidatedUser, markPx, method}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FillLiquidation {
    /// The liquidated account, `0x` + lowercase hex.
    pub liquidated_user: String,
    /// The mark the liquidation was decided at.
    #[serde(serialize_with = "decimal_nano::serialize")]
    pub mark_px: Decimal,
    pub method: LiquidationMethod,
}

impl FillLiquidation {
    pub(crate) fn new(user: Address, mark_px: Decimal, method: LiquidationMethod) -> Self {
        Self {
            liquidated_user: format!("{user:#x}"),
            mark_px,
            method,
        }
    }
}

/// The venue's `dir` for the backstop liquidator's takeover fill: the
/// liquidated position's own label, `Liquidated Cross Long` and its three
/// siblings (measured live on the liquidator's fills), not an `Open`. The
/// backstop takeover's fill record adopts it with the liquidation-ledger step.
#[allow(dead_code)]
pub(crate) fn liquidated_dir(
    leverage: crate::wire::LeverageType,
    direction: super::account::Direction,
) -> &'static str {
    use super::account::Direction::{Long, Short};
    use crate::wire::LeverageType::{Cross, Isolated};
    match (leverage, direction) {
        (Cross, Long) => "Liquidated Cross Long",
        (Cross, Short) => "Liquidated Cross Short",
        (Isolated, Long) => "Liquidated Isolated Long",
        (Isolated, Short) => "Liquidated Isolated Short",
    }
}

/// The wire's nano-trimmed decimal string for an optional decimal (only
/// written when present: the field's `skip_serializing_if` omits `None`).
fn serialize_opt_nano<S: serde::Serializer>(
    value: &Option<Decimal>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => decimal_nano::serialize(v, serializer),
        None => serializer.serialize_none(),
    }
}

/// One funding payment, as the venue's `userFunding` delta carries it: the
/// info route wraps it as `{time, hash, delta}`, the stream serves it flat.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserFunding {
    /// The hour boundary (ms) the payment is stamped with.
    pub time: u64,
    pub coin: String,
    /// The signed balance delta (negative = paid).
    #[serde(serialize_with = "decimal_nano::serialize")]
    pub usdc: Decimal,
    /// The signed position size at the payment.
    #[serde(serialize_with = "decimal_nano::serialize")]
    pub szi: Decimal,
    #[serde(serialize_with = "decimal_nano::serialize")]
    pub funding_rate: Decimal,
    /// Always `None`, served as `null`.
    pub n_samples: Option<u32>,
}

impl UserFunding {
    /// One hourly payment of `(usdc, rate)` on a position of `szi`, not yet
    /// stamped (`time` 0: the boundary is the caller's).
    pub(crate) fn hourly(coin: &str, szi: Decimal, payment: (Decimal, Decimal)) -> Self {
        Self {
            time: 0,
            coin: coin.to_string(),
            usdc: payment.0,
            szi,
            funding_rate: payment.1,
            n_samples: None,
        }
    }
}

/// The `Arc`-shared list's serde shim: the persisted shape is the plain list,
/// so a state file written before the ledger existed (or by a build that
/// stored it plainly) reads the same.
pub(crate) mod shared {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::VecDeque;
    use std::sync::Arc;

    pub fn serialize<S, T>(list: &Arc<VecDeque<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        list.as_ref().serialize(serializer)
    }

    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Arc<VecDeque<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        VecDeque::<T>::deserialize(deserializer).map(Arc::new)
    }

    /// Whether the list is empty (an empty ledger is not written, so an
    /// account that never traded persists exactly as before).
    pub fn is_empty<T>(list: &Arc<VecDeque<T>>) -> bool {
        list.is_empty()
    }
}

/// What one applied fill did to the position: the facts the ledger needs that
/// only the account math knows (computed there, never re-derived).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FillEffect {
    /// The signed size before the fill.
    pub start_position: Decimal,
    /// The realized PnL of the closed part (zero for an open).
    pub closed_pnl: Decimal,
    /// The venue's direction label.
    pub dir: &'static str,
}

impl FillEffect {
    /// The effect of a fill of signed size `signed` on a position of `old`
    /// that realized `closed_pnl`.
    pub(crate) fn of(old: Decimal, signed: Decimal, closed_pnl: Decimal) -> Self {
        Self {
            start_position: old,
            closed_pnl,
            dir: direction_label(old, signed),
        }
    }
}

/// The venue's `dir` for a fill of signed size `signed` on a position of
/// `old`: an open or an add on the same side, a reduction, or a flip through
/// zero.
fn direction_label(old: Decimal, signed: Decimal) -> &'static str {
    let buy = signed > Decimal::ZERO;
    if old.is_zero() || (old > Decimal::ZERO) == buy {
        return if buy { "Open Long" } else { "Open Short" };
    }
    let long = old > Decimal::ZERO;
    let new = old + signed;
    let flips = !new.is_zero() && (new > Decimal::ZERO) != long;
    match (long, flips) {
        (true, true) => "Long > Short",
        (true, false) => "Close Long",
        (false, true) => "Short > Long",
        (false, false) => "Close Short",
    }
}

/// Which side of the match a fill was on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liquidity {
    /// The aggressor (`crossed: true`): an immediate take, a liquidation
    /// close, a settlement.
    Taker,
    /// The resting order that was matched (`crossed: false`).
    Maker,
}

/// One applied fill, ready to record: the facts the account math took, what
/// it did to the position, and the order-side facts only the caller knows.
#[derive(Debug, Clone)]
pub(crate) struct FillRecord<'a> {
    /// `Some` on a liquidation's fills (see [`FillLiquidation`]).
    pub liquidation: Option<FillLiquidation>,
    pub facts: &'a FillFacts,
    pub effect: FillEffect,
    /// The order's id (0 for a fill that no order made: a delisting
    /// settlement, a backstop takeover, an ADL close).
    pub oid: u64,
    pub cloid: Option<B128>,
    pub liquidity: Liquidity,
    /// The token the fee was paid in. The sim's perp clearinghouse is USDC
    /// (the default dex's collateral); a builder dex's own collateral token
    /// lands with the per-dex ledger.
    pub fee_token: &'a str,
}

/// A deterministic transaction hash from the trade id: `0x` + 64 hex digits.
fn hash_of(tid: u64) -> String {
    format!("0x{tid:064x}")
}

fn push_bounded<T: Clone>(list: &mut Arc<VecDeque<T>>, entry: T) {
    let list = Arc::make_mut(list);
    list.push_back(entry);
    while list.len() > LEDGER_BOUND {
        list.pop_front();
    }
}

/// Append one fill to the account's ledger under a fresh trade id (`next_tid`
/// is the sim's counter; the caller holds the state lock) stamped `time` (the
/// sim clock, ms).
pub(crate) fn record_fill(
    account: &mut Account,
    next_tid: &mut u64,
    time: u64,
    fill: &FillRecord<'_>,
) {
    *next_tid = next_tid.saturating_add(1);
    let tid = *next_tid;
    let facts = fill.facts;
    push_bounded(
        &mut account.fills,
        UserFill {
            coin: facts.coin.clone(),
            px: facts.px,
            sz: facts.size,
            side: match facts.side {
                Side::Buy => "B",
                Side::Sell => "A",
            }
            .to_string(),
            time,
            start_position: fill.effect.start_position,
            dir: fill.effect.dir.to_string(),
            closed_pnl: fill.effect.closed_pnl,
            hash: hash_of(tid),
            oid: fill.oid,
            crossed: fill.liquidity == Liquidity::Taker,
            fee: facts.fee,
            builder_fee: None,
            tid,
            cloid: fill
                .cloid
                .map(|c| format!("0x{}", alloy_primitives::hex::encode(c))),
            liquidation: fill.liquidation.clone(),
            fee_token: fill.fee_token.to_string(),
            twap_id: None,
        },
    );
}

/// Append one funding payment to a funding list.
pub(crate) fn record_funding(list: &mut Arc<VecDeque<UserFunding>>, payment: UserFunding) {
    push_bounded(list, payment);
}

impl Sim {
    /// The account's fills, oldest first; `None` when the sim never touched
    /// the address (the caller passes the read through to the real venue).
    /// A poisoned state lock also answers `None`: `is_poisoned` is the
    /// health signal, and no fill is guessed. The `/info` and ws reads adopt
    /// it with the API steps.
    #[allow(dead_code)]
    pub(crate) fn user_fills(&self, user: Address) -> Option<Vec<UserFill>> {
        let state = self.state.lock().ok()?;
        let account = state.accounts.get(&user)?;
        Some(account.fills.iter().cloned().collect())
    }

    /// The account's funding payments stamped at or after `start_ms`, oldest
    /// first; `None` when the sim never touched the address. The `/info` and
    /// ws reads adopt it with the API steps.
    #[allow(dead_code)]
    pub(crate) fn user_fundings(&self, user: Address, start_ms: u64) -> Option<Vec<UserFunding>> {
        let state = self.state.lock().ok()?;
        let account = state.accounts.get(&user)?;
        Some(
            account
                .fundings
                .iter()
                .filter(|f| f.time >= start_ms)
                .cloned()
                .collect(),
        )
    }

    /// Record one applied fill on `address`'s ledger (a no-op for an address
    /// the sim holds no account for: nothing to attribute it to). The clock is
    /// read before the lock (`clock()` takes the same mutex).
    pub(super) fn record_fill(
        &self,
        address: Address,
        fill: &FillRecord<'_>,
    ) -> crate::error::Result<()> {
        let time = self.clock(crate::now_ms())?;
        self.with_state(|st| {
            if let Some(account) = st.accounts.get_mut(&address) {
                record_fill(account, &mut st.next_tid, time, fill);
            }
            Ok(())
        })?;
        self.touch();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::order::{Order, Tif};
    use crate::sim::{Deal, DealClass};
    use crate::wire::{L2Book, Level, Meta};

    /// A book with the given (bids, asks) as (px, sz) pairs.
    fn book(bids: &[(&str, &str)], asks: &[(&str, &str)]) -> L2Book {
        let level = |(px, sz): &(&str, &str)| Level {
            px: px.parse().unwrap(),
            sz: sz.parse().unwrap(),
            n: 1,
        };
        L2Book {
            coin: "ETH".into(),
            time: 0,
            levels: [
                bids.iter().map(level).collect(),
                asks.iter().map(level).collect(),
            ],
            spread: None,
        }
    }

    /// A live feed serving `book` with the best ask as the mark.
    fn feed_with_book(book: L2Book, meta: Meta) -> crate::feed::MarketFeed {
        let feed = crate::feed::MarketFeed::memory(vec!["ETH".into()]);
        let now = crate::now_ms();
        feed.put_meta(meta, now);
        let mark = book.levels[1].first().map_or(Decimal::ONE, |l| l.px);
        feed.put_ctx(
            "ETH".into(),
            crate::wire::test_ctx(&mark.to_string(), &mark.to_string()),
            now,
        );
        feed.put_book(book, now);
        feed.put_oi_caps(Vec::new());
        feed
    }

    fn deal(sim: &Sim, feed: &crate::feed::MarketFeed, who: Address, amount: &str) {
        sim.deal(
            feed,
            &Deal {
                address: who,
                class: DealClass::Perp,
                coin: None,
                amount: amount.parse().unwrap(),
            },
        )
        .unwrap();
    }

    fn market(is_buy: bool, sz: &str) -> Order {
        Order {
            coin: "ETH".into(),
            is_buy,
            size: sz.parse().unwrap(),
            limit_px: None,
            tif: Tif::Ioc,
            reduce_only: false,
            cloid: None,
            trigger: None,
        }
    }

    fn dec(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    /// An open, then a partial close: the directions, the starting sizes, the
    /// realized PnL of the closed part only, the fees the sim charged, and a
    /// strictly increasing tid.
    #[test]
    fn open_then_partial_close_records_dir_start_position_and_closed_pnl() {
        let feed = feed_with_book(
            book(&[("110", "5")], &[("101", "5")]),
            crate::sim::meta_with("ETH", 10),
        );
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        deal(&sim, &feed, a, "10000");

        let open = sim.order(&feed, a, &market(true, "2")).unwrap();
        let close = sim.order(&feed, a, &market(false, "0.5")).unwrap();

        let fills = sim.user_fills(a).expect("a touched account");
        assert_eq!(fills.len(), 2);
        let (o, c) = (&fills[0], &fills[1]);
        assert_eq!(o.dir, "Open Long");
        assert_eq!(o.start_position, Decimal::ZERO);
        assert_eq!((o.side.as_str(), o.px, o.sz), ("B", dec("101"), dec("2")));
        assert_eq!(o.closed_pnl, Decimal::ZERO);
        assert!(o.crossed);
        assert_eq!(o.oid, open.oid);
        assert_eq!(
            o.fee,
            dec("2") * dec("101") * crate::sim::order::taker_fee()
        );
        assert_eq!(o.fee_token, "USDC");
        assert_eq!(o.twap_id, None);
        assert_eq!(c.dir, "Close Long");
        assert_eq!(c.start_position, dec("2"));
        assert_eq!((c.side.as_str(), c.px, c.sz), ("A", dec("110"), dec("0.5")));
        // (110 - 101) * 0.5, the part realized into the balance.
        assert_eq!(c.closed_pnl, dec("4.5"));
        assert_eq!(c.oid, close.oid);
        assert!(o.tid < c.tid);
        assert_eq!(o.hash.len(), 66);
        assert_ne!(o.hash, c.hash);
    }

    /// Selling through zero is one fill labelled `Long > Short`, realizing
    /// only the closed (long) part; the reverse flip reads `Short > Long`; a
    /// buy that reduces a short is `Close Short`.
    #[test]
    fn a_flip_is_labelled_and_realizes_only_the_closed_part() {
        let feed = feed_with_book(
            book(&[("110", "9")], &[("101", "9")]),
            crate::sim::meta_with("ETH", 10),
        );
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        deal(&sim, &feed, a, "10000");

        sim.order(&feed, a, &market(true, "1")).unwrap();
        sim.order(&feed, a, &market(false, "3")).unwrap();
        sim.order(&feed, a, &market(true, "1")).unwrap();
        sim.order(&feed, a, &market(true, "3")).unwrap();

        let fills = sim.user_fills(a).unwrap();
        let dirs: Vec<&str> = fills.iter().map(|f| f.dir.as_str()).collect();
        assert_eq!(
            dirs,
            ["Open Long", "Long > Short", "Close Short", "Short > Long"]
        );
        // Sold 3 at 110 against a long 1 at 101: only the 1 closed.
        assert_eq!(fills[1].start_position, dec("1"));
        assert_eq!(fills[1].closed_pnl, dec("9"));
        // Short 2 at 110, bought 1 at 101: (110 - 101) * 1.
        assert_eq!(fills[2].start_position, dec("-2"));
        assert_eq!(fills[2].closed_pnl, dec("9"));
        // Short 1 at 110, bought 3 at 101: the closed 1 realizes 9.
        assert_eq!(fills[3].start_position, dec("-1"));
        assert_eq!(fills[3].closed_pnl, dec("9"));
    }

    /// Both parties of an internal match are recorded: the resting maker
    /// crossed=false at its own price with the maker fee, the taker true.
    #[test]
    fn a_resting_maker_fill_is_not_crossed() {
        // No served bids: the maker's resting bid is the only liquidity.
        let feed = feed_with_book(book(&[], &[("101", "5")]), crate::sim::meta_with("ETH", 10));
        let sim = Sim::memory();
        let maker = Address::repeat_byte(0xaa);
        let taker = Address::repeat_byte(0xbb);
        deal(&sim, &feed, maker, "1000");
        deal(&sim, &feed, taker, "1000");
        let cloid = alloy_primitives::B128::repeat_byte(0x11);
        let rest = Order {
            limit_px: Some(dec("100")),
            tif: Tif::Gtc,
            cloid: Some(cloid),
            ..market(true, "1")
        };
        let oid = sim.place(&feed, maker, &rest).unwrap().resting.unwrap();
        let hit = Order {
            limit_px: Some(dec("100")),
            ..market(false, "1")
        };
        sim.place(&feed, taker, &hit).unwrap();

        let m = &sim.user_fills(maker).unwrap()[0];
        assert!(!m.crossed);
        assert_eq!(
            (m.oid, m.px, m.sz, m.side.as_str()),
            (oid, dec("100"), dec("1"), "B")
        );
        assert_eq!(m.dir, "Open Long");
        assert_eq!(m.fee, dec("100") * crate::sim::order::maker_fee());
        assert_eq!(
            m.cloid.as_deref(),
            Some("0x11111111111111111111111111111111")
        );
        let t = &sim.user_fills(taker).unwrap()[0];
        assert!(t.crossed);
        assert_eq!(
            (t.px, t.side.as_str(), t.dir.as_str()),
            (dec("100"), "A", "Open Short")
        );
        assert!(t.cloid.is_none());
        assert!(m.tid != t.tid);
    }

    /// A resting order the live book crosses fills passively on the tick: the
    /// maker side again, not crossed.
    #[test]
    fn a_passive_tick_fill_is_not_crossed() {
        let feed = feed_with_book(
            book(&[("99", "5")], &[("101", "5")]),
            crate::sim::meta_with("ETH", 10),
        );
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        deal(&sim, &feed, a, "1000");
        let rest = Order {
            limit_px: Some(dec("100")),
            tif: Tif::Gtc,
            ..market(true, "1")
        };
        let oid = sim.place(&feed, a, &rest).unwrap().resting.unwrap();
        assert!(
            sim.user_fills(a).unwrap().is_empty(),
            "resting is not a fill"
        );
        // The ask falls through the order's price.
        feed.put_book(book(&[("99", "5")], &[("100", "5")]), crate::now_ms());
        sim.evaluate(&feed).unwrap();

        let fills = sim.user_fills(a).unwrap();
        assert_eq!(fills.len(), 1);
        assert!(!fills[0].crossed);
        assert_eq!(
            (fills[0].oid, fills[0].px, fills[0].sz),
            (oid, dec("100"), dec("1"))
        );
        assert_eq!(fills[0].fee, dec("100") * crate::sim::order::maker_fee());
    }

    /// A liquidation's book close is a fill of the liquidated account.
    #[test]
    fn a_liquidation_close_is_recorded() {
        let feed = feed_with_book(
            book(&[("100", "9")], &[("101", "9")]),
            crate::sim::meta_with("ETH", 10),
        );
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        deal(&sim, &feed, a, "1000");
        sim.order(&feed, a, &market(true, "2")).unwrap();
        let fill = sim.liquidate_position(&feed, a, "ETH", dec("2")).unwrap();

        let fills = sim.user_fills(a).unwrap();
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[1].dir, "Close Long");
        assert_eq!(fills[1].oid, fill.oid);
        assert_eq!(fills[1].fee, Decimal::ZERO, "no clearance fee");
        assert!(fills[1].crossed);
        assert_eq!(fills[1].closed_pnl, dec("-2"));
    }

    /// The wire shape: camelCase, nano-trimmed decimal strings, `null` for
    /// the twap id, `cloid` omitted when absent; the funding delta likewise.
    #[test]
    fn the_serialized_shapes_match_the_venue() {
        let fill = UserFill {
            coin: "BTC".into(),
            px: dec("64173.0"),
            sz: dec("0.50"),
            side: "B".into(),
            time: 1,
            start_position: Decimal::ZERO,
            dir: "Open Long".into(),
            closed_pnl: Decimal::ZERO,
            hash: hash_of(7),
            oid: 9,
            crossed: true,
            fee: dec("0.1440"),
            builder_fee: None,
            tid: 7,
            cloid: None,
            liquidation: None,
            fee_token: "USDC".into(),
            twap_id: None,
        };
        let v = serde_json::to_value(&fill).unwrap();
        assert_eq!(v["px"], "64173.0");
        assert_eq!(v["sz"], "0.5");
        assert_eq!(v["startPosition"], "0.0");
        assert_eq!(v["closedPnl"], "0.0");
        assert_eq!(v["fee"], "0.144");
        assert_eq!(v["feeToken"], "USDC");
        assert!(v["twapId"].is_null());
        assert!(v.get("cloid").is_none());
        assert!(v.get("liquidation").is_none());
        assert_eq!(v["hash"], format!("0x{}7", "0".repeat(63)));
        let back: UserFill = serde_json::from_value(v).unwrap();
        assert_eq!(back, fill);
        let funding = UserFunding {
            time: 3_600_000,
            coin: "BTC".into(),
            usdc: dec("-0.0012500"),
            szi: dec("1.0"),
            funding_rate: dec("0.0000125"),
            n_samples: None,
        };
        let v = serde_json::to_value(&funding).unwrap();
        assert_eq!(v["usdc"], "-0.00125");
        assert_eq!(v["fundingRate"], "0.0000125");
        assert!(v["nSamples"].is_null());
    }

    /// Both lists are bounded: the oldest entries drop first.
    #[test]
    fn the_ledger_is_bounded() {
        let mut account = Account::default();
        let mut next_tid = 0;
        let facts = FillFacts::close("ETH", Side::Sell, dec("1"), dec("1"), 1);
        let effect = FillEffect::of(dec("1"), dec("-1"), Decimal::ZERO);
        let fill = FillRecord {
            liquidation: None,
            facts: &facts,
            effect,
            oid: 1,
            cloid: None,
            liquidity: Liquidity::Taker,
            fee_token: "USDC",
        };
        for _ in 0..LEDGER_BOUND + 5 {
            record_fill(&mut account, &mut next_tid, 1, &fill);
            record_funding(
                &mut account.fundings,
                UserFunding {
                    time: next_tid * 3_600_000,
                    coin: "ETH".into(),
                    usdc: Decimal::ONE,
                    szi: Decimal::ONE,
                    funding_rate: Decimal::ONE,
                    n_samples: None,
                },
            );
        }
        assert_eq!(account.fills.len(), LEDGER_BOUND);
        assert_eq!(account.fundings.len(), LEDGER_BOUND);
        assert_eq!(
            account.fills.front().unwrap().tid,
            6,
            "the oldest five dropped"
        );
        assert_eq!(account.fills.back().unwrap().tid, LEDGER_BOUND as u64 + 5);
        assert_eq!(account.fundings.front().unwrap().time, 6 * 3_600_000);
    }

    /// A state file written before the ledger existed still loads (with an
    /// empty ledger), and a ledger round-trips through the file.
    #[test]
    fn an_old_state_file_loads_and_the_ledger_round_trips() {
        let old = r#"{"epoch": 2, "next_oid": 5, "accounts": {
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa": {"perp_balance": "12"}}}"#;
        let mut state: crate::sim::SimState = serde_json::from_str(old).unwrap();
        // The load folds the legacy single balance and marks the account known.
        state.migrate();
        let a = Address::repeat_byte(0xaa);
        assert_eq!(state.next_tid, 0);
        assert!(state.accounts[&a].fills.is_empty());
        assert!(state.accounts[&a].fundings.is_empty());
        assert_eq!(state.accounts[&a].perp_balance, dec("12"));

        let mut next_tid = state.next_tid;
        let facts = FillFacts::close("ETH", Side::Buy, dec("1"), dec("2"), 1);
        let effect = FillEffect::of(dec("-1"), dec("1"), dec("3"));
        let fill = FillRecord {
            liquidation: None,
            facts: &facts,
            effect,
            oid: 4,
            cloid: None,
            liquidity: Liquidity::Maker,
            fee_token: "USDC",
        };
        let account = state.accounts.get_mut(&a).unwrap();
        record_fill(account, &mut next_tid, 9, &fill);
        record_funding(
            &mut account.fundings,
            UserFunding {
                time: 3_600_000,
                coin: "ETH".into(),
                usdc: dec("-1"),
                szi: dec("1"),
                funding_rate: dec("0.0000125"),
                n_samples: None,
            },
        );
        state.next_tid = next_tid;
        let json = serde_json::to_string(&state).unwrap();
        let mut back: crate::sim::SimState = serde_json::from_str(&json).unwrap();
        back.migrate();
        assert_eq!(back, state);
        assert_eq!(back.accounts[&a].perp_balance, dec("12"));
        assert_eq!(back.next_tid, 1);
        assert_eq!(back.accounts[&a].fills[0].dir, "Close Short");
    }

    /// Touched is the registry's notion: an address with no account answers
    /// `None` on both reads (the caller passes through to the real venue); a
    /// dealt account answers `Some`, empty until it trades; the funding read
    /// filters by `start_ms`.
    #[test]
    fn an_untouched_address_is_none_a_touched_one_is_some() {
        let feed = feed_with_book(
            book(&[("100", "5")], &[("101", "5")]),
            crate::sim::meta_with("ETH", 10),
        );
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        assert!(sim.user_fills(a).is_none());
        assert!(sim.user_fundings(a, 0).is_none());
        deal(&sim, &feed, a, "100");
        assert_eq!(sim.user_fills(a), Some(Vec::new()));
        assert_eq!(sim.user_fundings(a, 0), Some(Vec::new()));
        assert!(sim.user_fills(Address::repeat_byte(0xbb)).is_none());

        sim.with_account(a, |account| {
            for hour in 1..=3 {
                record_funding(
                    &mut account.fundings,
                    UserFunding {
                        time: hour * 3_600_000,
                        coin: "ETH".into(),
                        usdc: Decimal::ONE,
                        szi: Decimal::ONE,
                        funding_rate: Decimal::ONE,
                        n_samples: None,
                    },
                );
            }
            Ok(())
        })
        .unwrap();
        let times: Vec<u64> = sim
            .user_fundings(a, 2 * 3_600_000)
            .unwrap()
            .iter()
            .map(|f| f.time)
            .collect();
        assert_eq!(times, [7_200_000, 10_800_000]);
    }
}
