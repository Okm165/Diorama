//! The funding lane (the docs' interval): one whole hourly payment per hour
//! boundary crossed, oracle-priced, all-or-nothing per account. The trade
//! path settles the pre-fill size first (order.rs); the automaton sweeps
//! every account (tick.rs).
use super::account::{add, credit_ledger, mul, Account};
use super::ledger::{record_funding, UserFunding};
use crate::error::Result;
use crate::feed::dex::dex_of_coin;
use crate::feed::{MarketFeed, Prices};
#[cfg(test)]
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
/// Funding is per dex (the venue keeps one clearinghouse per (user, dex)):
/// each dex has its own stamp, its payments land in its own ledger, and the
/// all-or-nothing deferral is scoped to the dex, so one unpriced coin holds
/// back only its own dex's accrual.
pub(crate) fn accrue_funding(
    account: &mut Account,
    feed: &MarketFeed,
    to_ms: u64,
) -> Result<usize> {
    let mut moved = 0;
    for dex in account.dexs_with_activity() {
        moved += accrue_dex(account, feed, &dex, to_ms)?;
    }
    Ok(moved)
}

/// Whether every position of `dex` has a live price and funding rate (the
/// all-or-nothing gate of its accrual).
fn dex_is_priced(account: &Account, feed: &MarketFeed, dex: &str) -> bool {
    account
        .positions
        .keys()
        .filter(|coin| dex_of_coin(coin) == dex)
        .all(|coin| feed.required_prices(coin).is_some() && feed.funding(coin).is_some())
}

