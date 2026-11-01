//! The REST lane of the market plane: typed `POST /info` calls gated by the
//! rate budget meter.
//!
//! The poller refreshes the cache (`metaAndAssetCtxs`, one `l2Book` per
//! configured market, `allMids`) every `poll_interval_ms`; a book or mids the WS
//! client updated within one interval is skipped, so the REST lane is the gap
//! reconciliation rather than a duplicate read. The same client serves the
//! passthrough reads (untouched accounts are the real venue's answers).
//!
//! Failover is the shared [`EndpointPool::execute`]: admission, cooldown,
//! bounded retry and the attempt budget live in the common spine. This module
//! implements only one request against one endpoint and the venue's own
//! answer-vs-endpoint-fault boundary.
use crate::error::{Error, Result};
use crate::feed::MarketFeed;
use crate::wire::{InfoRequest, L2Book, MetaAndAssetCtxs};
use diorama_anvil::{Attempt, AttemptOutcome, EndpointPool, PoolError, Scheme};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;

/// Whether a non-2xx `/info` answer is the venue's own refusal rather than an
/// endpoint fault. A client error means the request itself is bad (an unknown
/// user, an unsupported info type); every endpoint answers it the same way.
/// Auth walls, wrong paths, timeouts, rate limits and server errors are the
/// endpoint's refusal, so another endpoint may serve the same body.
fn venue_method_refusal(status: reqwest::StatusCode) -> bool {
    status.is_client_error()
        && !matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED
                | reqwest::StatusCode::FORBIDDEN
                | reqwest::StatusCode::NOT_FOUND
                | reqwest::StatusCode::REQUEST_TIMEOUT
                | reqwest::StatusCode::PAYLOAD_TOO_LARGE
                | reqwest::StatusCode::TOO_MANY_REQUESTS
        )
}

/// The venue API client (typed requests in, typed responses out) over the
/// shared endpoint pool. The market API is a read surface: every request
/// starts at a random healthy endpoint, fails over on a transport/throttle/
/// gateway fault, and a failed endpoint is cooled down for a later round —
/// the same policy the JSON-RPC source clients use.
#[derive(Clone)]
pub struct RestClient {
    http: reqwest::Client,
    pool: EndpointPool,
}

impl RestClient {
    /// The one-argument form used by tests and one-off tools: the shared
    /// default policy. Deployments pass their instance's policy through
    /// [`Self::with_policy`].
    #[cfg(test)]
    pub fn new(api_urls: &[String]) -> Result<Self> {
        Self::with_policy(api_urls, diorama_anvil::PoolPolicy::default())
    }

    /// Build the client with the project's pool policy (rounds, cooldown,
    /// attempt budget). The policy is one type across every lane.
    #[cfg(test)]
    pub fn with_policy(api_urls: &[String], policy: diorama_anvil::PoolPolicy) -> Result<Self> {
        let pool = EndpointPool::new(api_urls, Scheme::Http, policy)?;
        Self::from_pool(pool, Duration::from_secs(10))
    }

    /// Build the client from the instance's attributed endpoints: the group's
    /// roles/chain id/weight survive into the pool, exactly as they do for the
    /// JSON-RPC source lanes.
    pub fn with_specs(
        specs: Vec<diorama_anvil::EndpointSpec>,
        policy: diorama_anvil::PoolPolicy,
        timeout: Duration,
    ) -> Result<Self> {
        let pool = EndpointPool::from_specs(specs, Scheme::Http, policy)?;
        Self::from_pool(pool, timeout)
    }

