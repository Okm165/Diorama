//! Collateral transfers between an account's wallets: the spot wallet and the
//! perp clearinghouse (`sendAsset`, HTTP and CoreWriter action 13), the sends
//! to other accounts (`spotSend`, `usdSend`, CoreWriter action 6), and the
//! Core to EVM leg.
//!
//! The real venue keeps one clearinghouse per (user, perp dex) and a spot
//! wallet. Facts (the official SDK's `send_asset` and the docs): dex `""` is
//! the default perp dex, `"spot"` the spot wallet, any other name a builder
//! dex; and the token sent must match the collateral token of a perp-dex
//! endpoint.
//!
//! Our sim's account plane is the USDC clearinghouse (one perp wallet for the
//! default dex and every USDC-collateral builder dex — the per-dex ledger is
//! the next account-plane step). A USDH-collateral dex is therefore outside
//! what the account can express and is refused by name; the spot wallet, the
//! token registry and the ledger rows are the real shapes.
//!
//! Bounds: the perp wallet gives up what its clearinghouse lets it withdraw
//! (the served `withdrawable`: the margin in use stays — the docs' transfer
//! floor), a unified account's spot wallet what is left after the maintenance
//! its positions hold on every dex margining against the token (and never what
//! a resting spot order holds).
//!
//! Fees ([`super::fees`]): an account's first outbound transfer to another
//! account pays the 1 USDC activation fee on top of the amount (Circle's rule;
//! the debt is set when the account first receives funds), and a Core to EVM
//! transfer costs the HyperEVM gas.
use super::account::{self, Account};
use super::fees::{self, Paid, RowKind, TransferRow, ACTIVATION_FEE, QUOTE_TOKEN};
use super::{stamp_funding_baseline, Applied, Sim, SimState, TokenRef};
use crate::error::{Error, Result};
use crate::feed::MarketFeed;
use alloy_primitives::Address;
use rust_decimal::Decimal;

/// One end of a transfer: the spot wallet or the perp clearinghouse. The
/// dex name is the venue's `sourceDex` / `destinationDex`; the sim's perp
/// cash is one USDC ledger, so every `Perp` name reads the same balance until
/// the per-dex ledger lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wallet {
    Spot,
    /// A perp dex by name (`""` = the default dex). Constructed by the
    /// transfer tests and by the `/exchange` action dispatch; the sim's one
    /// perp wallet reads every name as the same cash.
    Perp(String),
}

/// The `dex` string of the venue's `sendAsset` for the spot wallet.
pub(crate) const SPOT_DEX_NAME: &str = "spot";

impl Wallet {
    /// The wallet the venue's `sourceDex` / `destinationDex` string names.
    pub(crate) fn of_dex_name(name: &str) -> Self {
        if name == SPOT_DEX_NAME {
            Wallet::Spot
        } else {
            Wallet::Perp(name.to_string())
        }
    }

    /// The `sourceDex` / `destinationDex` string of this wallet.
    fn dex_name(&self) -> &str {
        match self {
            Wallet::Spot => SPOT_DEX_NAME,
            Wallet::Perp(dex) => dex,
        }
    }

    fn describe(&self) -> String {
        match self {
            Wallet::Spot => "the spot wallet".to_string(),
            Wallet::Perp(dex) if dex.is_empty() => "the default perp dex".to_string(),
            Wallet::Perp(dex) => format!("perp dex `{dex}`"),
        }
    }
}

/// One `sendAsset` applied: `amount` of `token` from `from`'s `source` wallet
/// to `to`'s `destination` wallet.
pub(crate) struct AssetSend<'a> {
    pub from: Address,
    pub to: Address,
    pub token: TokenRef<'a>,
    pub amount: Decimal,
    pub source: Wallet,
    pub destination: Wallet,
    /// The ledger row the action writes (`spotSend` a `spotTransfer`, the
    /// others a `send`).
    pub kind: RowKind,
}

/// A refusal in the sim's own voice (the transfer actions' texts are not
/// captured from the real venue).
fn refuse(reason: impl Into<String>) -> Error {
    Error::bad_request(format!("venue-sim: {}", reason.into()))
}

