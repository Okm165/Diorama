//! The matching lane's first slice (plan §4.5/§5, Phase 2): a devnet order
//! fills against the **real book** — the live depth the feed caches — with
//! per-level consumption (the COW analogue). The plan's identity (I3): the
//! order moves the *book*, never the *mark*; the pricing oracle moves only by
//! an explicit crash. The trader's own state moves the way the venue's would:
//! size and size-weighted entry update, a reducing fill realizes its PnL into
//! the perp balance (the measured identity `accountValue == totalRawUsd + Σ
//! signed positionValue` stays exact), and the initial margin follows
//! `|szi|·entry/leverage`.
//!
//! Scope, named rather than implied: only the IOC shape fills (a Gtc
//! remainder would have to rest — refused by name, Phase 2), the signature is
//! accepted but never verified (the devnet has no keys — the
//! anvil-impersonation analogue), and the depth is the served book's (the live
//! book minus what the devnet already consumed, so a second order sees the
//! first one's impact).
use super::Sim;
use crate::error::{Error, Result};
use crate::feed::{Consumption, MarketFeed};
use crate::wire::L2Book;
use alloy_primitives::Address;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// One order: the venue's own fields, decoded from either surface (the
/// control route's body or the `/exchange` action).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub(crate) struct Order {
    pub coin: String,
    pub is_buy: bool,
    pub size: Decimal,
    /// The limit price (absent = market: fill whatever the book gives).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_px: Option<Decimal>,
    #[serde(default)]
    pub tif: Tif,
    #[serde(default)]
    pub reduce_only: bool,
}

/// Time in force: only IOC fills (a Gtc/Alo remainder cannot rest in the
/// devnet — the resting book is Phase 2).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Tif {
    #[default]
    Ioc,
    Gtc,
}

/// What a fill took (the response shape both surfaces serve).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Fill {
    pub total_sz: Decimal,
    pub avg_px: Decimal,
    /// The levels consumed, as the feed recorded them (the ownership write).
    pub consumed: Vec<Consumption>,
    pub oid: u64,
}

impl Sim {
    /// `order`: fill `req` for `address` against the served book.
    pub(crate) fn order(&self, feed: &MarketFeed, address: Address, req: &Order) -> Result<Fill> {
        let (filled, avg_px, consumed) = walk_served_book(feed, req)?;
        let meta = feed.meta();
        let leverage = self.leverage_for(address, &req.coin, meta.as_ref())?;
        let facts = super::account::FillFacts {
            coin: req.coin.clone(),
            is_buy: req.is_buy,
            size: filled,
            px: avg_px,
            leverage,
            reduce_only: req.reduce_only,
        };
        self.land_fill(feed, address, &facts, meta.as_ref())?;
        for c in &consumed {
            feed.consume(c.clone())?;
        }
        let oid = self.next_oid()?;
        self.metrics
            .orders_filled_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.mark_dirty()?;
        Ok(Fill {
            total_sz: filled,
            avg_px,
            consumed,
            oid,
        })
    }

