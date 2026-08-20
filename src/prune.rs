//! Service Usage based pruning of exposed services.

use std::collections::HashSet;

use crate::auth::AuthContext;
use crate::error::{Error, classify_upstream};
use crate::registry::Endpoint;

/// Page size requested from the Service Usage list endpoint.
const PAGE_SIZE: &str = "200";

/// Most pages one listing may consume.
///
/// At [`PAGE_SIZE`] entries a page this covers 10,000 services, an order of
/// magnitude more than Google publishes, so reaching it means the upstream is
/// not terminating the listing rather than that the project is large.
const MAX_PAGES: usize = 50;

/// Fetch the set of APIs enabled on `project` via Service Usage v1.
///
/// Calls `GET /v1/projects/{project}/services?filter=state:ENABLED`,
/// following `nextPageToken` pagination, and returns the `config.name`
/// values (e.g. `run.googleapis.com`) for intersection with the registry's
/// `api_name` field.
///
/// Fallback policy on failure is the caller's: log a WARN naming the
/// failure and pass `None` to [`select_services`] (expose all) unless an
/// explicit `--only` list is configured.
pub async fn enabled_services(
    auth: &AuthContext,
    project: &str,
    http: &reqwest::Client,
) -> Result<HashSet<String>, Error> {
    let url = format!("https://serviceusage.googleapis.com/v1/projects/{project}/services");
    let mut enabled = HashSet::new();
    let mut page_token: Option<String> = None;
    let mut guard = PageGuard::default();

    loop {
        let mut headers = reqwest::header::HeaderMap::new();
        auth.apply(&mut headers).await?;

        let mut request = http
            .get(&url)
            .headers(headers)
            .query(&[("filter", "state:ENABLED"), ("pageSize", PAGE_SIZE)]);
        if let Some(token) = &page_token {
            request = request.query(&[("pageToken", token.as_str())]);
        }

        let response = request.send().await.map_err(|source| Error::Http {
            url: url.clone(),
            source,
        })?;
        let status = response.status();
        let body = response.text().await.map_err(|source| Error::Http {
            url: url.clone(),
            source,
        })?;
        if !status.is_success() {
            return Err(Error::Upstream(classify_upstream(status.as_u16(), &body)));
        }

        page_token = collect_page(&body, &mut enabled)?;
        let Some(next) = &page_token else {
            return Ok(enabled);
        };
        guard.advance(next)?;
    }
}

/// Guards a paginated listing against never terminating.
///
/// Two ways a listing fails to end, both of which would otherwise spin this
/// loop forever while holding a token and issuing requests: a token that
/// repeats (the server hands back a cursor it already served) and a token
/// chain that simply never ends. Neither is recoverable here, so both stop the
/// listing and surface as an error the caller degrades on.
#[derive(Debug, Default)]
struct PageGuard {
    seen: HashSet<String>,
    pages: usize,
}

impl PageGuard {
    /// Record a `nextPageToken` and decide whether paging may continue.
    fn advance(&mut self, token: &str) -> Result<(), Error> {
        self.pages += 1;
        if self.pages >= MAX_PAGES {
            return Err(Error::PaginationLimit { pages: MAX_PAGES });
        }
        if !self.seen.insert(token.to_owned()) {
            return Err(Error::PaginationStalled);
        }
        Ok(())
    }
}

/// Select the endpoints to expose, with deterministic precedence.
///
/// A non-empty `only` list (exact `service_id` match) replaces the enablement
/// intersection; otherwise the registry is intersected with `enabled` (matched
/// on `api_name`) when it is known. `exclude` is applied last in **both**
/// cases, so the deny-list always wins: an operator excluding a service is
/// stating it must not be reachable, and no other flag may quietly re-admit
/// it.
pub fn select_services<'a>(
    endpoints: &'a [Endpoint],
    enabled: Option<&HashSet<String>>,
    only: &[String],
    exclude: &[String],
) -> Vec<&'a Endpoint> {
    endpoints
        .iter()
        .filter(|endpoint| {
            if only.is_empty() {
                enabled.is_none_or(|set| set.contains(endpoint.api_name))
            } else {
                only.iter().any(|o| o == endpoint.service_id)
            }
        })
        .filter(|endpoint| !exclude.iter().any(|x| x == endpoint.service_id))
        .collect()
}