/// The shape rules that need no account: a positive amount, known dexes, and
/// the token matching every perp endpoint's collateral.
fn check_shape(send: &AssetSend<'_>, feed: &MarketFeed) -> Result<()> {
    if send.amount <= Decimal::ZERO {
        return Err(refuse("the transfer amount must be positive"));
    }
    for wallet in [&send.source, &send.destination] {
        let Wallet::Perp(dex) = wallet else {
            continue;
        };
        if feed.meta_of(dex).is_none() {
            return Err(refuse(format!(
                "perp dex `{dex}` is not in the venue's dex registry"
            )));
        }
        let collateral = feed.collateral_token_of(dex);
        if collateral != send.token.index {
            return Err(refuse(format!(
                "token {} is not the collateral token of {} ({collateral}); the token sent must \
                 match the collateral token of a perp dex endpoint",
                send.token.coin,
                wallet.describe()
            )));
        }
        // One perp wallet, denominated in the quote token: a dex margining
        // against another token needs the per-dex clearinghouse ledger.
        if collateral != QUOTE_TOKEN.index {
            return Err(refuse(format!(
                "{} margins against token {collateral}; the sim models the USDC \
                 clearinghouse only",
                wallet.describe()
            )));
        }
    }
    if send.from == send.to && send.source == send.destination {
        return Err(refuse("the source and the destination are the same wallet"));
    }
    Ok(())
}

/// What the spot wallet may give up of `token` now: the balance less what a
/// resting order holds, and for a unified account less the maintenance its
/// positions hold on every dex margining against the token.
pub(crate) fn spot_free(account: &Account, feed: &MarketFeed, token: u32) -> Result<Decimal> {
    let held = match account.spot.get(&token) {
        Some(e) => account::sub(e.total, e.hold.max(Decimal::ZERO), "spot hold")?,
        None => Decimal::ZERO,
    };
    Ok(
        match account::spot_bridge_bound(account, feed, feed.margin_meta().as_ref(), token)? {
            Some(free) => held.min(free),
            None => held,
        },
    )
}

/// What the source wallet may give up now (see the module docs).
fn source_bound(
    account: &Account,
    feed: &MarketFeed,
    wallet: &Wallet,
    token: u32,
) -> Result<Decimal> {
    match wallet {
        Wallet::Spot => spot_free(account, feed, token),
        // The venue's bound is the clearinghouse's `withdrawable`: the margin
        // the open positions use stays (the docs' transfer floor). Our account
        // plane has the one USDC clearinghouse, so the account's figure is the
        // wallet's. A mark not served yet is a refusal, never a guess.
        Wallet::Perp(_) => match account::view(account, feed, feed.margin_meta().as_ref()) {
            Ok(view) => Ok(view.withdrawable),
            Err(Error::Warming) => Err(refuse(
                "the marks behind the perp wallet's positions are not served yet",
            )),
            Err(e) => Err(e),
        },
    }
}

/// The destination can take `amount` without overflowing its decimal range
/// (checked before anything moves, so a refusal never half applies).
fn check_headroom(
    account: Option<&Account>,
    wallet: &Wallet,
    token: u32,
    amount: Decimal,
) -> Result<()> {
    let current = match (account, wallet) {
        (None, _) => Decimal::ZERO,
        (Some(a), Wallet::Spot) => a.spot.get(&token).map_or(Decimal::ZERO, |e| e.total),
        (Some(a), Wallet::Perp(_)) => a.perp_balance,
    };
    account::add(current, amount, "asset send")?;
    Ok(())
}

/// The source side can pay `amount` of the token and the activation `due`: a
/// quote-token send pays both from the source wallet, any other token pays
/// the fee from the spot wallet's USDC.
fn check_funds(
    source: &Account,
    feed: &MarketFeed,
    send: &AssetSend<'_>,
    due: Decimal,
) -> Result<()> {
    let bound = source_bound(source, feed, &send.source, send.token.index)?;
    let quote = send.token.index == QUOTE_TOKEN.index;
    let need = if quote {
        send.amount + due
    } else {
        send.amount
    };
    if need > bound {
        let fee = if quote && due > Decimal::ZERO {
            format!(" plus the {due} USDC activation fee")
        } else {
            String::new()
        };
        return Err(refuse(format!(
            "the transfer of {} {}{fee} exceeds what {} may send now ({bound})",
            send.amount,
            send.token.coin,
            send.source.describe()
        )));
    }
    if !quote && due > Decimal::ZERO {
        let free = spot_free(source, feed, QUOTE_TOKEN.index)?;
        if due > free {
            return Err(refuse(format!(
                "an account's first outbound transfer also costs the {due} USDC activation \
                 fee, and the spot wallet has {free} USDC free"
            )));
        }
    }
    Ok(())
}

