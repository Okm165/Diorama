//! Rebuilding books from the venue's `l2` feed.
//!
//! The plain `l2Book` websocket channel is throttled by the venue (20 levels
//! every 5 to 12 s since mid 2026), so a book relayed from it lags a trading
//! UI badly. The `l2` channel is what the venue's own app uses: one snapshot
//! (`{"s": {coin, time, levels}}`), then a diff about every half second, each
//! either deflate-compressed (`{"c": base64}`, raw deflate) or plain
//! (`{"u": diff}`). A diff is
//! `{"c": coin, "t": time, "l": [[{p, s}], [{p, s}]], "r": [[idx], [idx]]}`:
//! `l` are the levels whose size is new or changed (by price), `r` the indexes
//! in the previous side list of the levels that are gone. Applying it is
//! "remove the `r` indexes (highest first, the venue sometimes lists them
//! ascending), upsert `l` by price, sort, keep 20", which reproduced 42 of 42
//! real `l2Book` snapshots exactly. A diff carries no order counts, so a new
//! level is counted as one order and a resized one keeps its count.
use crate::wire::{L2Book, Level};
use base64::Engine;
use flate2::read::DeflateDecoder;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::str::FromStr;

/// The depth the venue serves per side.
#[allow(dead_code, reason = "the ws lane uses this when it subscribes the l2 channel (next port step)")]
const DEPTH: usize = 20;

/// The books a socket carries, by coin: each `l2` frame is applied to the
/// coin's last book. A reconnect starts from nothing (the venue sends a fresh
/// snapshot on subscribe), so this lives in the session, never across one.
#[derive(Default)]
#[allow(dead_code, reason = "constructed by the ws lane when it carries an l2 stream (next port step)")]
pub(crate) struct Rebuilder {
    books: BTreeMap<String, L2Book>,
}

impl Rebuilder {
    /// Fold one `l2` frame's `data` in: the coin's updated book, or `None`
    /// for a frame that cannot apply (a diff for a coin with no snapshot, an
    /// undecodable payload). Nothing but the snapshot starts a book.
    #[allow(dead_code, reason = "called by the ws lane when it carries an l2 stream (next port step)")]
    pub(crate) fn apply(&mut self, data: &Value) -> Option<L2Book> {
        if let Some(snapshot) = data.get("s") {
            let book: L2Book = serde_json::from_value(snapshot.clone()).ok()?;
            self.books.insert(book.coin.clone(), book.clone());
            return Some(book);
        }
        let diff = if let Some(plain) = data.get("u") {
            plain.clone()
        } else {
            serde_json::from_str(&inflate(data.get("c")?.as_str()?)?).ok()?
        };
        let coin = diff.get("c")?.as_str()?;
        let book = self.books.get_mut(coin)?;
        apply_diff(book, &diff)?;
        Some(book.clone())
    }

    /// Drop a coin's book (its stream was unsubscribed).
    #[allow(dead_code, reason = "called by the ws lane when it carries an l2 stream (next port step)")]
    pub(crate) fn forget(&mut self, coin: &str) {
        self.books.remove(coin);
    }
}

/// The coin an `l2` frame is about, whatever its encoding (a snapshot names it
/// in `s`, a plain diff in `u`, a compressed one inside its payload).
#[allow(dead_code, reason = "called by the ws lane to route an l2 frame (next port step)")]
pub(crate) fn frame_coin(data: &Value) -> Option<String> {
    if let Some(coin) = data.pointer("/s/coin").or_else(|| data.pointer("/u/c")) {
        return coin.as_str().map(str::to_string);
    }
    let diff: Value = serde_json::from_str(&inflate(data.get("c")?.as_str()?)?).ok()?;
    diff.get("c")?.as_str().map(str::to_string)
}

/// Raw-deflate JSON inside base64 (what the venue's compressed frames carry).
fn inflate(b64: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let mut out = String::new();
    DeflateDecoder::new(bytes.as_slice())
        .read_to_string(&mut out)
        .ok()?;
    Some(out)
}

