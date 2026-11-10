//! The Chainlink aggregator wire: the ABI surface the bridge reads and writes
//! (the devnet's mock aggregators speak the real interface) and
//! the typed calls over it. One home, so the wire cannot drift between the
//! read and the write side.
use crate::error::{Error, Result};
use alloy_primitives::{Address, I256};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{sol, SolCall};
use diorama_config::FeedSection;

sol! {
    /// The Chainlink aggregator surface the bridge needs (their mocks speak
    /// the same interface as the real feeds, §7.1).
    interface AggregatorV3Interface {
        function latestRoundData() external view returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound);
        function decimals() external view returns (uint8);
        function updateAnswer(int256 _answer) external;
    }
}

/// The target's decimals when its code answers; `None` while the target is
/// not installed (an empty return does not decode: the *fact* is "no code
/// yet"; a transport fault still surfaces).
pub(crate) async fn read_target_decimals(
    rpc: &diorama_anvil::Rpc,
    aggregator: Address,
) -> Result<Option<u8>> {
    match rpc
        .call_decode(aggregator, &AggregatorV3Interface::decimalsCall {})
        .await
    {
        Ok(decoded) => Ok(Some(decoded)),
        Err(diorama_anvil::Error::Abi { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) async fn read_decimals(rpc: &diorama_anvil::Rpc, aggregator: Address) -> Result<u8> {
    Ok(rpc
        .call_decode(aggregator, &AggregatorV3Interface::decimalsCall {})
        .await?)
}

/// The target's decimals, when it exists and matches the source. `None` while
/// the fixture deploy has not run yet — *absence* is a fact the bridge waits
/// on; a present-but-mismatched target or an unexpected fault is a boot error
/// naming itself (a mis-scaled answer poisons consumers).
pub(crate) async fn target_decimals(
    anvil_t: &diorama_anvil::Rpc,
    feed: &FeedSection,
    source: u8,
) -> Result<Option<u8>> {
    match read_decimals(anvil_t, feed.target).await {
        Ok(d) if d == source => Ok(Some(d)),
        Ok(d) => Err(Error::FeedBridgeDecimals {
            feed: feed.name.clone(),
            have: source,
            want: d,
        }),
        Err(Error::FeedBridgeAbi { .. }) => {
            tracing::warn!(
                feed = %feed.name,
                target = %feed.target,
                "feed target has no code yet; the bridge waits for the devnet's fixture deploy"
            );
            Ok(None)
        }
        Err(e) => Err(Error::FeedBridgeFeed {
            feed: feed.name.clone(),
            what: "target decimals",
            source: Box::new(e),
        }),
    }
}

pub(crate) async fn read_answer(rpc: &diorama_anvil::Rpc, aggregator: Address) -> Result<i128> {
    Ok(read_answer_with_stamp(rpc, aggregator).await?.0)
}

/// The answer and the source round's own `updatedAt`.
///
/// The stamp is the **heartbeat** signal: Chainlink republishes a round on its
/// heartbeat even when the answer has not moved. The bridge must propagate that
/// publication, not just answer changes, or a guard feed with a 2 h max-age goes
/// stale in a quiet market even though its source is alive. The target mock's
/// stamp is the block that mined the bridge's write; the source stamp is the
/// parent-chain round's, so the two are compared only through the bridge's own
/// last-published stamp, never against each other.
pub(crate) async fn read_answer_with_stamp(
    rpc: &diorama_anvil::Rpc,
    aggregator: Address,
) -> Result<(i128, u64)> {
    let decoded = rpc
        .call_decode(aggregator, &AggregatorV3Interface::latestRoundDataCall {})
        .await?;
    let answer = decoded.answer.try_into().map_err(|_| Error::FeedBridgeAbi {
        what: "latestRoundData() answer",
        detail: "does not fit i128".into(),
    })?;
    let stamp = decoded.updatedAt.try_into().map_err(|_| Error::FeedBridgeAbi {
        what: "latestRoundData() updatedAt",
        detail: "does not fit u64".into(),
    })?;
    Ok((answer, stamp))
}

pub(crate) async fn publish(
    rpc: &diorama_anvil::Rpc,
    aggregator: Address,
    writer: Address,
    answer: i128,
) -> Result<()> {
    let call = AggregatorV3Interface::updateAnswerCall {
        _answer: I256::try_from(answer).map_err(|_| Error::FeedBridgeAbi {
            what: "updateAnswer(int256)",
            detail: format!("answer {answer} does not fit int256"),
        })?,
    };
    let tx = TransactionRequest::default()
        .from(writer)
        .to(aggregator)
        .input(call.abi_encode().into());
    rpc.send_transaction(tx).await?;
    Ok(())
}