/// Take the amount (and the quote-token fee) out of the sender's wallets.
fn debit_sender(state: &mut SimState, send: &AssetSend<'_>, due: Decimal) -> Result<()> {
    let quote = send.token.index == QUOTE_TOKEN.index;
    let out = if quote {
        send.amount + due
    } else {
        send.amount
    };
    let from = state.accounts.entry(send.from).or_default();
    match &send.source {
        Wallet::Spot => {
            let entry = account::spot_entry(from, send.token);
            entry.total = account::sub(entry.total, out, "asset send")?;
        }
        Wallet::Perp(_) => {
            from.perp_balance = account::sub(from.perp_balance, out, "asset send")?;
        }
    }
    if !quote && due > Decimal::ZERO {
        let entry = account::spot_entry(from, QUOTE_TOKEN);
        entry.total = account::sub(entry.total, due, "activation fee")?;
    }
    Ok(())
}

/// Put the full amount into the recipient's wallet.
fn credit_recipient(state: &mut SimState, send: &AssetSend<'_>) -> Result<()> {
    let to = state.accounts.entry(send.to).or_default();
    match &send.destination {
        Wallet::Spot => {
            account::credit_entry(to, send.token, send.amount, "asset send")?;
        }
        Wallet::Perp(_) => {
            to.perp_balance = account::add(to.perp_balance, send.amount, "asset send")?;
        }
    }
    Ok(())
}

/// Apply one send to the state (`Sim::send_asset`'s body, under the lock).
fn apply_send(
    state: &mut SimState,
    send: &AssetSend<'_>,
    feed: &MarketFeed,
    clock: u64,
) -> Result<()> {
    let due = state.activation_due(send.from, send.to);
    let new_recipient = send.from != send.to && !state.exists(send.to);
    let source = state
        .accounts
        .get(&send.from)
        .ok_or_else(|| refuse(format!("account {} is not a venue-sim account", send.from)))?;
    check_funds(source, feed, send, due)?;
    check_headroom(
        state.accounts.get(&send.to),
        &send.destination,
        send.token.index,
        send.amount,
    )?;
    debit_sender(state, send, due)?;
    if let Some(from) = state.accounts.get_mut(&send.from) {
        stamp_funding_baseline(from, clock);
    }
    credit_recipient(state, send)?;
    if let Some(to) = state.accounts.get_mut(&send.to) {
        stamp_funding_baseline(to, clock);
    }
    state.settle_activation((send.from, send.to), new_recipient);
    let paid = Paid {
        fee: due,
        fee_token: if due > Decimal::ZERO {
            QUOTE_TOKEN.coin.to_string()
        } else {
            String::new()
        },
        native: Decimal::ZERO,
    };
    let usd = fees::token_usd(feed, send.token.index).unwrap_or(Decimal::ZERO);
    let row = TransferRow {
        kind: send.kind,
        user: send.from,
        destination: send.to,
        dexes: (send.source.dex_name(), send.destination.dex_name()),
        token: send.token.coin,
        amount: send.amount,
        usdc_value: send.amount * usd,
        paid: &paid,
        nonce: clock,
    };
    state.record_ledger(&[send.from, send.to], clock, &row.delta());
    Ok(())
}

/// One Core to EVM transfer: `amount` of `token` from `from`'s spot wallet to
/// the token's system address `destination` (the bridge completes it on the
/// EVM side).
pub(crate) struct CoreToEvm<'a> {
    pub from: Address,
    pub destination: Address,
    pub token: TokenRef<'a>,
    pub amount: Decimal,
    pub kind: RowKind,
}

/// The wallet debits of a Core to EVM transfer, summed per token, and what
/// the row records as paid.
struct Costs {
    debits: Vec<(u32, String, Decimal)>,
    paid: Paid,
    /// The activation was charged in another token than the row's fee token.
    activation_aside: bool,
    /// The USDC value of the amount sent (the row's `usdcValue`).
    usd: Decimal,
}

impl Costs {
    fn add(&mut self, token: TokenRef<'_>, amount: Decimal) {
        match self.debits.iter_mut().find(|(i, _, _)| *i == token.index) {
            Some((_, _, sum)) => *sum += amount,
            None => self
                .debits
                .push((token.index, token.coin.to_string(), amount)),
        }
    }
}

