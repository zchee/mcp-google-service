//! Runtime configuration: quota-project resolution and service selection.

use std::path::{Path, PathBuf};

use clap::ValueEnum;

use crate::error::Error;

/// How tools are surfaced to the MCP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExposeMode {
    /// Expose `list_services` / `search_tools` / `describe_tools` / `call`.
    TwoTier,
    /// Register every pruned, namespaced upstream tool with its real schema.
    Flat,
}

/// Fully resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Quota project sent as `x-goog-user-project` and used for pruning.
    pub quota_project: String,
    /// Explicit allowlist of service ids; overrides pruning when non-empty.
    pub only: Vec<String>,
    /// Service ids that are never exposed.
    pub exclude: Vec<String>,
    /// Tool-surface mode.
    pub expose: ExposeMode,
    /// Resolve credentials and enablement before serving, failing fast.
    ///
    /// Off by default: `serve` answers `initialize` and `tools/list` from the
    /// snapshot immediately and resolves both in the background, surfacing
    /// problems on the first `call` and in `list_services`. On, the pre-P2
    /// behaviour: a credential that cannot be acquired stops the server before
    /// it serves anything. Implied by [`ExposeMode::Flat`], whose tool list is
    /// fixed at `initialize` and therefore has to be final before serving.
    pub strict_startup: bool,
}

impl Config {
    /// Resolve the runtime configuration from CLI values and the process
    /// environment.
    pub fn resolve(
        project_flag: Option<String>,
        only: Vec<String>,
        exclude: Vec<String>,
        expose: ExposeMode,
        strict_startup: bool,
    ) -> Result<Self, Error> {
        let quota_project = resolve_quota_project(
            project_flag,
            std::env::var("GOOGLE_MCP_QUOTA_PROJECT").ok(),
            std::env::var("GOOGLE_CLOUD_PROJECT").ok(),
            || adc_file_path().and_then(|path| quota_project_from_adc(&path)),
        )?;
        validate_quota_project(&quota_project)?;
        let cfg = Self {
            quota_project,
            only,
            exclude,
            expose,
            strict_startup: strict_startup || expose == ExposeMode::Flat,
        };
        tracing::debug!(
            quota_project = %cfg.quota_project,
            only = ?cfg.only,
            exclude = ?cfg.exclude,
            expose = ?cfg.expose,
            strict_startup = cfg.strict_startup,
            "configuration resolved"
        );
        Ok(cfg)
    }
}

/// Pick the quota project with the documented precedence: `--project` flag,
/// `GOOGLE_MCP_QUOTA_PROJECT`, `GOOGLE_CLOUD_PROJECT`, then the ADC file's
/// `quota_project_id`. Blank values are treated as unset.
fn resolve_quota_project(
    flag: Option<String>,
    mcp_env: Option<String>,
    gcp_env: Option<String>,
    adc: impl FnOnce() -> Option<String>,
) -> Result<String, Error> {
    let non_blank = |value: Option<String>| value.filter(|v| !v.trim().is_empty());
    non_blank(flag)
        .or_else(|| non_blank(mcp_env))
        .or_else(|| non_blank(gcp_env))
        .or_else(|| non_blank(adc()))
        .ok_or(Error::QuotaProjectUnresolved)
}

/// Shortest and longest legal Google Cloud project id.
const PROJECT_ID_LEN: std::ops::RangeInclusive<usize> = 6..=30;

/// Shortest and longest legal Google Cloud project number.
///
/// Project numbers are int64 values rendered as decimal, so 20 digits covers
/// every value one can take.
const PROJECT_NUMBER_LEN: std::ops::RangeInclusive<usize> = 1..=20;

