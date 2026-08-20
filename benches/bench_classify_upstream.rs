//! `error::classify_upstream` over the live-captured Google error bodies.
//!
//! The bodies are the same verbatim 2026-08-19 captures the unit tests pin,
//! plus the JSON-envelope shape the MCP transport wraps them in, the generic
//! 403 that classifies as permission-denied, and a 500 that takes the
//! sanitize-and-pass-through path. Each case is one `args` value, so the
//! report lists them by name.
//!
//! Allocations are counted by [`AllocProfiler`], which adds a thread-local
//! increment per allocation to the timed region.

use divan::{AllocProfiler, Bencher, black_box, counter::BytesCount};
use mcp_google_service::error::{UpstreamFailure, classify_upstream};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

/// Live-captured (2026-08-19) 401 body for a request with no credential.
const MISSING_CREDENTIAL_BODY: &str = "Request is missing required authentication \
     credential. Expected OAuth 2 access token, login cookie or other valid \
     authentication credential. See \
     https://developers.google.com/identity/sign-in/web/devconsole-project.";

/// Live-captured (2026-08-19) 401 body for an API-key request.
const API_KEY_BODY: &str = "API keys are not supported by this API. Expected \
     OAuth2 access token or other authentication credentials that assert a \
     principal. See https://cloud.google.com/docs/authentication";

/// Live-captured (2026-08-19) 403 body for a missing quota project.
const QUOTA_PROJECT_BODY: &str = "Your application is authenticating by using \
     local Application Default Credentials. The developerknowledge.googleapis.com \
     API requires a quota project, which is not set by default.";

/// Live-captured (2026-08-19) 403 body for a disabled service.
const SERVICE_DISABLED_BODY: &str = "Developer Knowledge API has not been used in \
     project gwskey-6 before or it is disabled. Enable it by visiting \
     https://console.developers.google.com/apis/api/developerknowledge.googleapis.com/overview?project=gwskey-6 \
     then retry.";

/// The disabled-service body inside the JSON error envelope Google sends.
const SERVICE_DISABLED_ENVELOPE: &str = concat!(
    r#"{"error":{"code":403,"message":""#,
    "Developer Knowledge API has not been used in \
     project gwskey-6 before or it is disabled. Enable it by visiting \
     https://console.developers.google.com/apis/api/developerknowledge.googleapis.com/overview?project=gwskey-6 \
     then retry.",
    r#"","status":"PERMISSION_DENIED"}}"#,
);

/// A 403 that matches no remediation pattern.
const PERMISSION_DENIED_BODY: &str = "Permission denied on resource project x.";

/// A 500 that passes through the sanitizer untouched.
const INTERNAL_ERROR_BODY: &str = "Internal error encountered.";

/// One classifier input; the variant name is what the report shows.
#[derive(Clone, Copy, Debug)]
enum Case {
    MissingCredential,
    ApiKeyUnsupported,
    QuotaProjectMissing,
    ServiceDisabled,
    ServiceDisabledEnvelope,
    PermissionDenied,
    Internal500,
}

impl Case {
    fn input(self) -> (u16, &'static str) {
        match self {
            Self::MissingCredential => (401, MISSING_CREDENTIAL_BODY),
            Self::ApiKeyUnsupported => (401, API_KEY_BODY),
            Self::QuotaProjectMissing => (403, QUOTA_PROJECT_BODY),
            Self::ServiceDisabled => (403, SERVICE_DISABLED_BODY),
            Self::ServiceDisabledEnvelope => (403, SERVICE_DISABLED_ENVELOPE),
            Self::PermissionDenied => (403, PERMISSION_DENIED_BODY),
            Self::Internal500 => (500, INTERNAL_ERROR_BODY),
        }
    }
}

const CASES: &[Case] = &[
    Case::MissingCredential,
    Case::ApiKeyUnsupported,
    Case::QuotaProjectMissing,
    Case::ServiceDisabled,
    Case::ServiceDisabledEnvelope,
    Case::PermissionDenied,
    Case::Internal500,
];

fn main() {
    divan::main();
}

#[divan::bench(args = CASES)]
fn classify(bencher: Bencher, case: Case) {
    let (status, body) = case.input();
    bencher
        .counter(BytesCount::of_str(body))
        .bench(|| -> UpstreamFailure { classify_upstream(black_box(status), black_box(body)) });
}
