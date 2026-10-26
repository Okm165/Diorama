//! The order lane's shared rules: size, tick/lot, the $10 minimum, the
//! freshness window, the oracle band, the max order value by leverage tier,
//! and the trigger side — the refusals every door answers with (the walk, the
//! resting paths, `/exchange`, the CoreWriter). Exercised through the lane's
//! tests (order.rs).
use super::order::{bbo, Order, Tif, Tpsl, Trigger};
use crate::error::{Error, Result};
use crate::feed::{BookAge, MarketFeed};
use rust_decimal::Decimal;

/// The shared order validations — size, tick/lot, the $10 minimum, and the
/// freshness window — used by the walk and every resting path.
pub(crate) fn validate_order(
    feed: &MarketFeed,
    meta: Option<&crate::wire::Meta>,
    req: &Order,
) -> Result<()> {
    // An unknown coin is a permanent refusal, not a warming one: the meta is
    // the venue's served universe, so a name it does not carry can never warm
    // up. Without this the order fell through to the book lookup and read as
    // `503 warming up` (measured) — a transient shape that invites infinite
    // retries. A *configured* market with no served data keeps the warming
    // shape: the two faults stay distinct, like the l2Book read's 200-null.
    if let Some(meta) = meta {
        if meta.asset(&req.coin).is_none() {
            return Err(Error::NotFound {
                what: format!("coin `{}` is not in the venue's meta", req.coin),
            });
        }
    }
    if req.size <= Decimal::ZERO {
        return Err(Error::bad_request("order size must be > 0"));
    }
    // A resting or post-only order needs its own price: the venue never walks
    // a Gtc/Alo as a market order (it would fill instead of rest).
    if req.limit_px.is_none() && req.trigger.is_none() && req.tif != Tif::Ioc {
        return Err(Error::bad_request("Order has invalid price."));
    }
    // The docs (delisting): "When an asset is delisted, all positions are
    // settled and open orders are cancelled ... After settlement, no new
    // orders will be accepted." (The settlement itself is the sim's
    // `settle_delisted` pass.)
    if meta
        .and_then(|m| m.asset(&req.coin))
        .and_then(|a| a.is_delisted)
        .unwrap_or(false)
    {
        return Err(Error::bad_request(
            "After settlement, no new orders will be accepted.",
        ));
    }
    validate_tick_lot(meta, req)?;
    validate_min_notional(feed, req)?;
    validate_price_band(feed, req)?;
    validate_order_value(feed, meta, req)?;
    // The freshness discipline (stale ≠ absent): a live-sourced book older
    // than the shared window is not tradable — the fill would price off a
    // snapshot the venue has already left. A scripted book is the scenario's
    // present and never stale.
    if let BookAge::Stale(age) = feed.book_age(&req.coin, crate::now_ms(), MarketFeed::MAX_STALE_MS) {
        tracing::warn!(coin = %req.coin, age_ms = age, "order refused: stale book");
        return Err(Error::bad_request(format!(
            "the {} book is stale ({age} ms since the last live update); trading on stale data is refused",
            req.coin
        )));
    }
    Ok(())
}

/// The venue's price band (docs: a limit more than 80 % away from the oracle
/// is rejected — a pre-validation class error upstream; here it rides the
/// order's own status with the same string).
pub(crate) fn validate_price_band(feed: &MarketFeed, req: &Order) -> Result<()> {
    let Some(px) = req.limit_px else {
        return Ok(());
    };
    let Some(oracle) = feed.oracle(&req.coin) else {
        return Ok(());
    };
    if oracle <= Decimal::ZERO {
        return Ok(());
    }
    let distance = (px - oracle).abs();
    let band = super::account::mul(oracle, Decimal::new(8, 1), "oracle band")?;
    if distance > band {
        return Err(Error::bad_request("Order price too far from oracle"));
    }
    Ok(())
}