/// Reject a quota project that is neither a well-formed project id nor a
/// project number.
///
/// Google accepts both spellings wherever a project is named: the id grammar
/// `[a-z][a-z0-9-]{4,28}[a-z0-9]`, and the all-digit project number. Both
/// appear in `x-goog-user-project` and in Service Usage resource names, and an
/// operator who copies the number out of the console is not making a mistake,
/// so rejecting it would be this crate inventing a restriction Google does not
/// have.
///
/// The check exists because this value is interpolated into the Service Usage
/// URL and travels on every upstream request as a header, and it can arrive
/// from an environment variable or an ADC file rather than from the operator's
/// command line.
///
/// Failures describe the shape that was wrong and never echo the value: it is
/// attacker-influenceable text, and repeating it would put that text into logs
/// and model-visible errors.
fn validate_quota_project(value: &str) -> Result<(), Error> {
    let invalid = |reason: String| Err(Error::InvalidQuotaProject { reason });

    let len = value.chars().count();

    // Checked before the id grammar so an all-digit value is judged as the
    // project number it is, rather than failing on "does not start with a
    // letter" -- a message that would send an operator looking for a mistake
    // they did not make.
    if len > 0 && value.chars().all(|c| c.is_ascii_digit()) {
        return if PROJECT_NUMBER_LEN.contains(&len) {
            Ok(())
        } else {
            invalid(format!(
                "it is all digits, so it can only be a project number, but it is \
                 {len} digits, outside the {}-{} range",
                PROJECT_NUMBER_LEN.start(),
                PROJECT_NUMBER_LEN.end()
            ))
        };
    }

    if !PROJECT_ID_LEN.contains(&len) {
        return invalid(format!(
            "it is {len} characters long, outside the {}-{} range",
            PROJECT_ID_LEN.start(),
            PROJECT_ID_LEN.end()
        ));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return invalid(
            "it contains characters outside lowercase letters, digits and hyphens".to_owned(),
        );
    }
    if !value.starts_with(|c: char| c.is_ascii_lowercase()) {
        return invalid("it does not start with a lowercase letter".to_owned());
    }
    if value.ends_with('-') {
        return invalid("it ends with a hyphen".to_owned());
    }
    Ok(())
}

/// Locate the Application Default Credentials file:
/// `GOOGLE_APPLICATION_CREDENTIALS` when set, otherwise the gcloud default
/// path under the home directory.
fn adc_file_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS") {
        return Some(PathBuf::from(explicit));
    }
    std::env::home_dir()
        .map(|home| home.join(".config/gcloud/application_default_credentials.json"))
}

/// Subset of the ADC file relevant to quota-project resolution.
#[derive(serde::Deserialize)]
struct AdcFile {
    quota_project_id: Option<String>,
}

