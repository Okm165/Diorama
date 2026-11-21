//! `feeds` — the mirror's on-chain oracle control, the spot-plane twin of the
//! venue sim's `/control/crash|set_book`.
//!
//! Why this exists
//! ---------------
//! The venue sim owns a perp market's mark/oracle/book through `/control/*`;
//! the *spot* plane's on-chain oracles (the Chainlink-shaped feeds the risk
//! stack reads) had no symmetric control: chain 1's refresh is a manual script
//! and chain 999's feed had none, so an adverse-scenario drill could not move a
//! borrow-side price (measured 2026-11-13, DAWG-M15). This module gives both
//! engines the same verbs over one feed shape — [`MirrorPriceFeed`], the
//! runtime the refresh script installs — so a scenario owns an oracle with the
//! same semantics on T and V:
//!
//! * `crash`/`rally` move the answer by a percentage (0 < pct < 100);
//! * `set` writes an exact answer;
//! * the timestamp is the chain head (fresh by construction) and the round
//!   identity strictly increases — the indexer orders these feeds by round and
//!   treats a lower round as a regression, so a same-round rewrite would wedge
//!   the poller (measured: a static round id produced a per-second "feed round
//!   regressed" loop).
//!
//! Every write is silent (`anvil_setStorageAt`), journaled for
//! `/admin/rewind`, and marked devnet-owned so the shadow scanner cannot
//! overwrite the scenario with the fork's value.
//!
//! [`MirrorPriceFeed`]: ../daloc_monorepo/contracts/test/mocks/MirrorPriceFeed.sol
use crate::admin::AdminSurface;
use crate::error::Result;
use crate::journal::{JournalEntry, JournalSink};
use crate::plane::Plane;
use alloy_primitives::{Address, U256};
use diorama_anvil::Rpc;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// The MirrorPriceFeed storage layout the refresh script pins (slots 0/1/2/4/5).
/// Slot 2 (decimals) and 4 are set at install time and never move here.
const SLOT_ANSWER: u64 = 0;
const SLOT_TIMESTAMP: u64 = 1;
const SLOT_DECIMALS: u64 = 2;
const SLOT_ROUND: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedMode {
    Crash,
    Rally,
    Set,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeedControlRequest {
    pub feed: Address,
    pub mode: FeedMode,
    /// Crash/rally move the current answer by this percentage (0 < pct < 100).
    /// Decimal, not `f64`: the answer is an exact integer in the feed's own
    /// decimals, and a binary float would move it by a unit or two of dust
    /// (the crate's money rule applies to the arithmetic, even to a scenario
    /// control).
    #[serde(default)]
    pub pct: Option<Decimal>,
    /// `set`'s exact answer, in the feed's own decimals (8 for the USD feeds).
    #[serde(default)]
    pub answer: Option<i128>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FeedControlOutcome {
    pub ok: bool,
    pub feed: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_before: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<i128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Read a slot as its low 64 bits (answers/timestamps/rounds here are all
/// small non-negative integers; a wider word is a foreign layout and the
/// control refuses rather than guessing).
async fn read_u64(rpc: &Rpc, feed: Address, slot: u64) -> Result<Option<u64>> {
    let word = rpc.storage_at_latest(feed, U256::from(slot)).await?.0;
    Ok(decode_u64(&word))
}

/// The pure half of [`read_u64`]: a plain integer occupies the word's low
/// bytes; anything set above bit 64 is a foreign layout.
fn decode_u64(word: &[u8; 32]) -> Option<u64> {
    let high = u128::from_be_bytes(word[8..24].try_into().expect("16 bytes"));
    if high != 0 {
        return None;
    }
    Some(u64::from_be_bytes(word[24..].try_into().expect("8 bytes")))
}

/// The answer as a signed decimal (the feeds are non-negative; a negative word
/// is not a MirrorPriceFeed layout).
async fn read_answer(rpc: &Rpc, feed: Address) -> Result<Option<i128>> {
    Ok(read_u64(rpc, feed, SLOT_ANSWER).await?.map(|v| v as i128))
}

/// The feed's current answer, read through the same slots the control writes.
///
/// Exposed for the venue profile's **print sync**: a long-running sim walks its
/// own oracle, and the mirror's on-chain print must follow it inside the
/// indexer's two ingestion bounds (see `[services.venue_sim.print_sync]`). The
/// caller decides the step; this is only the read.
pub async fn read_feed_answer(rpc: &Rpc, feed: Address) -> Result<Option<i128>> {
    read_answer(rpc, feed).await
}

/// The feed's own `decimals` (slot 2, the mirror runtime's layout). `None` when
/// slot 2 is not a plain integer — a foreign layout the caller must not guess at.
pub async fn read_feed_decimals(rpc: &Rpc, feed: Address) -> Result<Option<u8>> {
    Ok(read_u64(rpc, feed, SLOT_DECIMALS).await?.and_then(|v| u8::try_from(v).ok()))
}

fn refused(feed: Address, error: impl Into<String>) -> FeedControlOutcome {
    FeedControlOutcome {
        ok: false,
        feed,
        answer_before: None,
        answer: None,
        round: None,
        error: Some(error.into()),
    }
}

/// The scenario's target answer: `set`'s exact figure, or the percentage move
/// of the current one. The pure half of [`feed_control`], so the request
/// grammar and its bounds are one testable fact.
fn scenario_answer(req: &FeedControlRequest, before: i128) -> std::result::Result<i128, String> {
    match req.mode {
        FeedMode::Set => {
            let Some(answer) = req.answer else {
                return Err("mode=set requires `answer`".to_owned());
            };
            if answer <= 0 {
                return Err(format!("answer {answer} must be positive"));
            }
            Ok(answer)
        }
        FeedMode::Crash | FeedMode::Rally => {
            let Some(pct) = req.pct else {
                return Err("crash/rally require `pct`".to_owned());
            };
            if pct <= Decimal::ZERO || pct >= Decimal::from(100) {
                return Err(format!("pct {pct} must be in (0, 100)"));
            }
            let hundred = Decimal::from(100);
            let factor = if req.mode == FeedMode::Crash {
                Decimal::ONE - pct / hundred
            } else {
                Decimal::ONE + pct / hundred
            };
            let moved = (Decimal::from(before) * factor).round();
            let Some(moved) = moved.to_i128() else {
                return Err(format!("the moved answer {moved} is not representable"));
            };
            if moved <= 0 {
                return Err(format!("the moved answer {moved} must stay positive"));
            }
            Ok(moved)
        }
    }
}

/// Own the feed: one scenario answer, fresh timestamp, strictly increasing
/// round. Fails closed — a refused or unverified write is reported, never
/// marked, so the shadow scanner keeps serving the fork's value.
pub async fn feed_control(
    surface: &AdminSurface<'_>,
    journal: Option<JournalSink<'_>>,
    req: &FeedControlRequest,
) -> Result<FeedControlOutcome> {
    let target = surface.target;
    let Some(before) = read_answer(target, req.feed).await? else {
        return Ok(refused(
            req.feed,
            "slot 0 is not a plain integer — not a MirrorPriceFeed layout; \
             install the runtime first (refresh-chain1-price-feeds.sh) or pick the feed address",
        ));
    };
    let answer = match scenario_answer(req, before) {
        Ok(answer) => answer,
        Err(error) => return Ok(refused(req.feed, error)),
    };

    let head = target.block_number().await?;
    let (_head, head_block) = target.head_and_block(head).await?;
    let head_ts = head_block.header.timestamp;
    // The round must strictly increase past whatever the feed last served and
    // past the wall clock (the refresh script uses the head timestamp; a
    // re-run after a clock correction must not go backwards).
    let prev_round = read_u64(target, req.feed, SLOT_ROUND).await?.unwrap_or(0);
    let round = prev_round.max(head_ts).max(now_unix_s()) + 1;

    let writes = [
        (SLOT_ANSWER, U256::from(answer.unsigned_abs())),
        (SLOT_TIMESTAMP, U256::from(head_ts)),
        (SLOT_ROUND, U256::from(round)),
    ];
    write_owned_slots(surface, journal, req.feed, &writes).await?;
    tracing::info!(
        feed = %req.feed,
        mode = ?req.mode,
        before,
        answer,
        round,
        "devnet feed control applied (silent write, journaled, marked devnet-owned)"
    );
    Ok(FeedControlOutcome {
        ok: true,
        feed: req.feed,
        answer_before: Some(before),
        answer: Some(answer),
        round: Some(round),
        error: None,
    })
}

/// Apply the slots silently, journal and mark each one **as it lands**.
///
/// The deal.rs contract: a refused write is never marked (a mark on a slot
/// whose write never landed would freeze the source's value under a devnet
/// mark), so the first refusal aborts. Per-slot booking makes the abort safe:
/// an earlier slot that did land is already journaled and marked, so
/// `/admin/rewind` can still undo it and the shadow scanner sees it as ours.
/// Booking after the whole loop would leave a partially applied scenario
/// invisible to both.
async fn write_owned_slots(
    surface: &AdminSurface<'_>,
    journal: Option<JournalSink<'_>>,
    feed: Address,
    writes: &[(u64, U256)],
) -> Result<()> {
    for (slot, value) in writes {
        let before_value = surface
            .target
            .storage_at_latest(feed, U256::from(*slot))
            .await?
            .0;
        let write = diorama_anvil::Write::Storage {
            address: feed,
            slot: U256::from(*slot),
            value: *value,
        };
        match diorama_anvil::apply_writes(
            surface.target,
            std::slice::from_ref(&write),
            surface.chunk_size,
        )
        .await
        {
            Ok(1) => {}
            Ok(applied) => {
                return Err(crate::error::Error::FeedControlRefused {
                    reason: format!("slot {slot} write refused ({applied} of 1 applied)"),
                });
            }
            Err(e) => {
                return Err(crate::error::Error::FeedControlRefused {
                    reason: format!("slot {slot} write failed: {e}"),
                });
            }
        }
        // The undo entry first: a crash between the two leaves a journal row
        // for a value that is already what rewind writes (harmless), never a
        // marked write the rewind cannot see.
        if let Some(sink) = journal {
            sink.journal.record(
                sink.at,
                &[JournalEntry::Storage {
                    address: feed,
                    slot: U256::from(*slot),
                    before: U256::from_be_bytes(before_value),
                }],
            )?;
        }
        mark_owned(surface.plane, &write)?;
    }
    Ok(())
}

fn mark_owned(plane: &Plane, write: &diorama_anvil::Write) -> Result<()> {
    plane.shadow.lock()?.mark(write);
    plane.save_shadow()
}

/// Wall-clock seconds: the round floor that keeps a re-run monotonic even when
/// the chain clock was corrected backwards.
fn now_unix_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const FEED: Address = address!("00000000000000000000000000000000000000fe");

    fn request(mode: FeedMode, pct: Option<&str>, answer: Option<i128>) -> FeedControlRequest {
        FeedControlRequest {
            feed: FEED,
            mode,
            pct: pct.map(|p| p.parse().expect("a decimal")),
            answer,
        }
    }

    /// Crash and rally are exact decimal moves of the feed's own integer:
    /// 4321.5 USD at 8 decimals, crashed 10 %, is 3889.35 USD — rounded to
    /// the answer's unit, never a float's approximation of it.
    #[test]
    fn crash_and_rally_move_the_answer_exactly() {
        let before = 432_150_000_000i128;
        assert_eq!(
            scenario_answer(&request(FeedMode::Crash, Some("10"), None), before),
            Ok(388_935_000_000),
        );
        assert_eq!(
            scenario_answer(&request(FeedMode::Rally, Some("10"), None), before),
            Ok(475_365_000_000),
        );
    }

    /// The grammar refuses what a scenario must not do: no negative/zero
    /// answers, no out-of-range percentage, no missing argument, and no move
    /// that rounds the feed to zero.
    #[test]
    fn the_request_grammar_refuses_the_undefined_moves() {
        let crash = |pct: &str| scenario_answer(&request(FeedMode::Crash, Some(pct), None), 100);
        assert!(crash("0").is_err());
        assert!(crash("100").is_err());
        assert!(crash("-5").is_err());
        assert!(scenario_answer(&request(FeedMode::Crash, None, None), 100).is_err());
        assert!(scenario_answer(&request(FeedMode::Set, None, None), 100).is_err());
        assert!(scenario_answer(&request(FeedMode::Set, None, Some(0)), 100).is_err());
        assert!(
            scenario_answer(&request(FeedMode::Crash, Some("99.9"), None), 1).is_err(),
            "a move that rounds to zero is refused, not written"
        );
        assert_eq!(
            scenario_answer(&request(FeedMode::Set, None, Some(42)), 100),
            Ok(42),
            "set writes the exact answer"
        );
    }

    /// The slot decoder accepts a plain integer in the low 64 bits and
    /// refuses a wider word: a foreign layout must not be read as a small
    /// answer and overwritten.
    #[test]
    fn a_slot_decodes_only_a_plain_integer() {
        let mut word = [0u8; 32];
        word[24..].copy_from_slice(&1234u64.to_be_bytes());
        assert_eq!(decode_u64(&word), Some(1234));

        let mut wider = word;
        wider[23] = 1; // bit 64: outside the plain-integer layout
        assert_eq!(decode_u64(&wider), None);
    }
}