/// The venue's max order value by the asset's max leverage (docs, §16.3:
/// $30M ≥25× / $5M / $2M / $500k; a limit order may be 10× the market cap).
/// The venue's exact string is not pinned — this names our rule until a live
/// probe pins theirs (docs/INTEGRATION.md).
pub(crate) fn validate_order_value(
    feed: &MarketFeed,
    meta: Option<&crate::wire::Meta>,
    req: &Order,
) -> Result<()> {
    let Some(asset) = meta.and_then(|m| m.asset(&req.coin)) else {
        return Ok(());
    };
    let market_cap = match asset.max_leverage {
        l if l >= 25 => Decimal::from(30_000_000u64),
        l if l >= 20 => Decimal::from(5_000_000u64),
        l if l >= 10 => Decimal::from(2_000_000u64),
        _ => Decimal::from(500_000u64),
    };
    let cap = if req.limit_px.is_some() {
        super::account::mul(market_cap, Decimal::from(10), "limit value cap")?
    } else {
        market_cap
    };
    let Some(px) = req
        .limit_px
        .or_else(|| req.trigger.map(|t| t.trigger_px))
        .or_else(|| feed.mark(&req.coin))
    else {
        return Ok(()); // no reference price yet: the feed is warming
    };
    let value = super::account::mul(req.size, px, "order value")?;
    if value > cap {
        return Err(Error::bad_request(format!(
            "Order value {value} exceeds the maximum for this asset's leverage (docs: $30M \
             ≥25x / $5M / $2M / $500k; a limit order may be 10x the market cap)"
        )));
    }
    Ok(())
}

/// The venue's tick/lot rules (the venue's docs): a size carries at most
/// the asset's `szDecimals` decimals; a price at most 5 significant figures
/// and at most `6 − szDecimals` decimals (an integer price is always valid).
pub(crate) fn validate_tick_lot(meta: Option<&crate::wire::Meta>, req: &Order) -> Result<()> {
    let Some(asset) = meta.and_then(|m| m.asset(&req.coin)) else {
        return Ok(());
    };
    let sz_dec = u32::from(asset.sz_decimals);
    if req.size.scale() > sz_dec {
        return Err(Error::bad_request(format!(
            "size {} has more than {sz_dec} decimals ({}'s szDecimals)",
            req.size, req.coin
        )));
    }
    let Some(px) = req.limit_px else {
        return Ok(());
    };
    if px.fract().is_zero() {
        return Ok(()); // an integer price is always valid
    }
    let max_dec = 6u32.saturating_sub(sz_dec);
    if px.scale() > max_dec {
        // The venue's tick-size refusal (its tick is 10^(6 − szDecimals)).
        tracing::warn!(coin = %req.coin, price = %px, max_dec, "order refused: tick size");
        return Err(Error::bad_request("Price must be divisible by tick size."));
    }
    if significant_digits(px) > 5 {
        tracing::warn!(coin = %req.coin, price = %px, "order refused: significant figures");
        return Err(Error::bad_request(
            "Order price has more than 5 significant figures.",
        ));
    }
    Ok(())
}

/// The venue's trigger direction rule (docs, stated for the stop/take
/// market orders): for a buy, a *stop*'s trigger sits above the mid and a
/// *take*'s below; for a sell, the reverse — a mis-sided trigger would fire
/// immediately. The mid is the served book's `(best bid + best ask) / 2`; a
/// missing or one-sided book skips the check (the feed is warming).
pub(crate) fn validate_trigger_side(feed: &MarketFeed, req: &Order, trigger: &Trigger) -> Result<()> {
    let (Some(bid), Some(ask)) = (
        bbo(feed, &req.coin, crate::sim::order::Side::Buy),
        bbo(feed, &req.coin, crate::sim::order::Side::Sell),
    ) else {
        return Ok(());
    };
    let mid = super::account::div(
        super::account::add(bid, ask, "mid price")?,
        Decimal::from(2),
        "mid price",
    )?;
    use crate::sim::order::Side;
    let ok = match (req.side(), trigger.tpsl) {
        (Side::Buy, Tpsl::Sl) | (Side::Sell, Tpsl::Tp) => trigger.trigger_px > mid,
        (Side::Buy, Tpsl::Tp) | (Side::Sell, Tpsl::Sl) => trigger.trigger_px < mid,
    };
    if !ok {
        return Err(Error::bad_request(
            "Trigger price is on the wrong side of the mid price.",
        ));
    }
    Ok(())
}

/// The venue's minimum order value: a perp order's notional (size × the
/// limit, else the mark) is at least $10. The refusal is the venue's own
/// string from the exchange docs' error sample ("Order must have minimum
/// value of $10."), so a client matching on it sees the real message.
pub(crate) fn validate_min_notional(feed: &MarketFeed, req: &Order) -> Result<()> {
    let notional = if let Some(px) = req.limit_px {
        super::account::mul(req.size, px, "order notional")?
    } else {
        let mark = feed.mark(&req.coin).ok_or(Error::Warming)?;
        super::account::mul(req.size, mark, "order notional")?
    };
    if notional < Decimal::from(10) {
        return Err(Error::bad_request("Order must have minimum value of $10."));
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