    fn from_pool(pool: EndpointPool, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self { http, pool })
    }

    /// The shared health/rotation pool, for `/metrics` and diagnostics.
    pub fn pool(&self) -> &EndpointPool {
        &self.pool
    }

    /// One typed info call over the pool. A method-level non-2xx answer is the
    /// venue's typed refusal and is returned immediately; an endpoint-level
    /// refusal is a fault, so the next endpoint is tried before the call
    /// fails.
    pub(crate) async fn info<T: DeserializeOwned>(&self, req: &InfoRequest) -> Result<T> {
        let attempt = InfoAttempt::<T> {
            api: self,
            req,
            marker: std::marker::PhantomData,
        };
        match self.pool.execute(&attempt, &[]).await {
            // A method-level refusal is returned by the attempt as `Served`:
            // it is the venue's answer, not an endpoint to fail over.
            Ok(served) => served.value,
            Err(PoolError::Exhausted { attempts, last }) => Err(Error::ApiExhausted {
                attempts,
                last: last.map(|error| error.to_string()).unwrap_or_default(),
            }),
            Err(PoolError::Cooling { retry_after_ms }) => Err(Error::ApiCooling { retry_after_ms }),
        }
    }

    /// Forward a raw info body verbatim (the untouched-account passthrough:
    /// the venue's own answer, unparsed). A **method-level** refusal (400/422
    /// and the rest of the client-error class) is the venue's answer and is
    /// returned immediately: every endpoint would answer it the same way. An
    /// **endpoint-level** refusal (auth wall, wrong path, timeout, 429, 5xx)
    /// is failed over; if every endpoint refuses, the call returns a typed
    /// [`Error::ApiExhausted`], never the last non-2xx body as a success.
    pub async fn raw_info(&self, body: Vec<u8>) -> Result<reqwest::Response> {
        let attempt = RawAttempt {
            api: self,
            body: &body,
        };
        match self.pool.execute(&attempt, &[]).await {
            Ok(served) => Ok(served.value),
            Err(PoolError::Exhausted { attempts, last }) => Err(Error::ApiExhausted {
                attempts,
                last: last.map(|error| error.to_string()).unwrap_or_default(),
            }),
            Err(PoolError::Cooling { retry_after_ms }) => Err(Error::ApiCooling { retry_after_ms }),
        }
    }

    async fn info_at<T: DeserializeOwned>(&self, base: &str, req: &InfoRequest) -> Result<T> {
        let resp = self
            .http
            .post(format!("{base}/info"))
            .json(req)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            // A method-level refusal is the venue's answer; every endpoint
            // would repeat it. Only endpoint-level faults fail over.
            return Err(if venue_method_refusal(status) {
                Error::ApiMethodRefusal {
                    status: status.as_u16(),
                    body: text,
                }
            } else {
                Error::ApiStatus {
                    status: status.as_u16(),
                    body: text,
                }
            });
        }
        serde_json::from_str(&text).map_err(|source| Error::Decode {
            what: "info response",
            source,
        })
    }

    async fn raw_at(&self, base: &str, body: &[u8]) -> Result<reqwest::Response> {
        Ok(self
            .http
            .post(format!("{base}/info"))
            .header(
                reqwest::header::CONTENT_TYPE,
                diorama_anvil::JSON_CONTENT_TYPE,
            )
            .body(body.to_vec())
            .send()
            .await?)
    }
}

/// One typed info call against one endpoint.
struct InfoAttempt<'a, T> {
    api: &'a RestClient,
    req: &'a InfoRequest,
    marker: std::marker::PhantomData<T>,
}

impl<T: DeserializeOwned> Attempt for InfoAttempt<'_, T> {
    type Output = Result<T, Error>;
    type Fault = Error;

    async fn attempt(&self, index: usize) -> AttemptOutcome<Result<T, Error>, Error> {
        let base = self.api.pool.url(index);
        match self.api.info_at(base, self.req).await {
            Ok(value) => AttemptOutcome::Served(Ok(value)),
            Err(fault @ Error::ApiMethodRefusal { .. }) => AttemptOutcome::Served(Err(fault)),
            Err(fault) => AttemptOutcome::Fault(fault),
        }
    }
}

/// One raw info body against one endpoint. A method-level client refusal is
/// the venue's answer (served); every other non-2xx status is a fault.
struct RawAttempt<'a> {
    api: &'a RestClient,
    body: &'a [u8],
}

