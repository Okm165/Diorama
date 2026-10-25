//! The funding lane (the docs' interval): one whole hourly payment per hour
//! boundary crossed, oracle-priced, all-or-nothing per account. The trade
//! path settles the pre-fill size first (order.rs); the automaton sweeps
//! every account (tick.rs).
use super::account::{add, mul, Account};
use crate::error::Result;
use crate::feed::MarketFeed;
use rust_decimal::Decimal;

/// One hour in milliseconds: the funding interval (the docs: "funding is
/// paid every hour").
const HOUR_MS: u64 = 3_600_000;

/// Accrue funding from the account's stamp to `to_ms` at the live rates:
/// the docs' interval is hourly and all-or-nothing ("the funding rate is
/// added or subtracted from the balance of contract holders at the funding
/// interval") — one whole hourly payment (`position_size · oracle_price ·
/// funding_rate`, oracle-priced) per hour boundary crossed since the last
/// accrual, nothing for a read inside the hour.
///
/// A stamp of 0 means "never accrued": the first call establishes the
/// baseline (an account must not inherit funding from the epoch's beginning).
/// All-or-nothing per account: if any position's coin has no live context,
/// the stamp stays where it is — a partially accrued account would silently
/// lose the skipped positions' funding forever. Fills accrue the pre-fill
/// size before they change positions (the trade-time settlement), so a
/// position's boundaries settle before its size moves; the rate served at
/// the accrual prices every boundary crossed since the last one (the sim
/// keeps no per-boundary rate history — the gaps table names it).
pub(crate) fn accrue_funding(account: &mut Account, feed: &MarketFeed, to_ms: u64) -> Result<usize> {
    if account.funded_at_ms == 0 {
        account.funded_at_ms = to_ms;
        return Ok(0);
    }
    let from = account.funded_at_ms;
    if to_ms <= from {
        return Ok(0);
    }
    let hours = (to_ms / HOUR_MS).saturating_sub(from / HOUR_MS);
    if hours == 0 {
        account.funded_at_ms = to_ms;
        return Ok(0);
    }
    let priced = account
        .positions
        .keys()
        .all(|coin| feed.required_prices(coin).is_some() && feed.funding(coin).is_some());
    if !priced {
        return Ok(0);
    }
    let mut moved = 0;
    for (coin, p) in account.positions.iter_mut() {
        let Some((_mark, oracle)) = feed.required_prices(coin) else {
            continue;
        };
        let Some(rate) = feed.funding(coin) else {
            continue;
        };
        let payment = mul(mul(mul(-p.szi, oracle, "funding notional")?, rate, "funding payment")?, Decimal::from(hours), "funding payment")?;
        if payment.is_zero() {
            continue;
        }
        account.perp_balance = add(account.perp_balance, payment, "funding balance")?;
        p.funding_all_time = add(p.funding_all_time, payment, "cum funding")?;
        p.funding_since_open = add(p.funding_since_open, payment, "cum funding")?;
        p.funding_since_change = add(p.funding_since_change, payment, "cum funding")?;
        moved += 1;
    }
    account.funded_at_ms = to_ms;
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use crate::sim::account::Position;
    use super::*;
    use crate::sim::feed_with_funding;
    use crate::wire::LeverageType;

    /// A long 1.0 BTC at 100, 10× cross: the funding tests' position.
    fn long_btc() -> Position {
        Position {
            szi: Decimal::ONE,
            entry_px: Decimal::from(100),
            leverage: LeverageType::Cross,
            leverage_value: 10,
            margin_used: Decimal::from(10),
            isolated_margin: Decimal::ZERO,
            funding_all_time: Decimal::ZERO,
            funding_since_open: Decimal::ZERO,
            funding_since_change: Decimal::ZERO,
        }
    }

    /// A zero-balance account stamped at the 1 h mark, holding `long_btc`.
    fn funding_account() -> Account {
        let mut account = Account {
            perp_balance: Decimal::ZERO,
            funded_at_ms: 3_600_000,
            ..Account::default()
        };
        account.positions.insert("BTC".into(), long_btc());
        account
    }

    /// Funding accrues hourly at the oracle price, and moves the balance.
    #[test]
    fn funding_accrues_hourly_on_oracle() {
        let feed = feed_with_funding(&[("BTC", "100", "101")]);
        let mut account = funding_account();
        // One hour at rate 0.0000125 (the published hourly rate) on oracle 101.
        let moved = accrue_funding(&mut account, &feed, 7_200_000).unwrap();
        assert_eq!(moved, 1);
        let expected = -Decimal::ONE * Decimal::from(101) * Decimal::new(125, 7);
        assert_eq!(account.perp_balance, expected);
        assert_eq!(account.positions["BTC"].funding_since_open, expected);
    }

    /// A missing coin's context defers the whole account's accrual: no silent
    /// funding loss for the priced positions while another waits.
    #[test]
    fn funding_waits_for_every_coin_to_be_priced() {
        let feed = feed_with_funding(&[("BTC", "100", "100")]);
        let mut account = funding_account();
        account.positions.insert("ETH".into(), long_btc());
        assert_eq!(accrue_funding(&mut account, &feed, 7_200_000).unwrap(), 0);
        assert_eq!(account.funded_at_ms, 3_600_000, "the stamp stays put");
        feed.put_ctx("ETH".into(), crate::wire::funding_ctx("100", "100"), 1);
        assert_eq!(accrue_funding(&mut account, &feed, 7_200_000).unwrap(), 2);
        assert_eq!(account.funded_at_ms, 7_200_000);
    }

    /// The docs' interval is all-or-nothing: a read inside the hour moves no
    /// balance, the boundary pays the whole hourly amount.
    #[test]
    fn funding_pays_only_on_the_hour() {
        let feed = feed_with_funding(&[("BTC", "100", "101")]);
        let mut account = funding_account();
        // 59 minutes past the baseline: inside the interval, nothing moves.
        assert_eq!(accrue_funding(&mut account, &feed, 7_140_000).unwrap(), 0);
        assert_eq!(account.perp_balance, Decimal::ZERO);
        // The next boundary pays the whole hour, not the elapsed fraction.
        let moved = accrue_funding(&mut account, &feed, 7_200_000).unwrap();
        assert_eq!(moved, 1);
        assert_eq!(
            account.perp_balance,
            -Decimal::ONE * Decimal::from(101) * Decimal::new(125, 7)
        );
    }
}