/// The HYPE gas of a Core to EVM transfer: nothing for HYPE itself (every real
/// row shows none), HYPE from the sender's HYPE when it covers it, otherwise
/// the token's equivalent at the spot prices; not charged when unpriced.
fn charge_gas(account: &Account, feed: &MarketFeed, token: TokenRef<'_>, costs: &mut Costs) {
    if token.coin == fees::GAS_TOKEN {
        return;
    }
    let gas = fees::core_to_evm_gas_hype();
    let hype = feed.token_of(fees::GAS_TOKEN);
    if let Some(index) = hype {
        if spot_free(account, feed, index).is_ok_and(|free| free >= gas) {
            let coin = TokenRef {
                index,
                coin: fees::GAS_TOKEN,
            };
            costs.add(coin, gas);
            costs.paid.native = gas;
            return;
        }
    }
    let priced = hype
        .and_then(|h| fees::token_usd(feed, h))
        .zip(fees::token_usd(feed, token.index));
    let Some((hype_usd, token_usd)) = priced.filter(|(_, t)| !t.is_zero()) else {
        tracing::warn!(
            token = token.coin,
            "Core to EVM gas not charged: no HYPE or token price is served"
        );
        return;
    };
    let fee = gas * hype_usd / token_usd;
    costs.add(token, fee);
    costs.paid.fee += fee;
    costs.paid.fee_token = token.coin.to_string();
}

/// The activation a deposited account still owes, charged on its first
/// outbound transfer.
fn charge_owed_activation(token: TokenRef<'_>, costs: &mut Costs) {
    costs.add(QUOTE_TOKEN, ACTIVATION_FEE);
    if token.index == QUOTE_TOKEN.index || costs.paid.fee_token.is_empty() {
        costs.paid.fee += ACTIVATION_FEE;
        costs.paid.fee_token = QUOTE_TOKEN.coin.to_string();
    } else {
        costs.activation_aside = true;
    }
}

/// Plan a Core to EVM transfer: its gas and the owed activation.
fn core_to_evm_costs(account: &Account, feed: &MarketFeed, t: &CoreToEvm<'_>, owed: bool) -> Costs {
    let mut costs = Costs {
        debits: Vec::new(),
        paid: Paid::default(),
        activation_aside: false,
        usd: t.amount * fees::token_usd(feed, t.token.index).unwrap_or(Decimal::ZERO),
    };
    costs.add(t.token, t.amount);
    charge_gas(account, feed, t.token, &mut costs);
    if owed {
        charge_owed_activation(t.token, &mut costs);
    }
    costs
}

/// What stops the account paying every debit (summed per token) now, if
/// anything.
fn shortfall(account: &Account, feed: &MarketFeed, costs: &Costs) -> Result<Option<String>> {
    for (index, coin, need) in &costs.debits {
        let free = spot_free(account, feed, *index)?;
        if *need > free {
            return Ok(Some(format!(
                "{need} {coin} is needed (the amount and the fees) and {free} is free"
            )));
        }
    }
    Ok(None)
}

impl Sim {
    /// Apply one `sendAsset` (module docs). `Err(BadRequest)` is a refusal
    /// carrying its reason (nothing moved); any other error is a fault.
    pub(crate) fn send_asset(&self, send: &AssetSend<'_>, feed: &MarketFeed) -> Result<()> {
        check_shape(send, feed)?;
        let clock = self.clock(crate::now_ms())?;
        self.with_state(|state| apply_send(state, send, feed, clock))?;
        self.touch();
        Ok(())
    }

    /// Core spot send (CoreWriter action 6, `spotSend`): the spot wallet to
    /// another account's, with the same rules as `sendAsset`. `Refused` when
    /// the rules refuse it (logged with the reason).
    pub(crate) fn spot_send(
        &self,
        (from, to): (Address, Address),
        token: TokenRef<'_>,
        amount: Decimal,
        feed: &MarketFeed,
    ) -> Result<Applied> {
        let asset = AssetSend {
            from,
            to,
            token,
            amount,
            source: Wallet::Spot,
            destination: Wallet::Spot,
            kind: RowKind::SpotTransfer,
        };
        match self.send_asset(&asset, feed) {
            Ok(()) => Ok(Applied::Applied),
            Err(Error::BadRequest { reason }) => {
                tracing::warn!(account = %from, %reason, "spot send refused");
                Ok(Applied::Refused)
            }
            Err(e) => Err(e),
        }
    }

