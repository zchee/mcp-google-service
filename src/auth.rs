//! ADC-backed token acquisition and outbound auth headers.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use tokio::sync::{Mutex, OnceCell};
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

/// How long a failed fetch is held against the source before it is retried.
///
/// A broken credential is expensive to re-discover: gcp_auth retries a failing
/// token endpoint five times with back-off (~750 ms), probes the metadata
/// server, and finally shells out to `gcloud`, and a wedged source costs the
/// whole [`TOKEN_FETCH_TIMEOUT`]. Without this window every `call` would pay
/// that again just to return the same error. Within it the first failure is
/// returned at once; after it, one call retries, so a repaired credential is
/// picked up without a restart.
const FETCH_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

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
        Box::pin(async { fetch_from(&*self.provider).await })
    }
}

/// [`GcpTokenSource`] whose ADC chain is discovered on first use.
///
/// `gcp_auth::provider()` is not free: it loads the system trust store for its
/// own HTTP client (~120 ms on macOS) and, for a gcloud `authorized_user`
/// file, exchanges the refresh token eagerly (a network round trip). Deferring
/// it to the first token request keeps both off the startup path. A discovery
/// that fails is not cached here, so an operator who repairs their credentials
/// (`gcloud auth application-default login`) is picked up without a restart --
/// by the first call after [`FETCH_FAILURE_COOLDOWN`], which [`AuthContext`]
/// enforces so a broken chain is not re-walked on every call.
#[derive(Default)]
struct LazyGcpTokenSource {
    provider: OnceCell<Arc<dyn gcp_auth::TokenProvider>>,
}

impl TokenSource for LazyGcpTokenSource {
    fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>> {
        Box::pin(async {
            let provider = self.provider.get_or_try_init(discover_provider).await?;
            fetch_from(&**provider).await
        })
    }
}

/// Run gcp_auth's credential discovery without stalling the runtime.
///
/// `gcp_auth::provider()` is an async fn with blocking work inside it: it
/// loads the system trust store (~120 ms on macOS), parses keys, and may
/// spawn `gcloud`. Polled directly on a multi-thread worker it holds that
/// worker -- and whatever else sits in its queue, such as the MCP session's
/// own tasks -- for the duration; measured, that turned a 22 ms `tools/list`
/// into ~160 ms on one worker and into a 22/50 ms coin toss on sixteen under
/// load. `block_in_place` hands the worker's queue to a fresh thread first,
/// and `Handle::block_on` still drives the discovery's network I/O on the
/// runtime. A current-thread runtime (tests) has nothing to hand over to, so
/// there the future is simply awaited.
async fn discover_provider() -> Result<Arc<dyn gcp_auth::TokenProvider>, Error> {
    let handle = tokio::runtime::Handle::current();
    let discovered = match handle.runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(gcp_auth::provider()))
        }
        _ => gcp_auth::provider().await,
    };
    discovered.map_err(Error::from)
}