/// One page of the Service Usage `services.list` response.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServicesPage {
    #[serde(default)]
    services: Vec<ServiceEntry>,
    #[serde(default)]
    next_page_token: Option<String>,
}

/// One service entry; only `config.name` is relevant.
#[derive(Debug, serde::Deserialize)]
struct ServiceEntry {
    #[serde(default)]
    config: Option<ServiceConfig>,
}

/// The service config carrying the API name.
#[derive(Debug, serde::Deserialize)]
struct ServiceConfig {
    #[serde(default)]
    name: Option<String>,
}

/// Parse one Service Usage page, inserting `config.name` values into `out`.
///
/// Returns the `nextPageToken` when the listing has more pages. Split from
/// the HTTP fetch so pagination is testable without a network.
fn collect_page(page_json: &str, out: &mut HashSet<String>) -> Result<Option<String>, Error> {
    let page: ServicesPage = serde_json::from_str(page_json).map_err(|source| Error::Json {
        context: "Service Usage services page".to_owned(),
        source,
    })?;
    for entry in page.services {
        if let Some(name) = entry.config.and_then(|c| c.name).filter(|n| !n.is_empty()) {
            out.insert(name);
        }
    }
    Ok(page.next_page_token.filter(|token| !token.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: Endpoint = Endpoint {
        service_id: "run",
        host: "run.googleapis.com",
        api_name: "run.googleapis.com",
    };
    const LOGGING: Endpoint = Endpoint {
        service_id: "logging",
        host: "logging.googleapis.com",
        api_name: "logging.googleapis.com",
    };
    const BIGQUERY: Endpoint = Endpoint {
        service_id: "bigquery",
        host: "bigquery.googleapis.com",
        api_name: "bigquery.googleapis.com",
    };

    fn endpoints() -> [Endpoint; 3] {
        [RUN, LOGGING, BIGQUERY]
    }

    fn ids(selected: &[&Endpoint]) -> Vec<&'static str> {
        selected.iter().map(|e| e.service_id).collect()
    }

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    fn set(values: &[&str]) -> HashSet<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn only_overrides_enabled_but_never_exclude() {
        let eps = endpoints();
        let enabled = set(&["logging.googleapis.com"]);

        // `only` beats the enablement intersection: `run` is not enabled, and
        // is still selected.
        let selected = select_services(&eps, Some(&enabled), &owned(&["run"]), &[]);
        assert_eq!(ids(&selected), ["run"], "only wins over enabled");

        // `exclude` beats `only`: naming a service on both is a contradiction,
        // and the deny-list is the half that must win.
        let selected = select_services(&eps, Some(&enabled), &owned(&["run"]), &owned(&["run"]));
        assert!(
            selected.is_empty(),
            "an excluded service must not be reachable however it was selected; got {:?}",
            ids(&selected)
        );
    }

    #[test]
    fn exclude_narrows_a_wider_only_list() {
        let eps = endpoints();
        let selected = select_services(
            &eps,
            None,
            &owned(&["run", "logging", "bigquery"]),
            &owned(&["logging"]),
        );
        assert_eq!(ids(&selected), ["run", "bigquery"]);
    }

    #[test]
    fn only_with_unknown_id_selects_nothing() {
        let eps = endpoints();
        let selected = select_services(&eps, None, &owned(&["nonexistent"]), &[]);
        assert!(selected.is_empty());
    }

    #[test]
    fn enabled_intersects_by_api_name() {
        let eps = endpoints();
        let enabled = set(&["run.googleapis.com", "bigquery.googleapis.com"]);
        let selected = select_services(&eps, Some(&enabled), &[], &[]);
        assert_eq!(ids(&selected), ["run", "bigquery"]);
    }

    #[test]
    fn no_enabled_set_exposes_all() {
        let eps = endpoints();
        let selected = select_services(&eps, None, &[], &[]);
        assert_eq!(ids(&selected), ["run", "logging", "bigquery"]);
    }

    #[test]
    fn exclude_applies_after_enabled_intersection() {
        let eps = endpoints();
        let enabled = set(&["run.googleapis.com", "logging.googleapis.com"]);
        let selected = select_services(&eps, Some(&enabled), &[], &owned(&["logging"]));
        assert_eq!(ids(&selected), ["run"]);
    }

    #[test]
    fn exclude_without_enabled_set() {
        let eps = endpoints();
        let selected = select_services(&eps, None, &[], &owned(&["run", "bigquery"]));
        assert_eq!(ids(&selected), ["logging"]);
    }

    #[test]
    fn empty_enabled_set_selects_nothing() {
        let eps = endpoints();
        let enabled = HashSet::new();
        let selected = select_services(&eps, Some(&enabled), &[], &[]);
        assert!(selected.is_empty());
    }

    #[test]
    fn collect_page_extracts_names_and_token() {
        let mut out = HashSet::new();
        let token = collect_page(
            r#"{
                "services": [
                    {"name": "projects/123/services/run.googleapis.com",
                     "config": {"name": "run.googleapis.com"}, "state": "ENABLED"},
                    {"name": "projects/123/services/logging.googleapis.com",
                     "config": {"name": "logging.googleapis.com"}, "state": "ENABLED"}
                ],
                "nextPageToken": "page-2"
            }"#,
            &mut out,
        )
        .expect("valid page parses");
        assert_eq!(token.as_deref(), Some("page-2"));
        assert_eq!(out, set(&["run.googleapis.com", "logging.googleapis.com"]));
    }

    #[test]
    fn collect_page_accumulates_across_pages_until_no_token() {
        let mut out = HashSet::new();
        let first = collect_page(
            r#"{"services":[{"config":{"name":"run.googleapis.com"}}],"nextPageToken":"t2"}"#,
            &mut out,
        )
        .expect("first page parses");
        assert_eq!(first.as_deref(), Some("t2"));

        let second = collect_page(
            r#"{"services":[{"config":{"name":"bigquery.googleapis.com"}}]}"#,
            &mut out,
        )
        .expect("last page parses");
        assert_eq!(second, None, "missing token ends pagination");
        assert_eq!(out, set(&["run.googleapis.com", "bigquery.googleapis.com"]));
    }

    #[test]
    fn collect_page_skips_malformed_entries_and_blank_tokens() {
        let mut out = HashSet::new();
        let token = collect_page(
            r#"{
                "services": [
                    {"config": {"name": "run.googleapis.com"}},
                    {"config": {}},
                    {},
                    {"config": {"name": ""}}
                ],
                "nextPageToken": ""
            }"#,
            &mut out,
        )
        .expect("page parses");
        assert_eq!(token, None, "blank token ends pagination");
        assert_eq!(out, set(&["run.googleapis.com"]));
    }

    #[test]
    fn collect_page_handles_empty_object() {
        let mut out = HashSet::new();
        let token = collect_page("{}", &mut out).expect("empty page parses");
        assert_eq!(token, None);
        assert!(out.is_empty());
    }

    #[test]
    fn page_guard_refuses_a_repeated_token() {
        let mut guard = PageGuard::default();
        guard.advance("page-2").expect("the first token advances");
        guard.advance("page-3").expect("a new token advances");

        let error = guard
            .advance("page-2")
            .expect_err("a token already served means the listing is not progressing");
        assert!(matches!(error, Error::PaginationStalled), "got: {error}");
    }

    #[test]
    fn page_guard_stops_at_the_page_budget() {
        let mut guard = PageGuard::default();
        let error = (0..MAX_PAGES + 1)
            .map(|page| guard.advance(&format!("page-{page}")))
            .find_map(Result::err)
            .expect("an endless chain of fresh tokens must still terminate");
        assert!(
            matches!(error, Error::PaginationLimit { pages: MAX_PAGES }),
            "got: {error}"
        );
    }

    #[test]
    fn collect_page_rejects_invalid_json() {
        let mut out = HashSet::new();
        let err = collect_page("not json", &mut out).expect_err("invalid JSON errors");
        assert!(matches!(err, Error::Json { .. }));
    }
}