/// Apply one decoded diff to its coin's book; `None` when the diff is not the
/// documented shape (the book is then left as it was).
fn apply_diff(book: &mut L2Book, diff: &Value) -> Option<()> {
    let upserts = diff.get("l")?.as_array()?;
    let removals = diff.get("r")?.as_array()?;
    let time = diff.get("t").and_then(Value::as_u64);
    // Parse everything first so a malformed diff changes nothing.
    let mut parsed = Vec::with_capacity(2);
    for side in 0..2 {
        let mut set = Vec::new();
        for level in upserts.get(side)?.as_array()? {
            let px = Decimal::from_str(level.get("p")?.as_str()?).ok()?;
            let sz = Decimal::from_str(level.get("s")?.as_str()?).ok()?;
            set.push((px, sz));
        }
        let mut gone: Vec<usize> = removals
            .get(side)?
            .as_array()?
            .iter()
            .filter_map(|i| usize::try_from(i.as_u64()?).ok())
            .collect();
        gone.sort_unstable_by(|a, b| b.cmp(a));
        gone.dedup();
        parsed.push((set, gone));
    }
    for (side, (set, gone)) in parsed.into_iter().enumerate() {
        let levels = &mut book.levels[side];
        for index in gone {
            if index < levels.len() {
                levels.remove(index);
            }
        }
        for (px, sz) in set {
            match levels.iter().position(|l| l.px == px) {
                Some(at) if sz.is_zero() => {
                    levels.remove(at);
                }
                Some(at) => levels[at].sz = sz,
                None if sz.is_zero() => {}
                None => levels.push(Level { px, sz, n: 1 }),
            }
        }
        if side == 0 {
            levels.sort_by_key(|l| std::cmp::Reverse(l.px));
        } else {
            levels.sort_by_key(|l| l.px);
        }
        levels.truncate(DEPTH);
    }
    if let Some(time) = time {
        book.time = time;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ws_codec::deflate_b64;
    use serde_json::json;

    fn snapshot() -> Value {
        json!({"s": {"coin": "BTC", "time": 10, "levels": [
            [{"px": "100", "sz": "1", "n": 2}, {"px": "99", "sz": "2", "n": 1}, {"px": "98", "sz": "3", "n": 1}],
            [{"px": "101", "sz": "1", "n": 1}, {"px": "102", "sz": "2", "n": 1}],
        ]}})
    }

    fn px(book: &L2Book, side: usize) -> Vec<String> {
        book.levels[side].iter().map(|l| l.px.to_string()).collect()
    }

    #[test]
    fn a_snapshot_starts_the_book_and_a_plain_diff_updates_it() {
        let mut rebuilder = Rebuilder::default();
        let book = rebuilder.apply(&snapshot()).unwrap();
        assert_eq!(px(&book, 0), ["100", "99", "98"]);
        // 100 resized, 99 gone (index 1), a new best ask 100.5.
        let diff = json!({"u": {"c": "BTC", "t": 11,
            "l": [[{"p": "100", "s": "5"}], [{"p": "100.5", "s": "4"}]],
            "r": [[1], []]}});
        let book = rebuilder.apply(&diff).unwrap();
        assert_eq!(px(&book, 0), ["100", "98"]);
        assert_eq!(book.levels[0][0].sz, Decimal::from(5));
        assert_eq!(book.levels[0][0].n, 2, "a resized level keeps its count");
        assert_eq!(px(&book, 1), ["100.5", "101", "102"]);
        assert_eq!(book.levels[1][0].n, 1, "a new level counts one order");
        assert_eq!(book.time, 11);
    }

    #[test]
    fn a_compressed_diff_decodes_and_removals_apply_highest_first() {
        let mut rebuilder = Rebuilder::default();
        rebuilder.apply(&snapshot()).unwrap();
        // The venue sometimes lists removals ascending: indexes 0 and 2 of
        // the previous list must both go, whatever the order given.
        let diff = json!({"c": "BTC", "t": 12, "l": [[], []], "r": [[0, 2], []]}).to_string();
        let book = rebuilder.apply(&json!({"c": deflate_b64(&diff)})).unwrap();
        assert_eq!(px(&book, 0), ["99"]);
        assert_eq!(px(&book, 1), ["101", "102"]);
    }

    #[test]
    fn the_book_is_sorted_and_capped_at_twenty_levels() {
        let mut rebuilder = Rebuilder::default();
        rebuilder.apply(&snapshot()).unwrap();
        let bids: Vec<Value> = (1..=25)
            .map(|i| json!({"p": (50 + i).to_string(), "s": "1"}))
            .collect();
        let diff = json!({"u": {"c": "BTC", "t": 13, "l": [bids, []], "r": [[], []]}});
        let book = rebuilder.apply(&diff).unwrap();
        assert_eq!(book.levels[0].len(), 20);
        assert_eq!(px(&book, 0)[0], "100", "best bid first");
        assert!(book.levels[0].windows(2).all(|w| w[0].px > w[1].px));
    }

    #[test]
    fn a_frame_that_cannot_apply_changes_nothing() {
        let mut rebuilder = Rebuilder::default();
        let orphan = json!({"u": {"c": "BTC", "t": 1, "l": [[], []], "r": [[], []]}});
        assert!(rebuilder.apply(&orphan).is_none(), "no snapshot yet");
        rebuilder.apply(&snapshot()).unwrap();
        for bad in [
            json!({"u": {"c": "BTC", "t": 2, "l": [[{"p": "x", "s": "1"}], []], "r": [[], []]}}),
            json!({"u": {"c": "BTC", "t": 2, "l": [[]], "r": [[], []]}}),
            json!({"c": "!!not base64!!"}),
            json!({"unknown": 1}),
        ] {
            assert!(rebuilder.apply(&bad).is_none(), "{bad}");
        }
        let kept = rebuilder
            .apply(&json!({"u": {"c": "BTC", "t": 3, "l": [[], []], "r": [[], []]}}))
            .unwrap();
        assert_eq!(px(&kept, 0), ["100", "99", "98"], "the bad frames left it");
        rebuilder.forget("BTC");
        assert!(
            rebuilder.apply(&orphan).is_none(),
            "forgotten with its stream"
        );
    }
}
