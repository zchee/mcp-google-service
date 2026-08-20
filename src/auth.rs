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
///
/// Public because it is observable: a caller of [`AuthContext::apply`] waits
/// this long in the worst case.
pub const TOKEN_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a failed fetch is held against the source before it is retried.
///
/// A broken credential is expensive to re-discover: gcp_auth retries a failing
/// token endpoint five times with back-off (~750 ms), probes the metadata
/// server, and finally shells out to `gcloud`, and a wedged source costs the
/// whole [`TOKEN_FETCH_TIMEOUT`]. Without this window every `call` would pay
/// that again just to return the same error. Within it the first failure is
/// returned at once; after it, one call retries, so a repaired credential is
/// picked up without a restart.
///
/// Public because it is observable: it is the window
/// [`Error::CredentialsCoolingDown`] reports a retry horizon against, and the
/// delay before a repaired credential is picked up.
pub const FETCH_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// Budget for discovering the credential chain.
///
/// Separate from [`TOKEN_FETCH_TIMEOUT`] because it bounds a different thing:
/// that one bounds the whole `fetch`, this one bounds the blocking discovery
/// *inside* it (see [`drive_blocking`] for why an outer timeout cannot). Kept
/// equal so a caller sees one budget rather than two, and so the outer bound
/// stays the backstop rather than the thing that fires first.
const DISCOVERY_TIMEOUT: Duration = TOKEN_FETCH_TIMEOUT;

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
    drive_blocking(gcp_auth::provider(), DISCOVERY_TIMEOUT)
        .await?
        .map_err(Error::from)
}

/// Drive a future that blocks its thread, under a budget that actually fires.
///
/// The budget has to be applied **inside** the blocking region, and that is
/// the whole point of this function existing. `block_in_place` hands the
/// worker's queue away and then blocks the calling thread; a
/// `timeout(..., block_in_place(...))` wrapped *outside* can never fire,
/// because the timer future it races is on the very thread that is blocked and
/// nothing polls it until the blocking work returns. The result was that
/// [`TOKEN_FETCH_TIMEOUT`] could not bound credential discovery at all: a
/// wedged metadata server or a hung `gcloud` held the token cache's lock for
/// the life of the process, and every upstream call queued behind it forever
/// with a restart as the only exit.
///
/// A current-thread runtime has no queue to hand over, so there the future is
/// awaited and the timeout is an ordinary one.
async fn drive_blocking<F>(future: F, budget: Duration) -> Result<F::Output, Error>
where
    F: Future + Send,
    F::Output: Send,
{
    let handle = tokio::runtime::Handle::current();
    match handle.runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(|| {
            handle
                .block_on(tokio::time::timeout(budget, future))
                .map_err(|_elapsed| Error::TokenFetchTimeout(budget))
        }),
        _ => tokio::time::timeout(budget, future)
            .await
            .map_err(|_elapsed| Error::TokenFetchTimeout(budget)),
    }
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

/// What the cache lock guards: the live token, the failure that stands in for
/// it while the source is cooling down, and how many tokens have been cached
/// so far.
#[derive(Default)]
struct CacheState {
    token: Option<CachedToken>,
    failure: Option<RecentFailure>,
    generation: u64,
}

