//! ADC-backed token acquisition and outbound auth headers.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::config::Config;
use crate::error::Error;

/// OAuth2 scope requested for every upstream call.
const SCOPES: &[&str] = &["https://www.googleapis.com/auth/cloud-platform"];

/// Refresh the cached token once it is closer than this to expiry.
const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// Budget for one token acquisition.
///
/// The cache lock is held across the fetch to keep it single-flight, so a
/// credential source that hangs (an unreachable metadata server, a wedged
/// `gcloud`) would otherwise stall every upstream call in the process behind
/// it with no upper bound.
const TOKEN_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Lifetime granted to a token whose expiry is already in the past.
///
/// A source can hand back a token that already looks expired locally, which in
/// practice means clock skew against the issuer rather than a truly dead
/// token. Believing that literally makes *every* request re-fetch, turning a
/// skewed clock into a request storm against the credential endpoint. The
/// floor is deliberately two refresh margins: [`CachedToken::is_stale`] fires
/// once `now + REFRESH_MARGIN` reaches the deadline, so a shorter floor would
/// still be stale on the very next call and change nothing.
const EXPIRED_TOKEN_FLOOR: Duration = Duration::from_secs(120);

/// Header carrying the quota project on every upstream request.
const USER_PROJECT_HEADER: HeaderName = HeaderName::from_static("x-goog-user-project");

/// A freshly fetched bearer token plus its expiry deadline.
pub struct FetchedToken {
    /// The raw OAuth2 access token. Never log this value.
    ///
    /// Held in [`Zeroizing`] so the heap buffer is overwritten when the token
    /// is dropped rather than left in freed memory for the rest of the
    /// process's life.
    pub value: Zeroizing<String>,
    /// Deadline after which the token is invalid; `None` means no expiry.
    pub expires_at: Option<Instant>,
}

/// Source of bearer tokens.
///
/// Production uses the gcp_auth ADC chain; tests inject fakes. The future is
/// boxed by hand because the trait must stay dyn-compatible without pulling
/// in the `async-trait` crate.
pub trait TokenSource: Send + Sync {
    /// Fetch a fresh token.
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>>;
}

/// Production [`TokenSource`] backed by the gcp_auth ADC chain.
struct GcpTokenSource {
    provider: Arc<dyn gcp_auth::TokenProvider>,
}

impl TokenSource for GcpTokenSource {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>> {
        Box::pin(async {
            let token = self.provider.token(SCOPES).await?;
            let expires_st: SystemTime = token.expires_at().into();
            let expires_at = expires_st
                .duration_since(SystemTime::now())
                .map_or_else(|_already_past| Instant::now(), |ttl| Instant::now() + ttl);
            Ok(FetchedToken {
                value: Zeroizing::new(token.as_str().to_owned()),
                expires_at: Some(expires_at),
            })
        })
    }
}

/// Cached, pre-encoded `Authorization` header plus its refresh deadline.
struct CachedToken {
    header: HeaderValue,
    expires_at: Option<Instant>,
}

impl CachedToken {
    fn from_fetched(fetched: FetchedToken) -> Result<Self, Error> {
        // The rendered header is itself credential material, so it is scrubbed
        // on drop like the token it wraps. `HeaderValue` keeps its own copy of
        // the bytes, which is why this only shortens the temporary's life
        // rather than removing every copy.
        let rendered = Zeroizing::new(format!("Bearer {}", *fetched.value));
        let mut header = HeaderValue::from_str(&rendered)?;
        header.set_sensitive(true);
        Ok(Self {
            header,
            expires_at: floor_expiry(fetched.expires_at),
        })
    }

    /// Whether the token must be refreshed before use at time `now`.
    fn is_stale(&self, now: Instant) -> bool {
        self.expires_at
            .is_some_and(|deadline| now + REFRESH_MARGIN >= deadline)
    }
}

/// Push an already-elapsed deadline far enough out to stop a refetch storm.
///
/// Deadlines still in the future are returned untouched: a genuinely
/// short-lived token must keep being refreshed on schedule. See
/// [`EXPIRED_TOKEN_FLOOR`] for why an elapsed one is not believed.
fn floor_expiry(expires_at: Option<Instant>) -> Option<Instant> {
    let now = Instant::now();
    expires_at.map(|deadline| {
        if deadline <= now {
            now + EXPIRED_TOKEN_FLOOR
        } else {
            deadline
        }
    })
}