    /// The fill's landing: the opening check runs on a clone, so a refused
    /// order leaves no trace (all-or-nothing).
    fn land_fill(
        &self,
        feed: &MarketFeed,
        address: Address,
        facts: &super::account::FillFacts,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<()> {
        let coin = facts.coin.clone();
        self.with_account(address, |account| {
            let mut probe = account.clone();
            super::account::apply_fill(&mut probe, facts)?;
            let view = super::account::view(&probe, feed, meta)?;
            if view.margin_summary.account_value < view.margin_summary.total_margin_used {
                return Err(Error::BadRequest {
                    reason: format!(
                        "insufficient margin: the {coin} position would need {} initial margin, \
                         the account is worth {}",
                        view.margin_summary.total_margin_used, view.margin_summary.account_value
                    ),
                });
            }
            super::account::apply_fill(account, facts)
        })
    }

    /// The leverage a fill uses: the position's own, else the asset's max
    /// leverage (the venue's default for a new position), else 20 — the
    /// measured no-position default the venue itself serves (`activeAssetData`
    /// for a flat account; §16.5's probe).
    fn leverage_for(
        &self,
        address: Address,
        coin: &str,
        meta: Option<&crate::wire::Meta>,
    ) -> Result<u32> {
        if let Some(p) = self
            .state
            .lock()?
            .accounts
            .get(&address)
            .and_then(|a| a.positions.get(coin))
        {
            return Ok(p.leverage_value);
        }
        Ok(meta
            .and_then(|m| m.universe.iter().find(|a| a.name == coin))
            .map(|a| a.max_leverage)
            .filter(|l| *l > 0)
            .unwrap_or(20))
    }

    /// The next order id (the venue's own numbering; monotonic across the
    /// scenario, persisted with the state).
    fn next_oid(&self) -> Result<u64> {
        let mut state = self.state.lock()?;
        state.next_oid = state.next_oid.saturating_add(1);
        Ok(state.next_oid)
    }
}

/// Walk the served book (the live depth minus what the devnet already
/// consumed) and validate the request: only IOC fills, positive size, real
/// depth within the limit.
fn walk_served_book(
    feed: &MarketFeed,
    req: &Order,
) -> Result<(Decimal, Decimal, Vec<Consumption>)> {
    if req.tif == Tif::Gtc {
        return Err(Error::BadRequest {
            reason: "resting orders are not modeled (IOC fills only; plan §4.5, Phase 2)".into(),
        });
    }
    if req.size <= Decimal::ZERO {
        return Err(Error::BadRequest {
            reason: "order size must be > 0".into(),
        });
    }
    validate_order(feed, feed.meta().as_ref(), req)?;
    let now = crate::now_ms();
    let book = feed.book(&req.coin, now).ok_or(Error::Warming)?;
    let (filled, avg_px, consumed) = walk(&book, req)?;
    if filled <= Decimal::ZERO {
        return Err(Error::BadRequest {
            reason: format!(
                "the book has no {} depth within the limit",
                if req.is_buy { "ask" } else { "bid" }
            ),
        });
    }
    Ok((filled, avg_px, consumed))
}

/// The venue's tick/lot rules (plan §5): a size carries at most the asset's
/// `szDecimals` decimals; a price at most 5 significant figures and at most
/// `6 − szDecimals` decimals (an integer price is always valid); and a perp
/// order's notional (size × the limit, else the mark) is at least $10. The
/// refusals name the rule — the venue's exact error strings are a Phase-2 pin
/// (they need a live probe against the real venue, §16.3).
fn validate_order(
    feed: &MarketFeed,
    meta: Option<&crate::wire::Meta>,
    req: &Order,
) -> Result<()> {
    let Some(asset) = meta.and_then(|m| m.universe.iter().find(|a| a.name == req.coin)) else {
        return Ok(());
    };
    let sz_dec = u32::from(asset.sz_decimals);
    if req.size.scale() > sz_dec {
        return Err(Error::BadRequest {
            reason: format!(
                "size {} has more than {sz_dec} decimals ({}'s szDecimals)",
                req.size, req.coin
            ),
        });
    }
    if let Some(px) = req.limit_px {
        let max_dec = 6u32.saturating_sub(sz_dec);
        if !px.fract().is_zero() {
            if px.scale() > max_dec {
                return Err(Error::BadRequest {
                    reason: format!(
                        "limit price {px} has more than {max_dec} decimals (6 − {sz_dec})"
                    ),
                });
            }
            if significant_digits(px) > 5 {
                return Err(Error::BadRequest {
                    reason: format!("limit price {px} has more than 5 significant figures"),
                });
            }
        }
    }
    let notional = if let Some(px) = req.limit_px {
        super::account::mul(req.size, px, "order notional")?
    } else {
        let mark = feed.mark(&req.coin).ok_or(Error::Warming)?;
        super::account::mul(req.size, mark, "order notional")?
    };
    if notional < Decimal::from(10) {
        return Err(Error::BadRequest {
            reason: format!("order value {notional} is below the $10 minimum (perps)"),
        });
    }
    Ok(())
}

/// A price's significant digits (trailing zeros are not significant —
/// `normalize` strips them).
fn significant_digits(px: Decimal) -> u32 {
    let mut m = px.normalize().mantissa().unsigned_abs();
    let mut n = 0;
    while m > 0 {
        n += 1;
        m /= 10;
    }
    n
}

/// Walk a book in price order: asks ascending for a buy, bids descending for
/// a sell; the limit stops the walk. Returns (filled size, size-weighted
/// average, the consumed levels).
fn walk(book: &L2Book, req: &Order) -> Result<(Decimal, Decimal, Vec<Consumption>)> {
    let side = usize::from(req.is_buy);
    let mut levels = book.levels[side].clone();
    levels.sort_by(|a, b| {
        if req.is_buy {
            a.px.cmp(&b.px)
        } else {
            b.px.cmp(&a.px)
        }
    });
    let mut remaining = req.size;
    let mut notional = Decimal::ZERO;
    let mut consumed = Vec::new();
    for level in &levels {
        if remaining <= Decimal::ZERO {
            break;
        }
        if let Some(limit) = req.limit_px {
            let within = if req.is_buy {
                level.px <= limit
            } else {
                level.px >= limit
            };
            if !within {
                break;
            }
        }
        let take = remaining.min(level.sz);
        if take <= Decimal::ZERO {
            continue;
        }
        remaining -= take;
        notional = super::account::add(
            notional,
            super::account::mul(take, level.px, "fill notional")?,
            "fill total",
        )?;
        consumed.push(Consumption {
            coin: book.coin.clone(),
            side: side as u8,
            price: level.px,
            size: take,
        });
    }
    let filled = req.size - remaining;
    let avg_px = if filled.is_zero() {
        Decimal::ZERO
    } else {
        notional / filled
    };
    Ok((filled, avg_px, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{Deal, DealClass};
    use crate::wire::{Level, Meta};

    /// A book with the given (bids, asks) as (px, sz) pairs.
    fn book(coin: &str, bids: &[(&str, &str)], asks: &[(&str, &str)]) -> L2Book {
        let level = |(px, sz): &(&str, &str)| Level {
            px: px.parse().unwrap(),
            sz: sz.parse().unwrap(),
            n: 1,
        };
        L2Book {
            coin: coin.into(),
            time: 0,
            levels: [
                bids.iter().map(level).collect(),
                asks.iter().map(level).collect(),
            ],
            spread: None,
        }
    }

    /// A one-asset meta.
    fn meta_with(coin: &str, max_leverage: u32) -> Meta {
        let mut m: Meta = serde_json::from_str(r#"{"universe": []}"#).unwrap();
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

    /// A feed serving the book with a live mark (the best ask, else 1) — the
    /// margin check values positions against it.
    fn feed_with_book(book: L2Book, meta: Meta) -> MarketFeed {
        let feed = MarketFeed::memory(vec!["ETH".to_string()]);
        feed.put_meta(meta, 0);
        let mark = book.levels[1]
            .first()
            .map(|l| l.px)
            .unwrap_or(Decimal::ONE);
        feed.put_ctx(
            "ETH".into(),
            crate::wire::test_ctx(&mark.to_string(), &mark.to_string()),
            0,
        );
        feed.put_book(book, 0);
        feed
    }

    /// A sim with `amount` USDC dealt to a fresh account.
    fn funded_sim(feed: &MarketFeed, amount: &str) -> (Sim, Address) {
        let sim = Sim::memory();
        let a = Address::repeat_byte(0xaa);
        sim.deal(
            feed,
            &Deal {
                address: a,
                class: DealClass::Perp,
                coin: None,
                amount: amount.parse().unwrap(),
            },
        )
        .unwrap();
        (sim, a)
    }

    /// An IOC order on ETH.
    fn ioc(is_buy: bool, size: &str) -> Order {
        Order {
            coin: "ETH".into(),
            is_buy,
            size: size.parse().unwrap(),
            limit_px: None,
            tif: Tif::Ioc,
            reduce_only: false,
        }
    }

    /// The same, reduce-only.
    fn reduce(is_buy: bool, size: &str) -> Order {
        Order {
            reduce_only: true,
            ..ioc(is_buy, size)
        }
    }

    /// I3: the fill moves the book and the position, never the mark.
    #[test]
    fn a_fill_moves_the_book_never_the_mark() {
        let feed = feed_with_book(book("ETH", &[("100", "2")], &[("101", "2")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        let mark_before = feed.mark("ETH").unwrap();

        let fill = sim.order(&feed, a, &ioc(true, "1.5")).unwrap();
        assert_eq!(fill.total_sz, "1.5".parse().unwrap());
        assert_eq!(fill.avg_px, "101".parse().unwrap());
        // I3: the mark is untouched.
        assert_eq!(feed.mark("ETH").unwrap(), mark_before);
        // The position moved: size and the size-weighted entry.
        let account = sim.account(a).unwrap().unwrap();
        let p = account.positions.get("ETH").unwrap();
        assert_eq!(p.szi, "1.5".parse().unwrap());
        assert_eq!(p.entry_px, "101".parse().unwrap());
        // The identity the client gates on stays exact.
        let served =
            crate::sim::account::clearinghouse(&account, &feed, Some(&meta_with("ETH", 10)), 0)
                .unwrap();
        assert!(
            crate::sim::account::identity_holds(&served),
            "the identity stays exact"
        );
    }

    /// I1: only the consumed levels are owned — a second order sees the first
    /// one's impact, and an untouched level keeps its live size.
    #[test]
    fn consumption_is_level_scoped_and_accumulates() {
        let feed = feed_with_book(
            book("ETH", &[("100", "5")], &[("101", "1"), ("102", "1")]),
            meta_with("ETH", 10),
        );
        let (sim, a) = funded_sim(&feed, "10000");
        // Take the whole 101 ask; the 102 ask stays live.
        sim.order(&feed, a, &ioc(true, "1")).unwrap();
        let served = feed.book("ETH", 0).unwrap();
        assert_eq!(served.levels[1].len(), 1, "the consumed ask left the book");
        assert_eq!(served.levels[1][0].px, "102".parse().unwrap());
        assert_eq!(served.levels[0][0].sz, "5".parse().unwrap(), "the bid is live");
        assert_eq!(feed.owned_levels(), 1);
        // A second order walks into the next level.
        let fill = sim.order(&feed, a, &ioc(true, "1")).unwrap();
        assert_eq!(fill.avg_px, "102".parse().unwrap());
        assert_eq!(feed.owned_levels(), 2);
    }

    /// I2/I6: a live update refreshes the unowned levels, keeps the owned
    /// delta, and prunes the records the market moved past.
    #[test]
    fn live_updates_keep_the_delta_and_prune_stale_records() {
        let feed = feed_with_book(book("ETH", &[], &[("101", "1"), ("102", "1")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(true, "1")).unwrap();
        // The live book refreshes 101 with a bigger size: the delta persists
        // (served = live − consumed) and the untouched 102 refreshes verbatim.
        feed.put_book(book("ETH", &[], &[("101", "3"), ("102", "7")]), 10);
        let served = feed.book("ETH", 0).unwrap();
        assert_eq!(served.levels[1][0].sz, "2".parse().unwrap(), "3 live − 1 consumed");
        assert_eq!(served.levels[1][1].sz, "7".parse().unwrap());
        assert_eq!(feed.owned_levels(), 1);
        // The market moves past 101: the record is pruned, never re-owned.
        feed.put_book(book("ETH", &[], &[("102", "7")]), 20);
        assert_eq!(feed.owned_levels(), 0);
        let served = feed.book("ETH", 0).unwrap();
        assert_eq!(served.levels[1][0].px, "102".parse().unwrap());
    }

    /// I4: the book hand-back restores the live book exactly.
    #[test]
    fn hand_back_restores_the_live_book() {
        let feed = feed_with_book(book("ETH", &[], &[("101", "2")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(true, "1")).unwrap();
        assert_eq!(feed.book("ETH", 0).unwrap().levels[1][0].sz, "1".parse().unwrap());
        feed.clear_book("ETH").unwrap();
        assert_eq!(feed.book("ETH", 0).unwrap().levels[1][0].sz, "2".parse().unwrap());
        assert_eq!(feed.owned_levels(), 0);
    }

    /// A reducing fill realizes its PnL into the perp balance.
    #[test]
    fn a_reduce_realizes_pnl() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        sim.order(&feed, a, &ioc(true, "2")).unwrap();
        let before = sim.account(a).unwrap().unwrap().perp_balance;
        // Sell 1 at 100: closed 1·(100 − 101) = −1.
        sim.order(&feed, a, &reduce(false, "1")).unwrap();
        let after = sim.account(a).unwrap().unwrap().perp_balance;
        assert_eq!(after, before - Decimal::ONE, "realized −1");
        // The position is 1 long at 101, still.
        let account = sim.account(a).unwrap().unwrap();
        let p = account.positions.get("ETH").unwrap();
        assert_eq!(p.szi, Decimal::ONE);
        assert_eq!(p.entry_px, "101".parse().unwrap());
    }

    /// The limit stops the walk (both directions), and the depth check is by
    /// name.
    #[test]
    fn the_limit_stops_the_walk() {
        let feed = feed_with_book(book("ETH", &[("100", "1")], &[("101", "1")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        let mut o = ioc(true, "1");
        o.limit_px = Some("100.5".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("no ask depth"), "{err}");
        let mut o = ioc(false, "1");
        o.limit_px = Some("100.5".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("no bid depth"), "{err}");
    }

    /// The venue's tick/lot rules and the $10 minimum refuse by name; an
    /// integer price of any width is always valid.
    #[test]
    fn tick_lot_and_minimum_refuse_by_name() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 4));
        let (sim, a) = funded_sim(&feed, "1000");
        // 5 decimals > szDecimals 4.
        let err = sim.order(&feed, a, &ioc(true, "0.00001")).unwrap_err();
        assert!(format!("{err}").contains("szDecimals"), "{err}");
        // 3 decimals > 6 − 4.
        let mut o = ioc(true, "1");
        o.limit_px = Some("100.123".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("decimals"), "{err}");
        // 6 significant figures.
        let mut o = ioc(true, "1");
        o.limit_px = Some("12345.6".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("significant"), "{err}");
        // 1 × 5 = 5 < the $10 minimum.
        let mut o = ioc(true, "1");
        o.limit_px = Some("5".parse().unwrap());
        let err = sim.order(&feed, a, &o).unwrap_err();
        assert!(format!("{err}").contains("minimum"), "{err}");
        // 0.1 × 101 = 10.1 ≥ the minimum: valid.
        let mut o = ioc(true, "0.1");
        o.limit_px = Some("101".parse().unwrap());
        assert!(sim.order(&feed, a, &o).is_ok(), "a valid order fills");
    }

    /// Every refusal is by name: resting orders, reduce-only that would not
    /// reduce, and an opening the account cannot margin.
    #[test]
    fn refusals_are_named() {
        let feed = feed_with_book(book("ETH", &[("100", "5")], &[("101", "5")]), meta_with("ETH", 10));
        let (sim, a) = funded_sim(&feed, "1000");
        // Gtc: a resting remainder cannot exist in the devnet.
        let mut gtc = ioc(true, "1");
        gtc.tif = Tif::Gtc;
        let err = sim.order(&feed, a, &gtc).unwrap_err();
        assert!(format!("{err}").contains("resting orders are not modeled"), "{err}");
        // A reduce-only order with nothing to reduce.
        let err = sim.order(&feed, a, &reduce(false, "1")).unwrap_err();
        assert!(format!("{err}").contains("would not reduce"), "{err}");
        // An unfunded opening: the numbers ride in the refusal.
        let b = Address::repeat_byte(0xbb);
        let err = sim.order(&feed, b, &ioc(true, "5")).unwrap_err();
        assert!(format!("{err}").contains("insufficient margin"), "{err}");
    }
}
