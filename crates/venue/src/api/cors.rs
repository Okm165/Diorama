//! CORS for the info router.
//!
//! The venue's real API answers browsers from any origin, and the consumer's
//! web app calls the mirror straight from the page (`POST /info`, a JSON body,
//! so the browser sends a preflight first). Without these headers the
//! mirror is unusable from a browser although curl and the services read it
//! fine. The info port is read-only market data plus the order lane's named
//! refusal and takes no credentials, so any origin is allowed and cookies are
//! not (no `Allow-Credentials`).
#![allow(dead_code)] // the info router adopts the layer next
use axum::extract::Request;
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD,
};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// How long a browser may reuse one preflight answer (seconds).
const PREFLIGHT_MAX_AGE: &str = "86400";

/// The headers a preflight is answered with when the page names none.
const DEFAULT_ALLOWED_HEADERS: &str = "content-type";

/// Answer a preflight on the spot; give every other response the allow-origin
/// header so the page may read it.
pub(crate) async fn cors(request: Request, next: Next) -> Response {
    let is_preflight = request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(ACCESS_CONTROL_REQUEST_METHOD);

    let mut response = if is_preflight {
        // The page's own list of request headers is echoed back, so a client
        // that adds one (an auth or tracing header) is not refused by the
        // mirror; with none named the JSON content type is allowed.
        let allowed_headers = request
            .headers()
            .get(ACCESS_CONTROL_REQUEST_HEADERS)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static(DEFAULT_ALLOWED_HEADERS));
        let mut preflight = StatusCode::NO_CONTENT.into_response();
        let headers = preflight.headers_mut();
        headers.insert(
            ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, allowed_headers);
        headers.insert(
            ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static(PREFLIGHT_MAX_AGE),
        );
        preflight
    } else {
        next.run(request).await
    };

    response
        .headers_mut()
        .insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Router;

    /// Serve a router wrapped in the CORS layer on a free port.
    async fn serve() -> String {
        let router = Router::new()
            .route("/info", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn(cors));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        format!("http://{addr}")
    }

    /// The browser's preflight for a JSON `POST /info` is answered (204) with
    /// the origin, the methods and the headers it asked about, although the
    /// route itself only takes POST.
    #[tokio::test]
    async fn a_preflight_is_answered_before_the_route_refuses_the_method() {
        let base = serve().await;
        let response = reqwest::Client::new()
            .request(Method::OPTIONS, format!("{base}/info"))
            .header("origin", "http://localhost:3000")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type, x-trace")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert_eq!(headers["access-control-allow-origin"], "*");
        assert_eq!(
            headers["access-control-allow-methods"],
            "GET, POST, OPTIONS"
        );
        assert_eq!(
            headers["access-control-allow-headers"],
            "content-type, x-trace"
        );
        assert_eq!(headers["access-control-max-age"], PREFLIGHT_MAX_AGE);
    }

    /// The real call carries the allow-origin header, so the page may read
    /// the answer.
    #[tokio::test]
    async fn an_answer_carries_the_allow_origin_header() {
        let base = serve().await;
        let response = reqwest::Client::new()
            .post(format!("{base}/info"))
            .header("origin", "http://localhost:3000")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
    }

    /// An OPTIONS that is not a preflight (no request-method header) is the
    /// route's own business: the method refusal still carries the header.
    #[tokio::test]
    async fn a_plain_options_is_left_to_the_route() {
        let base = serve().await;
        let response = reqwest::Client::new()
            .request(Method::OPTIONS, format!("{base}/info"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
    }
}