/// Authenticated outbound context: cached ADC token plus quota project.
///
/// Cloneable-by-reference via `Arc`; safe to share across tasks.
pub struct AuthContext {
    source: Arc<dyn TokenSource>,
    quota_project: HeaderValue,
    cached: Mutex<Option<CachedToken>>,
}

impl AuthContext {
    /// Build a context on the gcp_auth ADC chain for the configured quota
    /// project.
    pub async fn new(cfg: &Config) -> Result<Self, Error> {
        let provider = gcp_auth::provider().await?;
        Self::with_source(Arc::new(GcpTokenSource { provider }), &cfg.quota_project)
    }

    /// Build a context over an arbitrary token source.
    ///
    /// This is the injection point for tests that must observe header
    /// application or refresh behavior without real credentials.
    pub fn with_source(source: Arc<dyn TokenSource>, quota_project: &str) -> Result<Self, Error> {
        Ok(Self {
            source,
            quota_project: HeaderValue::from_str(quota_project)?,
            cached: Mutex::new(None),
        })
    }

    /// Set exactly `Authorization: Bearer <token>` and
    /// `x-goog-user-project: <quota project>` on `headers`.
    ///
    /// The cached token is refreshed when it is within 60s of expiry. The
    /// cache lock is held across the refresh, so concurrent callers trigger
    /// a single upstream fetch (single-flight), and that fetch is bounded by
    /// [`TOKEN_FETCH_TIMEOUT`] so a stalled credential source cannot pin the
    /// lock indefinitely.
    pub async fn apply(&self, headers: &mut HeaderMap) -> Result<(), Error> {
        let mut cached = self.cached.lock().await;
        if cached
            .as_ref()
            .is_none_or(|tok| tok.is_stale(Instant::now()))
        {
            let fetched = tokio::time::timeout(TOKEN_FETCH_TIMEOUT, self.source.fetch())
                .await
                .map_err(|_elapsed| Error::TokenFetchTimeout(TOKEN_FETCH_TIMEOUT))??;
            *cached = Some(CachedToken::from_fetched(fetched)?);
        }
        let token = cached.as_ref().expect("token cache populated just above");
        headers.insert(AUTHORIZATION, token.header.clone());
        headers.insert(USER_PROJECT_HEADER, self.quota_project.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Deterministic in-memory token source counting fetches.
    struct FakeSource {
        calls: AtomicUsize,
        ttl: Option<Duration>,
        token: &'static str,
    }

    impl FakeSource {
        fn new(ttl: Option<Duration>, token: &'static str) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                ttl,
                token,
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl TokenSource for FakeSource {
        fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let value = Zeroizing::new(self.token.to_owned());
            let expires_at = self.ttl.map(|ttl| Instant::now() + ttl);
            Box::pin(async move { Ok(FetchedToken { value, expires_at }) })
        }
    }

    /// A source that never answers, standing in for a wedged metadata server.
    struct HangingSource;

    impl TokenSource for HangingSource {
        fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn apply_sets_exactly_both_headers_and_marks_authorization_sensitive() {
        let source = FakeSource::new(Some(Duration::from_secs(3600)), "fake-token-value");
        let ctx = AuthContext::with_source(source, "my-quota-project").expect("valid inputs");

        let mut headers = HeaderMap::new();
        ctx.apply(&mut headers).await.expect("apply succeeds");

        assert_eq!(headers.len(), 2, "apply must set exactly two headers");
        let authorization = headers.get(AUTHORIZATION).expect("Authorization present");
        assert_eq!(authorization.to_str().ok(), Some("Bearer fake-token-value"));
        assert!(
            authorization.is_sensitive(),
            "Authorization must be marked sensitive so it is redacted from debug output"
        );
        assert_eq!(
            headers
                .get("x-goog-user-project")
                .and_then(|v| v.to_str().ok()),
            Some("my-quota-project")
        );
    }

    #[tokio::test]
    async fn apply_reuses_cached_token_while_fresh() {
        let source = FakeSource::new(Some(Duration::from_secs(3600)), "fresh");
        let ctx = AuthContext::with_source(Arc::clone(&source) as Arc<dyn TokenSource>, "p")
            .expect("valid inputs");

        for _ in 0..3 {
            ctx.apply(&mut HeaderMap::new())
                .await
                .expect("apply succeeds");
        }
        assert_eq!(source.calls(), 1, "a fresh token must not be refetched");
    }

    #[tokio::test]
    async fn apply_refreshes_token_within_expiry_margin() {
        let source = FakeSource::new(Some(Duration::from_secs(30)), "short-lived");
        let ctx = AuthContext::with_source(Arc::clone(&source) as Arc<dyn TokenSource>, "p")
            .expect("valid inputs");

        ctx.apply(&mut HeaderMap::new()).await.expect("first apply");
        ctx.apply(&mut HeaderMap::new())
            .await
            .expect("second apply");
        assert_eq!(
            source.calls(),
            2,
            "a token expiring within the 60s margin must be refreshed on each apply"
        );
    }

    #[tokio::test]
    async fn apply_never_refreshes_tokens_without_expiry() {
        let source = FakeSource::new(None, "eternal");
        let ctx = AuthContext::with_source(Arc::clone(&source) as Arc<dyn TokenSource>, "p")
            .expect("valid inputs");

        for _ in 0..3 {
            ctx.apply(&mut HeaderMap::new())
                .await
                .expect("apply succeeds");
        }
        assert_eq!(source.calls(), 1, "tokens without expiry are never stale");
    }

    #[tokio::test]
    async fn an_already_expired_token_is_fetched_once_not_once_per_request() {
        // A zero TTL is what clock skew against the token issuer looks like
        // locally: the token arrives already past its deadline. Believing that
        // literally makes every request re-fetch.
        let source = FakeSource::new(Some(Duration::ZERO), "skewed-clock-token");
        let ctx = AuthContext::with_source(Arc::clone(&source) as Arc<dyn TokenSource>, "p")
            .expect("valid inputs");

        for _ in 0..5 {
            ctx.apply(&mut HeaderMap::new())
                .await
                .expect("apply succeeds");
        }
        assert_eq!(
            source.calls(),
            1,
            "an already-expired token must be floored into usefulness, not \
             re-fetched on every single request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_token_source_times_out_rather_than_pinning_the_cache() {
        let ctx = AuthContext::with_source(Arc::new(HangingSource), "p").expect("valid inputs");

        let error = ctx
            .apply(&mut HeaderMap::new())
            .await
            .expect_err("a source that never answers must fail rather than hang");
        match error {
            Error::TokenFetchTimeout(after) => assert_eq!(after, TOKEN_FETCH_TIMEOUT),
            other => panic!("expected a token-fetch timeout, got: {other}"),
        }

        // The lock must be free afterwards, or one wedged fetch would take the
        // whole process down with it.
        let second = ctx
            .apply(&mut HeaderMap::new())
            .await
            .expect_err("the second attempt also times out");
        assert!(matches!(second, Error::TokenFetchTimeout(_)));
    }

    #[test]
    fn floor_expiry_only_moves_deadlines_that_have_already_passed() {
        let now = Instant::now();

        let future = now + Duration::from_secs(3600);
        assert_eq!(
            floor_expiry(Some(future)),
            Some(future),
            "a live deadline must be honoured exactly"
        );
        assert_eq!(floor_expiry(None), None, "no expiry stays no expiry");

        let past = now - Duration::from_secs(1);
        let floored = floor_expiry(Some(past)).expect("an expiry stays an expiry");
        assert!(
            floored > now + REFRESH_MARGIN,
            "the floor must clear the staleness margin, or the next call \
             re-fetches and nothing was fixed"
        );
    }

    #[test]
    fn staleness_boundary_sits_at_the_refresh_margin() {
        let now = Instant::now();
        let header = HeaderValue::from_static("Bearer x");

        let at_margin = CachedToken {
            header: header.clone(),
            expires_at: Some(now + REFRESH_MARGIN),
        };
        assert!(at_margin.is_stale(now), "exactly at the margin is stale");

        let beyond_margin = CachedToken {
            header: header.clone(),
            expires_at: Some(now + REFRESH_MARGIN + Duration::from_secs(1)),
        };
        assert!(!beyond_margin.is_stale(now), "beyond the margin is fresh");

        let no_expiry = CachedToken {
            header,
            expires_at: None,
        };
        assert!(!no_expiry.is_stale(now), "no expiry is never stale");
    }
}