/// Request a token for [`SCOPES`] from `provider` and convert its expiry.
async fn fetch_from(provider: &dyn gcp_auth::TokenProvider) -> Result<FetchedToken, Error> {
    let token = provider.token(SCOPES).await?;
    let expires_st: SystemTime = token.expires_at().into();
    let expires_at = expires_st
        .duration_since(SystemTime::now())
        .map_or_else(|_already_past| Instant::now(), |ttl| Instant::now() + ttl);
    Ok(FetchedToken {
        value: Zeroizing::new(token.as_str().to_owned()),
        expires_at: Some(expires_at),
    })
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

/// The most recent failed fetch, held against the source for
/// [`FETCH_FAILURE_COOLDOWN`].
struct RecentFailure {
    /// When the failure was observed, on tokio's clock so tests can advance it.
    at: tokio::time::Instant,
    /// The failure, already rendered; the original error is not `Clone`.
    cause: String,
}

/// What the cache lock guards: the live token, and the failure that stands
/// in for it while the source is cooling down.
#[derive(Default)]
struct CacheState {
    token: Option<CachedToken>,
    failure: Option<RecentFailure>,
}

/// Authenticated outbound context: cached ADC token plus quota project.
///
/// Cloneable-by-reference via `Arc`; safe to share across tasks.
pub struct AuthContext {
    source: Arc<dyn TokenSource>,
    quota_project: HeaderValue,
    cached: Mutex<CacheState>,
}

impl AuthContext {
    /// Build a context on the gcp_auth ADC chain for the configured quota
    /// project, discovering the chain now.
    ///
    /// Fails fast: an unusable credential source is reported here, before
    /// anything is served. This is what `--strict-startup` asks for.
    pub async fn new(cfg: &Config) -> Result<Self, Error> {
        let provider = gcp_auth::provider().await?;
        Self::with_source(Arc::new(GcpTokenSource { provider }), &cfg.quota_project)
    }

    /// Build a context on the gcp_auth ADC chain for the configured quota
    /// project, discovering the chain on the first token request instead.
    ///
    /// Nothing touches the credential source until [`AuthContext::apply`] is
    /// first called, so a missing or expired credential surfaces on that call
    /// rather than at startup. See [`LazyGcpTokenSource`].
    pub fn new_lazy(cfg: &Config) -> Result<Self, Error> {
        Self::with_source(Arc::new(LazyGcpTokenSource::default()), &cfg.quota_project)
    }

    /// Build a context over an arbitrary token source.
    ///
    /// This is the injection point for tests that must observe header
    /// application or refresh behavior without real credentials.
    pub fn with_source(source: Arc<dyn TokenSource>, quota_project: &str) -> Result<Self, Error> {
        Ok(Self {
            source,
            quota_project: HeaderValue::from_str(quota_project)?,
            cached: Mutex::default(),
        })
    }

    /// Set exactly `Authorization: Bearer <token>` and
    /// `x-goog-user-project: <quota project>` on `headers`.
    ///
    /// The cached token is refreshed when it is within 60s of expiry. The
    /// cache lock is held across the refresh, so concurrent callers trigger
    /// a single upstream fetch (single-flight), and that fetch is bounded by
    /// [`TOKEN_FETCH_TIMEOUT`] so a stalled credential source cannot pin the
    /// lock indefinitely. A fetch that fails, or times out, is not retried for
    /// [`FETCH_FAILURE_COOLDOWN`]: until then every call fails at once with
    /// [`Error::CredentialsCoolingDown`] carrying the original failure.
    pub async fn apply(&self, headers: &mut HeaderMap) -> Result<(), Error> {
        let mut state = self.cached.lock().await;
        if state
            .token
            .as_ref()
            .is_none_or(|tok| tok.is_stale(Instant::now()))
        {
            if let Some(failure) = &state.failure {
                let age = failure.at.elapsed();
                if age < FETCH_FAILURE_COOLDOWN {
                    return Err(Error::CredentialsCoolingDown {
                        cause: failure.cause.clone(),
                        retry_after_secs: (FETCH_FAILURE_COOLDOWN - age).as_secs().max(1),
                    });
                }
            }
            let fetched = match tokio::time::timeout(TOKEN_FETCH_TIMEOUT, self.source.fetch()).await
            {
                Ok(Ok(fetched)) => fetched,
                Ok(Err(error)) => return Err(Self::hold_failure(&mut state, error)),
                Err(_elapsed) => {
                    return Err(Self::hold_failure(
                        &mut state,
                        Error::TokenFetchTimeout(TOKEN_FETCH_TIMEOUT),
                    ));
                }
            };
            state.token = Some(CachedToken::from_fetched(fetched)?);
            state.failure = None;
        }
        let token = state
            .token
            .as_ref()
            .expect("token cache populated just above");
        headers.insert(AUTHORIZATION, token.header.clone());
        headers.insert(USER_PROJECT_HEADER, self.quota_project.clone());
        Ok(())
    }

    /// Record `error` as the failure the source is cooling down from, and hand
    /// it back unchanged for this call.
    fn hold_failure(state: &mut CacheState, error: Error) -> Error {
        state.failure = Some(RecentFailure {
            at: tokio::time::Instant::now(),
            cause: error.to_string(),
        });
        error
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
        // whole process down with it -- and the wedged source must not be
        // waited on again at once, or every call would hang for the budget.
        let second = ctx
            .apply(&mut HeaderMap::new())
            .await
            .expect_err("the second attempt fails while the source cools down");
        match second {
            Error::CredentialsCoolingDown { cause, .. } => {
                assert!(cause.contains("timed out"), "cause: {cause}");
            }
            other => panic!("expected the cooled-down failure, got: {other}"),
        }

        // Once the cooldown has passed the source is tried again, and times
        // out again.
        tokio::time::advance(FETCH_FAILURE_COOLDOWN).await;
        let third = ctx
            .apply(&mut HeaderMap::new())
            .await
            .expect_err("after the cooldown the hanging source is retried");
        assert!(matches!(third, Error::TokenFetchTimeout(_)));
    }

    /// A source that fails a set number of times, then answers, counting
    /// every attempt.
    struct FlakySource {
        failures_left: AtomicUsize,
        calls: AtomicUsize,
    }

    impl FlakySource {
        fn failing(times: usize) -> Arc<Self> {
            Arc::new(Self {
                failures_left: AtomicUsize::new(times),
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl TokenSource for FlakySource {
        fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let fail = self
                .failures_left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
            Box::pin(async move {
                if fail {
                    Err(Error::QuotaProjectUnresolved)
                } else {
                    Ok(FetchedToken {
                        value: Zeroizing::new("token-after-repair".to_owned()),
                        expires_at: None,
                    })
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_fetch_is_not_retried_until_the_cooldown_passes() {
        let source = FlakySource::failing(usize::MAX);
        let ctx = AuthContext::with_source(Arc::clone(&source) as Arc<dyn TokenSource>, "p")
            .expect("valid inputs");

        let first = ctx
            .apply(&mut HeaderMap::new())
            .await
            .expect_err("the source fails");
        assert!(matches!(first, Error::QuotaProjectUnresolved));
        assert_eq!(source.calls(), 1);

        // Back-to-back calls must not pay the source's failure path again:
        // they get the same classified text, at once, with a retry horizon.
        for _ in 0..3 {
            let held = ctx
                .apply(&mut HeaderMap::new())
                .await
                .expect_err("still failing, without a new attempt");
            match held {
                Error::CredentialsCoolingDown {
                    cause,
                    retry_after_secs,
                } => {
                    assert_eq!(cause, Error::QuotaProjectUnresolved.to_string());
                    assert!(
                        (1..=FETCH_FAILURE_COOLDOWN.as_secs()).contains(&retry_after_secs),
                        "retry horizon must be within the cooldown: {retry_after_secs}s"
                    );
                }
                other => panic!("expected the cooled-down failure, got: {other}"),
            }
        }
        assert_eq!(
            source.calls(),
            1,
            "no call during the cooldown may reach the source"
        );

        tokio::time::advance(FETCH_FAILURE_COOLDOWN).await;
        let retried = ctx
            .apply(&mut HeaderMap::new())
            .await
            .expect_err("the source is still broken, so the retry fails too");
        assert!(matches!(retried, Error::QuotaProjectUnresolved));
        assert_eq!(source.calls(), 2, "exactly one retry after the cooldown");
    }

    #[tokio::test(start_paused = true)]
    async fn a_repaired_source_is_picked_up_after_the_cooldown_without_a_restart() {
        let source = FlakySource::failing(1);
        let ctx = AuthContext::with_source(Arc::clone(&source) as Arc<dyn TokenSource>, "p")
            .expect("valid inputs");

        ctx.apply(&mut HeaderMap::new())
            .await
            .expect_err("the first attempt fails");
        ctx.apply(&mut HeaderMap::new())
            .await
            .expect_err("inside the cooldown the failure is held");
        assert_eq!(source.calls(), 1);

        tokio::time::advance(FETCH_FAILURE_COOLDOWN).await;
        let mut headers = HeaderMap::new();
        ctx.apply(&mut headers)
            .await
            .expect("the repaired source answers on the retry");
        assert_eq!(source.calls(), 2);
        assert_eq!(
            headers.get(AUTHORIZATION).map(|v| v.to_str().ok()),
            Some(Some("Bearer token-after-repair")),
            "the recovered token must be the one attached"
        );

        // And the recovery clears the failure: the next call is served from
        // the cache, touching neither the source nor the cooldown.
        ctx.apply(&mut HeaderMap::new())
            .await
            .expect("cached token");
        assert_eq!(source.calls(), 2);
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