    /// Core to EVM: debit the sender's spot wallet for the amount, the HyperEVM
    /// gas and any owed activation, and write the ledger row. `Refused` (the
    /// reason logged) when the wallet cannot pay; nothing moves then. The
    /// bridge completes the transfer on the EVM side.
    pub(crate) fn debit_core_to_evm(
        &self,
        t: &CoreToEvm<'_>,
        feed: &MarketFeed,
    ) -> Result<Applied> {
        let clock = self.clock(crate::now_ms())?;
        let applied = self.with_state(|state| {
            let Some(account) = state.accounts.get(&t.from) else {
                tracing::warn!(account = %t.from, "Core to EVM refused: unknown account");
                return Ok(Applied::Refused);
            };
            let owed = state.activation_owed.contains(&t.from);
            let costs = core_to_evm_costs(account, feed, t, owed);
            if let Some(why) = shortfall(account, feed, &costs)? {
                tracing::warn!(account = %t.from, token = t.token.coin, "Core to EVM refused: {why}");
                return Ok(Applied::Refused);
            }
            book_core_to_evm(state, t, &costs, clock)?;
            Ok(Applied::Applied)
        })?;
        if matches!(applied, Applied::Applied) {
            self.touch();
        }
        Ok(applied)
    }

    /// EVM to Core: credit a deposit to `address`'s spot wallet and write the
    /// system address's ledger row. A deposit that creates the account is
    /// credited in full; its activation is owed on its first outbound
    /// transfer (Circle's rule), and the address exists from now on.
    pub(crate) fn credit_evm_deposit(
        &self,
        address: Address,
        token: TokenRef<'_>,
        amount: Decimal,
        feed: &MarketFeed,
    ) -> Result<()> {
        self.credit_evm_deposit_to((address, &Wallet::Spot), token, amount, feed)
    }

    /// [`Self::credit_evm_deposit`] into `wallet`: the spot wallet for a plain
    /// transfer to the system address, the perp clearinghouse when the
    /// CoreDepositWallet call named a `destinationDex`.
    pub(crate) fn credit_evm_deposit_to(
        &self,
        (address, wallet): (Address, &Wallet),
        token: TokenRef<'_>,
        amount: Decimal,
        feed: &MarketFeed,
    ) -> Result<()> {
        let clock = self.clock(crate::now_ms())?;
        self.with_state(|state| {
            if !state.exists(address) {
                state.activation_owed.insert(address);
            }
            let account = state.accounts.entry(address).or_default();
            stamp_funding_baseline(account, clock);
            match wallet {
                Wallet::Spot => {
                    account::credit_entry(account, token, amount, "spot credit")?;
                }
                Wallet::Perp(_) => {
                    account.perp_balance =
                        account::add(account.perp_balance, amount, "perp credit")?;
                }
            }
            state.activated.insert(address);
            let system = if token.coin == fees::GAS_TOKEN {
                crate::bridge::system::HYPE_SYSTEM
            } else {
                crate::bridge::system::system_address(u64::from(token.index))
            };
            let row = TransferRow {
                // A spot credit is the spot transfer from the system address;
                // a perp credit is a send into the perp wallet.
                kind: match wallet {
                    Wallet::Spot => RowKind::SpotTransfer,
                    Wallet::Perp(_) => RowKind::Send,
                },
                user: system,
                destination: address,
                dexes: (SPOT_DEX_NAME, wallet.dex_name()),
                token: token.coin,
                amount,
                usdc_value: amount * fees::token_usd(feed, token.index).unwrap_or(Decimal::ZERO),
                paid: &Paid::default(),
                nonce: clock,
            };
            state.record_ledger(&[address], clock, &row.delta());
            Ok(())
        })?;
        self.touch();
        Ok(())
    }
}

/// Debit a planned Core to EVM transfer and write its rows.
fn book_core_to_evm(
    state: &mut SimState,
    t: &CoreToEvm<'_>,
    costs: &Costs,
    clock: u64,
) -> Result<()> {
    let from = state.accounts.entry(t.from).or_default();
    for (index, coin, sum) in &costs.debits {
        let token = TokenRef {
            index: *index,
            coin,
        };
        let entry = account::spot_entry(from, token);
        entry.total = account::sub(entry.total, *sum, "Core to EVM")?;
    }
    stamp_funding_baseline(from, clock);
    state.activation_owed.remove(&t.from);
    state.activated.insert(t.from);
    let row = TransferRow {
        kind: t.kind,
        user: t.from,
        destination: t.destination,
        dexes: (SPOT_DEX_NAME, SPOT_DEX_NAME),
        token: t.token.coin,
        amount: t.amount,
        usdc_value: costs.usd,
        paid: &costs.paid,
        nonce: clock,
    };
    state.record_ledger(&[t.from], clock, &row.delta());
    if costs.activation_aside {
        let aside = serde_json::json!({
            "type": "accountActivationGas",
            "amount": fees::real_text(ACTIVATION_FEE),
            "token": QUOTE_TOKEN.coin,
        });
        state.record_ledger(&[t.from], clock, &aside);
    }
    Ok(())
}