/// Identifies which cached token a header set was built from.
///
/// Bumped every time a fresh token is cached. Anything that captured the
/// credential at construction time -- an upstream MCP session carries its
/// headers for its whole life -- compares the generation it was built with
/// against the current one to learn that the token has rotated underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenGeneration(u64);

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
        // Bounded for the same reason the lazy path is: a wedged metadata
        // server or a hung `gcloud` must not hold the process open forever.
        // A plain `timeout` suffices *here* precisely because this path does
        // not block its thread -- there is no `block_in_place` between the
        // timer and the runtime, which is the whole difference from
        // [`drive_blocking`]. Leaving this one unbounded would also make the
        // two paths disagree: `--strict-startup` (and `--expose flat`, which
        // forces it) would be the only way to reach an unbounded discovery.
        let provider = tokio::time::timeout(DISCOVERY_TIMEOUT, gcp_auth::provider())
            .await
            .map_err(|_elapsed| Error::TokenFetchTimeout(DISCOVERY_TIMEOUT))??;
        Self::with_source(Arc::new(GcpTokenSource { provider }), &cfg.quota_project)
    }

    /// Build a context on the gcp_auth ADC chain for the configured quota
    /// project, discovering the chain on the first token request instead.
    ///
    /// Nothing touches the credential source until [`AuthContext::apply`] is
    /// first called, so a missing or expired credential surfaces on that call
    /// rather than at startup. Discovery is what is being deferred, and it is
    /// not cheap: it loads the system trust store for gcp_auth's own HTTP
    /// client and, for a gcloud `authorized_user` file, exchanges the refresh
    /// token eagerly. A discovery that fails is not remembered as a verdict on
    /// the credentials, so repairing them needs no restart -- the next call
    /// past [`FETCH_FAILURE_COOLDOWN`] tries again.
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
        self.apply_tracked(headers).await.map(|_generation| ())
    }

    /// [`Self::apply`], also reporting which token the headers carry.
    ///
    /// The returned [`TokenGeneration`] changes exactly when a fresh token is
    /// cached, so a caller that keeps something built from these headers
    /// alive -- an upstream session -- can tell later whether it is still
    /// current by comparing generations, without ever seeing the token.
    pub async fn apply_tracked(&self, headers: &mut HeaderMap) -> Result<TokenGeneration, Error> {
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
            // Not `?`: this can fail *after* a successful fetch, on a token
            // that cannot be rendered as a header. Letting that escape without
            // recording it sends the next call back down the whole credential
            // chain, which is a retry storm for as long as the source keeps
            // producing the same unusable token.
            let cached = match CachedToken::from_fetched(fetched) {
                Ok(cached) => cached,
                Err(error) => {
                    // Deliberately NOT `hold_failure`: that stores
                    // `error.to_string()`, and this particular error is
                    // computed *from the token bytes*. The stored text reaches
                    // the model twice over -- through
                    // `Error::CredentialsCoolingDown` and through
                    // `list_services`' `startup.credentials_error` -- so it is
                    // only safe today because `http`'s `InvalidHeaderValue` is
                    // a unit struct that renders "failed to parse header
                    // value" and drops the offending bytes. That is a
                    // third-party `Display` impl standing between a raw bearer
                    // token and an MCP client, held by nothing in this
                    // repository and free to change in a patch release. A
                    // fixed string removes the dependency on it; the caller
                    // still receives the original error.
                    state.failure = Some(RecentFailure {
                        at: tokio::time::Instant::now(),
                        cause: "the credential source returned a token that \
                                cannot be rendered as an HTTP header"
                            .to_owned(),
                    });
                    return Err(error);
                }
            };
            state.token = Some(cached);
            state.failure = None;
            state.generation += 1;
        }
        let token = state
            .token
            .as_ref()
            .expect("token cache populated just above");
        headers.insert(AUTHORIZATION, token.header.clone());
        headers.insert(USER_PROJECT_HEADER, self.quota_project.clone());
        Ok(TokenGeneration(state.generation))
    }

    /// Forget the cached token if it is still the one `generation` names, so
    /// the next [`Self::apply`] fetches a fresh one.
    ///
    /// For an upstream that rejected the token as unauthorized even though it
    /// looked fresh locally: revoked, or expired on a skewed clock. Naming the
    /// generation keeps concurrent callers that hit the same rejection from
    /// discarding the replacement token the first of them already fetched.
    /// The cooldown still applies to the fetch that follows, so a source that
    /// recently failed is not hammered by this either.
    pub async fn invalidate(&self, generation: TokenGeneration) {
        let mut state = self.cached.lock().await;
        if state.generation == generation.0 {
            state.token = None;
        }
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

    /// A slow source, to keep concurrent callers overlapping inside `apply`.
    struct SlowSource {
        calls: AtomicUsize,
    }

    impl TokenSource for SlowSource {
        fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(FetchedToken {
                    value: Zeroizing::new("single-flight".to_owned()),
                    expires_at: None,
                })
            })
        }
    }

    /// Concurrent `apply` calls must produce exactly one fetch.
    ///
    /// The single-flight property is what stops a burst of dispatches from
    /// turning into a burst of credential requests, and it is asserted on the
    /// axis the other auth tests never exercise: several callers inside
    /// `apply` at once, on a runtime with real worker threads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_applies_trigger_exactly_one_fetch() {
        let source = Arc::new(SlowSource {
            calls: AtomicUsize::new(0),
        });
        let ctx = Arc::new(
            AuthContext::with_source(Arc::clone(&source) as Arc<dyn TokenSource>, "p")
                .expect("valid inputs"),
        );

        let mut joined = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let ctx = Arc::clone(&ctx);
            joined.spawn(async move { ctx.apply(&mut HeaderMap::new()).await });
        }
        while let Some(result) = joined.join_next().await {
            result
                .expect("no task panics")
                .expect("every concurrent caller gets the token");
        }

        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            1,
            "eight concurrent callers must share one fetch, not race the \
             credential source"
        );
    }

    /// Discovery must be bounded on the runtime that actually blocks.
    ///
    /// `#[tokio::test]` alone is a current-thread runtime, where
    /// `block_in_place` is never taken and the bug is unreachable -- which is
    /// exactly why it survived: the branch that ships is the one no test
    /// entered. `flavor = "multi_thread"` is what makes this a regression
    /// test rather than a tautology.
    ///
    /// Not `start_paused`: a paused clock does not advance while a thread is
    /// blocked outside the runtime, so the wall-clock budget is the point.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_discovery_is_bounded_by_its_own_budget() {
        let started = std::time::Instant::now();
        let outcome: Result<(), Error> =
            drive_blocking(std::future::pending::<()>(), Duration::from_millis(150)).await;

        let error = outcome.expect_err("a future that never resolves must hit the budget");
        assert!(
            matches!(error, Error::TokenFetchTimeout(budget) if budget == Duration::from_millis(150)),
            "expected the discovery budget to fire, got: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the budget fired only after {:?}; if this hangs instead, the \
             timeout is outside the blocking region again",
            started.elapsed()
        );
    }

    /// A token whose *value* cannot become a header must cool down like any
    /// other failure.
    ///
    /// `CachedToken::from_fetched` can fail after a successful fetch -- a token
    /// carrying a byte no `HeaderValue` accepts. That error left `apply`
    /// through `?` without ever reaching `hold_failure`, so nothing was
    /// recorded and the very next call walked the whole credential chain
    /// again: a retry storm against ADC for as long as the source keeps
    /// handing back the same unusable token.
    struct MalformedToken {
        calls: AtomicUsize,
    }

    impl TokenSource for MalformedToken {
        fn fetch(&self) -> Pin<Box<dyn Future<Output = Result<FetchedToken, Error>> + Send + '_>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(FetchedToken {
                    // A newline is legal in a Rust string and illegal in a
                    // header value, so this fails in `from_fetched`, after the
                    // fetch has already "succeeded".
                    value: Zeroizing::new("bad\ntoken".to_owned()),
                    expires_at: None,
                })
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_that_cannot_become_a_header_still_starts_the_cooldown() {
        let source = Arc::new(MalformedToken {
            calls: AtomicUsize::new(0),
        });
        let ctx = AuthContext::with_source(Arc::clone(&source) as Arc<dyn TokenSource>, "p")
            .expect("valid inputs");

        let first = ctx
            .apply(&mut HeaderMap::new())
            .await
            .expect_err("a token that cannot be rendered as a header must fail");
        assert!(
            matches!(first, Error::InvalidHeader(_)),
            "the caller should see why it failed, got: {first}"
        );
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);

        let second = ctx
            .apply(&mut HeaderMap::new())
            .await
            .expect_err("still broken");
        assert!(
            matches!(second, Error::CredentialsCoolingDown { .. }),
            "the failure must be held like any other, got: {second}"
        );
        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            1,
            "a malformed token must not send the next call back down the \
             credential chain"
        );
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

    #[tokio::test]
    async fn the_generation_changes_exactly_when_a_fresh_token_is_cached() {
        let source = FakeSource::new(None, "stable");
        let ctx = AuthContext::with_source(Arc::clone(&source) as Arc<dyn TokenSource>, "p")
            .expect("valid inputs");

        let first = ctx
            .apply_tracked(&mut HeaderMap::new())
            .await
            .expect("first fetch");
        let second = ctx
            .apply_tracked(&mut HeaderMap::new())
            .await
            .expect("served from cache");
        assert_eq!(
            first, second,
            "a cached token keeps its generation: {first:?} vs {second:?}"
        );
        assert_eq!(source.calls(), 1);

        // Invalidating the generation that is current forces a fresh fetch and
        // a new generation; anything built from `first` is now known stale.
        ctx.invalidate(first).await;
        let third = ctx
            .apply_tracked(&mut HeaderMap::new())
            .await
            .expect("re-fetched after invalidation");
        assert_ne!(third, first);
        assert_eq!(source.calls(), 2);
    }

    #[tokio::test]
    async fn invalidating_a_superseded_generation_is_a_no_op() {
        // Two callers hit a 401 on the same old token; the first one's
        // invalidation already produced a replacement, and the second must not
        // throw that replacement away too.
        let source = FakeSource::new(None, "stable");
        let ctx = AuthContext::with_source(Arc::clone(&source) as Arc<dyn TokenSource>, "p")
            .expect("valid inputs");

        let old = ctx
            .apply_tracked(&mut HeaderMap::new())
            .await
            .expect("first fetch");
        ctx.invalidate(old).await;
        let replacement = ctx
            .apply_tracked(&mut HeaderMap::new())
            .await
            .expect("replacement fetched");
        assert_eq!(source.calls(), 2);

        ctx.invalidate(old).await;
        let still = ctx
            .apply_tracked(&mut HeaderMap::new())
            .await
            .expect("served from cache");
        assert_eq!(still, replacement);
        assert_eq!(
            source.calls(),
            2,
            "a stale invalidation must not cost another fetch"
        );
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