/// Read `quota_project_id` from an ADC file; unreadable or invalid input
/// resolves to `None` (logged at debug level) so later mechanisms can apply.
fn quota_project_from_adc(path: &Path) -> Option<String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::debug!(path = %path.display(), error = %err, "ADC file not readable");
            return None;
        }
    };
    match serde_json::from_slice::<AdcFile>(&bytes) {
        Ok(adc) => adc.quota_project_id.filter(|v| !v.trim().is_empty()),
        Err(err) => {
            tracing::debug!(path = %path.display(), error = %err, "ADC file is not valid JSON");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mcp-google-service-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write temp file");
        path
    }

    #[test]
    fn flag_wins_over_everything() {
        let project = resolve_quota_project(
            Some("flag-proj".to_owned()),
            Some("mcp-proj".to_owned()),
            Some("gcp-proj".to_owned()),
            || Some("adc-proj".to_owned()),
        )
        .expect("flag resolves");
        assert_eq!(project, "flag-proj");
    }

    #[test]
    fn mcp_env_wins_when_no_flag() {
        let project = resolve_quota_project(
            None,
            Some("mcp-proj".to_owned()),
            Some("gcp-proj".to_owned()),
            || Some("adc-proj".to_owned()),
        )
        .expect("env resolves");
        assert_eq!(project, "mcp-proj");
    }

    #[test]
    fn gcp_env_wins_over_adc() {
        let project = resolve_quota_project(None, None, Some("gcp-proj".to_owned()), || {
            Some("adc-proj".to_owned())
        })
        .expect("env resolves");
        assert_eq!(project, "gcp-proj");
    }

    #[test]
    fn adc_is_the_last_resort() {
        let project = resolve_quota_project(None, None, None, || Some("adc-proj".to_owned()))
            .expect("adc resolves");
        assert_eq!(project, "adc-proj");
    }

    #[test]
    fn adc_is_not_read_when_flag_present() {
        let project = resolve_quota_project(Some("flag-proj".to_owned()), None, None, || {
            panic!("ADC file must not be read when the flag is set")
        })
        .expect("flag resolves");
        assert_eq!(project, "flag-proj");
    }

    #[test]
    fn error_when_nothing_configured() {
        let err = resolve_quota_project(None, None, None, || None)
            .expect_err("nothing configured must error");
        assert!(matches!(err, Error::QuotaProjectUnresolved));
    }

    #[test]
    fn blank_values_are_treated_as_unset() {
        let project =
            resolve_quota_project(Some("   ".to_owned()), Some(String::new()), None, || {
                Some("adc-proj".to_owned())
            })
            .expect("adc resolves");
        assert_eq!(project, "adc-proj");
    }

    #[test]
    fn valid_project_ids_are_accepted() {
        // Real shapes plus both length boundaries, so the range is exercised
        // from inside rather than only from outside.
        for id in [
            "my-project",
            "gwskey-6",
            "test-project",
            "abcdef",
            "a23456789012345678901234567890",
        ] {
            assert!(
                validate_quota_project(id).is_ok(),
                "`{id}` is a well-formed project id and must be accepted"
            );
        }
    }

    #[test]
    fn project_numbers_are_accepted_alongside_project_ids() {
        // Google names a project by either spelling, and the console shows the
        // number as prominently as the id, so an operator passing the number
        // is not making a mistake.
        for number in ["123456789012", "1", "12345678901234567890"] {
            assert!(
                validate_quota_project(number).is_ok(),
                "`{number}` is a project number and must be accepted"
            );
        }
    }

    #[test]
    fn a_mixed_digit_and_letter_shape_is_neither_an_id_nor_a_number() {
        // Not an id (does not start with a letter) and not a number (not all
        // digits), so accepting the number spelling must not widen into it.
        for mixed in ["123abc456", "9project", "12345678901234567890123"] {
            let Err(error) = validate_quota_project(mixed) else {
                panic!("`{mixed}` is neither a project id nor a project number");
            };
            assert!(matches!(error, Error::InvalidQuotaProject { .. }));
        }
    }

    #[test]
    fn invalid_project_ids_are_rejected_without_echoing_the_value() {
        // Distinct failure modes: too short, too long, leading digit, trailing
        // hyphen, uppercase, then payloads shaped like shell, terminal-escape
        // and path-traversal injections.
        let too_long = "a".repeat(31);
        let cases = [
            "p-1",
            too_long.as_str(),
            "1project",
            "project-",
            "MyProject",
            "proj$(id)",
            "proj;rm -rf /",
            "proj\u{1b}[31m",
            "../../etc/passwd",
        ];

        for id in cases {
            let Err(error) = validate_quota_project(id) else {
                panic!("`{id}` must be rejected as a quota project");
            };
            let rendered = error.to_string();
            assert!(
                matches!(error, Error::InvalidQuotaProject { .. }),
                "`{id}` must fail as an invalid quota project, not as {error}"
            );
            assert!(
                !rendered.contains(id),
                "the rejected value must not be echoed back into the error: {rendered}"
            );
        }
    }

    #[test]
    fn adc_file_with_quota_project_id() {
        let path = write_temp(
            "adc-with-quota.json",
            r#"{"client_id":"x","quota_project_id":"adc-proj","type":"authorized_user"}"#,
        );
        assert_eq!(quota_project_from_adc(&path).as_deref(), Some("adc-proj"));
    }

    #[test]
    fn adc_file_without_quota_project_id() {
        let path = write_temp(
            "adc-without-quota.json",
            r#"{"client_id":"x","type":"authorized_user"}"#,
        );
        assert_eq!(quota_project_from_adc(&path), None);
    }

    #[test]
    fn adc_file_missing_or_invalid() {
        let missing = Path::new("/nonexistent/mcp-google-service/adc.json");
        assert_eq!(quota_project_from_adc(missing), None);

        let invalid = write_temp("adc-invalid.json", "not json at all");
        assert_eq!(quota_project_from_adc(&invalid), None);
    }
}
