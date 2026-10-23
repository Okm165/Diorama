//! The REST lane of the market plane: typed `POST /info` calls gated by the
//! rate budget meter.
//!
//! The poller refreshes the cache (`metaAndAssetCtxs`, one `l2Book` per
//! configured market, `allMids`) every `poll_interval_ms`; the WS client keeps
//! marks/mids fresher between passes. The same client serves the passthrough
//! reads (untouched accounts are the real venue's answers).
use crate::error::{Error, Result};
use crate::feed::MarketFeed;
use crate::wire::{InfoRequest, L2Book, MetaAndAssetCtxs};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;

/// The venue API client (typed requests in, typed responses out).
#[derive(Clone)]
pub struct RestClient {
    http: reqwest::Client,
    api_url: String,
}

impl RestClient {
    pub fn new(api_url: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            api_url: api_url.trim_end_matches('/').to_string(),
        })
    }

    /// One typed info call. A non-2xx answer is a typed refusal carrying the
    /// body (the venue's error strings are the diagnostics).
    pub(crate) async fn info<T: DeserializeOwned>(&self, req: &InfoRequest) -> Result<T> {
        let resp = self
            .http
            .post(format!("{}/info", self.api_url))
            .json(req)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(Error::ApiStatus {
                status: status.as_u16(),
                body: text,
            });
        }
        serde_json::from_str(&text).map_err(|source| Error::Decode {
            what: "info response",
            source,
        })
    }

    /// Forward a raw info body verbatim (the untouched-account passthrough:
    /// the venue's own answer, unparsed).
    pub async fn raw_info(&self, body: Vec<u8>) -> Result<reqwest::Response> {
        let resp = self
            .http
            .post(format!("{}/info", self.api_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        Ok(resp)
    }
}

/// The REST poller: one pass every `interval`, each call budget-gated.
pub struct Poller {
    feed: Arc<MarketFeed>,
    api: RestClient,
    interval: Duration,
}

impl Poller {
    pub fn new(feed: Arc<MarketFeed>, api: RestClient, interval: Duration) -> Self {
        Self { feed, api, interval }
    }

    /// Run the poller forever (the sim's market-plane task).
    pub async fn run(self) {
        loop {
            self.pass().await;
            tokio::time::sleep(self.interval).await;
        }
    }

    /// One pass: meta+ctxs, spot meta, the configured books, allMids;
    /// failures are logged and counted, never fatal (the WS lane may be
    /// carrying the data).
    async fn pass(&self) {
        self.meta_and_ctxs().await;
        self.spot_meta().await;
        self.books().await;
        self.mids().await;
        self.feed.publish_metrics(crate::now_ms());
    }

    async fn spot_meta(&self) {
        if !self.gated(crate::feed::budget::weight::HEAVY) {
            return;
        }
        match self.api.info::<crate::wire::SpotMeta>(&InfoRequest::SpotMeta).await {
            Ok(spot) => self.feed.put_spot_meta(spot),
            Err(e) => self.rest_fault("spotMeta", &e),
        }
    }

    async fn meta_and_ctxs(&self) {
        if !self.gated(crate::feed::budget::weight::HEAVY) {
            return;
        }
        match self.api.info::<MetaAndAssetCtxs>(&InfoRequest::MetaAndAssetCtxs { dex: None }).await {
            Ok(MetaAndAssetCtxs(meta, ctxs)) => {
                let now = crate::now_ms();
                // The ctxs array is positionally aligned with the universe.
                for (asset, ctx) in meta.universe.iter().zip(ctxs) {
                    self.feed.put_ctx(asset.name.clone(), ctx, now);
                }
                self.feed.put_meta(meta, now);
            }
            Err(e) => self.rest_fault("metaAndAssetCtxs", &e),
        }
    }

    async fn books(&self) {
        for coin in &self.feed.markets {
            if !self.gated(crate::feed::budget::weight::LIGHT) {
                return;
            }
            let req = InfoRequest::L2Book {
                coin: coin.clone(),
                n_sig_figs: None,
                mantissa: None,
            };
            match self.api.info::<L2Book>(&req).await {
                Ok(book) => self.feed.put_book(book, crate::now_ms()),
                Err(e) => self.rest_fault("l2Book", &e),
            }
        }
    }

    async fn mids(&self) {
        if !self.gated(crate::feed::budget::weight::LIGHT) {
            return;
        }
        match self.api.info::<crate::wire::AllMids>(&InfoRequest::AllMids { dex: None }).await {
            Ok(mids) => self.feed.put_mids(mids.0, crate::now_ms()),
            Err(e) => self.rest_fault("allMids", &e),
        }
    }

    /// Spend weight or skip (backpressure; counted).
    fn gated(&self, weight: u64) -> bool {
        if self.feed.budget.try_spend(weight) {
            return true;
        }
        self.feed.metrics.inc_budget_skips();
        false
    }

    fn rest_fault(&self, what: &'static str, e: &Error) {
        self.feed.metrics.inc_rest_errors();
        tracing::warn!(call = what, error = %e, "market REST poll failed");
    }
}