impl Attempt for RawAttempt<'_> {
    type Output = reqwest::Response;
    type Fault = Error;

    async fn attempt(&self, index: usize) -> AttemptOutcome<reqwest::Response, Error> {
        let base = self.api.pool.url(index);
        match self.api.raw_at(base, self.body).await {
            Ok(resp) if resp.status().is_success() => AttemptOutcome::Served(resp),
            Ok(resp) if venue_method_refusal(resp.status()) => {
                // The venue understood the request and refused it. That is an
                // answer, not an endpoint fault: failing over would repeat it
                // at every endpoint.
                AttemptOutcome::Served(resp)
            }
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                AttemptOutcome::Fault(Error::ApiStatus {
                    status: status.as_u16(),
                    body: text,
                })
            }
            Err(error) => AttemptOutcome::Fault(error),
        }
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
        Self {
            feed,
            api,
            interval,
        }
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
        self.oi_caps().await;
        self.feed.publish_metrics(crate::now_ms());
    }

    /// `perpsAtOpenInterestCap`: the venue's cap list (light weight; the
    /// lane refuses OI-increasing orders on a capped coin).
    async fn oi_caps(&self) {
        if !self.gated(crate::feed::budget::weight::LIGHT) {
            return;
        }
        match self
            .api
            .info::<Vec<String>>(&InfoRequest::PerpsAtOpenInterestCap { dex: None })
            .await
        {
            Ok(caps) => self.feed.put_oi_caps(caps),
            Err(e) => self.rest_fault("perpsAtOpenInterestCap", &e),
        }
    }

    async fn spot_meta(&self) {
        if !self.gated(crate::feed::budget::weight::HEAVY) {
            return;
        }
        match self
            .api
            .info::<crate::wire::SpotMeta>(&InfoRequest::SpotMeta)
            .await
        {
            Ok(spot) => self.feed.put_spot_meta(spot),
            Err(e) => self.rest_fault("spotMeta", &e),
        }
    }

    async fn meta_and_ctxs(&self) {
        if !self.gated(crate::feed::budget::weight::HEAVY) {
            return;
        }
        match self
            .api
            .info::<MetaAndAssetCtxs>(&InfoRequest::MetaAndAssetCtxs { dex: None })
            .await
        {
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
        let now = crate::now_ms();
        let window = u64::try_from(self.interval.as_millis()).unwrap_or(u64::MAX);
        for coin in &self.feed.markets {
            // WSS primary, REST gap reconciliation: a book the socket updated
            // within one poll interval is not re-read. A scripted book owns the
            // coin and is never polled; a warming or stale one is.
            if matches!(
                self.feed.book_age(coin, now, window),
                crate::feed::BookAge::Fresh | crate::feed::BookAge::Scripted
            ) {
                continue;
            }
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
        let now = crate::now_ms();
        let window = u64::try_from(self.interval.as_millis()).unwrap_or(u64::MAX);
        if self.feed.mids_age_ms(now).is_some_and(|age| age <= window) {
            return;
        }
        if !self.gated(crate::feed::budget::weight::LIGHT) {
            return;
        }
        match self
            .api
            .info::<crate::wire::AllMids>(&InfoRequest::AllMids { dex: None })
            .await
        {
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

#[cfg(test)]
mod tests {
    use super::venue_method_refusal;
    use reqwest::StatusCode;

    /// A bad request is the venue's answer; an endpoint wall is a failover.
    /// This is the boundary the old `raw_info` collapsed: it retried every
    /// non-2xx and then returned the last body as `Ok`, so an outage could
    /// masquerade as a valid venue answer.
    #[test]
    fn a_method_refusal_is_not_an_endpoint_fault() {
        assert!(venue_method_refusal(StatusCode::BAD_REQUEST));
        assert!(venue_method_refusal(StatusCode::UNPROCESSABLE_ENTITY));
        assert!(!venue_method_refusal(StatusCode::UNAUTHORIZED));
        assert!(!venue_method_refusal(StatusCode::FORBIDDEN));
        assert!(!venue_method_refusal(StatusCode::NOT_FOUND));
        assert!(!venue_method_refusal(StatusCode::REQUEST_TIMEOUT));
        assert!(!venue_method_refusal(StatusCode::PAYLOAD_TOO_LARGE));
        assert!(!venue_method_refusal(StatusCode::TOO_MANY_REQUESTS));
        assert!(!venue_method_refusal(StatusCode::BAD_GATEWAY));
        assert!(!venue_method_refusal(StatusCode::OK));
    }
}