/// One dex's accrual (see [`accrue_funding`]): the number of payments moved.
#[allow(clippy::too_many_lines)] // one dex's accrual: the boundary walk, the per-coin rate, the ledger post
fn accrue_dex(account: &mut Account, feed: &MarketFeed, dex: &str, to_ms: u64) -> Result<usize> {
    let from = account.funded_at_ms(dex);
    if from == 0 {
        account.ledger_mut(dex).funded_at_ms = to_ms;
        return Ok(0);
    }
    if to_ms <= from {
        return Ok(0);
    }
    // The boundaries strictly after the stamp, through `to_ms`: the stamp sits
    // just after the last settled boundary, so a normal tick crosses exactly
    // one. Each boundary is priced with the rate the venue published **before**
    // it (`feed.funding_before`) — the charge for hour T is the rate in force
    // at T, not the prediction the venue publishes at T+1 s. A boundary with no
    // captured pre-boundary sample (a boot, a gap) falls back to the current
    // rate, which is what the lane served before the history existed.
    let first = from / HOUR_MS + 1;
    let last = to_ms / HOUR_MS;
    if last < first {
        account.ledger_mut(dex).funded_at_ms = to_ms;
        return Ok(0);
    }
    if !dex_is_priced(account, feed, dex) {
        return Ok(0);
    }
    let mut moved = 0;
    for (coin, p) in account.positions.iter_mut() {
        if dex_of_coin(coin) != dex {
            continue;
        }
        let Some(Prices { oracle, .. }) = feed.required_prices(coin) else {
            continue;
        };
        let Some(current) = feed.funding(coin) else {
            continue;
        };
        for boundary in first..=last {
            let boundary_ms = boundary * HOUR_MS;
            // The rate the venue published before the boundary, or — named, not
            // silent — the current one when this process has no pre-boundary
            // sample (a boot, a gap; the served ledger's rate is then the
            // approximation). Funding is money: every charge is logged with the
            // rate, its source, the oracle it was priced at and the payment, so a
            // reconciliation against the real venue's `fundingHistory` reads from
            // the mirror's own log rather than a re-derivation.
            let (rate, source) = match feed.funding_at(coin, boundary_ms) {
                Some(rate) => (rate, "boundary_value"),
                None => (current, "current_fallback"),
            };
            let payment = mul(
                mul(-p.szi, oracle, "funding notional")?,
                rate,
                "funding payment",
            )?;
            tracing::info!(
                coin = %coin,
                boundary_ms,
                rate = %rate,
                rate_source = source,
                oracle = %oracle,
                szi = %p.szi,
                payment = %payment,
                "funding boundary charged"
            );
            if payment.is_zero() {
                continue;
            }
            credit_ledger(&mut account.perps, dex, payment, "funding balance")?;
            p.funding_all_time = add(p.funding_all_time, payment, "cum funding")?;
            p.funding_since_open = add(p.funding_since_open, payment, "cum funding")?;
            p.funding_since_change = add(p.funding_since_change, payment, "cum funding")?;
            record_funding(
                &mut account.fundings,
                UserFunding {
                    time: boundary_ms,
                    ..UserFunding::hourly(coin, p.szi, (payment, rate))
                },
            );
            moved += 1;
        }
    }
    account.ledger_mut(dex).funded_at_ms = to_ms;
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::account::Position;
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
            perps: crate::sim::account::default_perps(Decimal::ZERO, 3_600_000),
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
        assert_eq!(account.perp_balance(""), expected);
        assert_eq!(account.positions["BTC"].funding_since_open, expected);
    }

    /// A boundary is charged the rate published **before** it — the venue's
    /// sample that lands after the boundary is already the next hour's
    /// prediction. Measured on the live mirror (2026-11-22): the served rate at
    /// `06:59:57` (`-3.1998e-05`) was *exactly* the real venue's charged rate
    /// for `07:00`, while the first post-boundary sample was `-9.9071e-05`; the
    /// lane charged a third value because it read the ctx just after the
    /// boundary. The history now prices the elapsed hour.
    #[test]
    fn a_boundary_is_charged_the_rate_published_before_it() {
        let feed = feed_with_funding(&[("BTC", "100", "101")]);
        let mut account = funding_account();
        // The next hour's prediction lands one second after the 2 h boundary.
        let mut next = crate::wire::funding_ctx("100", "101");
        next.funding = Decimal::new(9, 4);
        feed.put_ctx("BTC".into(), next, 7_200_001);
        let moved = accrue_funding(&mut account, &feed, 7_200_001).unwrap();
        assert_eq!(moved, 1);
        let charged = -Decimal::ONE * Decimal::from(101) * Decimal::new(125, 7);
        assert_eq!(
            account.perp_balance(""),
            charged,
            "the pre-boundary rate (0.0000125), not the 0.0009 prediction"
        );
        assert_eq!(account.fundings.len(), 1);
        assert_eq!(account.fundings[0].funding_rate, Decimal::new(125, 7));
        assert_eq!(account.fundings[0].time, 7_200_000);
    }

    /// A missing coin's context defers the whole account's accrual: no silent
    /// funding loss for the priced positions while another waits.
    #[test]
    fn funding_waits_for_every_coin_to_be_priced() {
        let feed = feed_with_funding(&[("BTC", "100", "100")]);
        let mut account = funding_account();
        account.positions.insert("ETH".into(), long_btc());
        assert_eq!(accrue_funding(&mut account, &feed, 7_200_000).unwrap(), 0);
        assert_eq!(account.funded_at_ms(""), 3_600_000, "the stamp stays put");
        feed.put_ctx("ETH".into(), crate::wire::funding_ctx("100", "100"), 1);
        assert_eq!(accrue_funding(&mut account, &feed, 7_200_000).unwrap(), 2);
        assert_eq!(account.funded_at_ms(""), 7_200_000);
    }

    /// **A boundary is charged the rate in force at it, captured as the value
    /// the last write introduced.** The venue's served rate is the running
    /// estimate of the current hour's charge: constant through an hour, then
    /// re-converging in the seconds after the boundary. The measured 09:00 BTC
    /// charge was `0.0000125` — the value the feed served at 08:59:59 — but the
    /// previous design recorded the *outgoing* rate on each write, so a write
    /// that landed just before the boundary priced it with the value it had just
    /// replaced (`0.000007361`). This pins the introduced value.
    #[test]
    fn a_boundary_is_charged_the_rate_in_force_at_it_not_the_one_it_replaced() {
        let feed = feed_with_funding(&[("BTC", "100", "101")]);
        let mut account = funding_account();
        // The half hour carried 0.0000375; the last write before the boundary
        // introduces 0.0000125 (the hour's converging estimate).
        let mut higher = crate::wire::funding_ctx("100", "101");
        higher.funding = Decimal::new(375, 7);
        feed.put_ctx("BTC".into(), higher, 5_400_000);
        feed.put_ctx("BTC".into(), crate::wire::funding_ctx("100", "101"), 7_199_999);
        let moved = accrue_funding(&mut account, &feed, 7_200_000).unwrap();
        assert_eq!(moved, 1);
        assert_eq!(
            account.perp_balance(""),
            -Decimal::ONE * Decimal::from(101) * Decimal::new(125, 7),
            "the value in force at the boundary (0.0000125), not the outgoing 0.0000375"
        );
        assert_eq!(account.fundings[0].funding_rate, Decimal::new(125, 7));
    }

    /// The docs' interval is all-or-nothing: a read inside the hour moves no
    /// balance, the boundary pays the whole hourly amount.
    #[test]
    fn funding_pays_only_on_the_hour() {
        let feed = feed_with_funding(&[("BTC", "100", "101")]);
        let mut account = funding_account();
        // 59 minutes past the baseline: inside the interval, nothing moves.
        assert_eq!(accrue_funding(&mut account, &feed, 7_140_000).unwrap(), 0);
        assert_eq!(account.perp_balance(""), Decimal::ZERO);
        // The next boundary pays the whole hour, not the elapsed fraction.
        let moved = accrue_funding(&mut account, &feed, 7_200_000).unwrap();
        assert_eq!(moved, 1);
        assert_eq!(
            account.perp_balance(""),
            -Decimal::ONE * Decimal::from(101) * Decimal::new(125, 7)
        );
    }

    /// Every payment is on the ledger, stamped with its hour boundary: the
    /// signed balance delta (a long pays: negative), the signed size and the
    /// rate that priced it, one entry per boundary crossed, summing to the
    /// balance's move.
    #[test]
    fn funding_payments_are_recorded_per_boundary() {
        let feed = feed_with_funding(&[("BTC", "100", "101")]);
        let mut account = funding_account();
        // Inside the hour: nothing recorded.
        accrue_funding(&mut account, &feed, 7_140_000).unwrap();
        assert!(account.fundings.is_empty());
        // The boundary at 2 h pays once.
        accrue_funding(&mut account, &feed, 7_200_000).unwrap();
        let hourly = -Decimal::ONE * Decimal::from(101) * Decimal::new(125, 7);
        assert_eq!(account.fundings.len(), 1);
        let first = &account.fundings[0];
        assert_eq!(first.time, 7_200_000);
        assert_eq!(first.coin, "BTC");
        assert_eq!(first.usdc, hourly);
        assert_eq!(first.szi, Decimal::ONE);
        assert_eq!(first.funding_rate, Decimal::new(125, 7));
        assert_eq!(first.n_samples, None);
        // A jump over three more boundaries records each one.
        accrue_funding(&mut account, &feed, 18_000_000 + 1).unwrap();
        let times: Vec<u64> = account.fundings.iter().map(|f| f.time).collect();
        assert_eq!(times, [7_200_000, 10_800_000, 14_400_000, 18_000_000]);
        let recorded: Decimal = account.fundings.iter().map(|f| f.usdc).sum();
        assert_eq!(
            recorded, account.perp_balance(""),
            "the ledger sums to the balance"
        );
    }

    /// A deferred accrual (a coin with no live context) records nothing.
    #[test]
    fn a_deferred_accrual_records_nothing() {
        let feed = feed_with_funding(&[("BTC", "100", "100")]);
        let mut account = funding_account();
        account.positions.insert("ETH".into(), long_btc());
        accrue_funding(&mut account, &feed, 7_200_000).unwrap();
        assert!(account.fundings.is_empty());
    }
}
