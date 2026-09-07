//! Static registry of Google remote MCP endpoints.

/// A single remote MCP endpoint served under `https://{host}{mcp_path}`.
pub struct Endpoint {
    /// Short service identifier used as the tool-name prefix (e.g. `run`).
    pub service_id: &'static str,
    /// Endpoint host (e.g. `run.googleapis.com`).
    pub host: &'static str,
    /// Service Usage API name used for enablement matching.
    pub api_name: &'static str,
    /// URL path the MCP server is mounted on: `/mcp` everywhere except the
    /// Vertex AI suites, which mount at `/mcp/{suite}` on one shared host.
    pub mcp_path: &'static str,
}

impl Endpoint {
    /// Full MCP URL this endpoint is served from.
    pub fn mcp_url(&self) -> String {
        format!("https://{}{}", self.host, self.mcp_path)
    }
}

/// Expands bare service ids into derived [`Endpoint`] entries, then appends
/// manual entries whose host or path cannot be derived from the id.
///
/// Every derived host follows `{service_id}.googleapis.com`, and Service
/// Usage reports that same string as the API name, so both fields are built
/// from the id at compile time rather than repeated by hand.
macro_rules! endpoints {
    (
        derived: [$($service_id:literal),+ $(,)?],
        manual: [$($manual:expr),+ $(,)?] $(,)?
    ) => {
        &[
            $(Endpoint {
                service_id: $service_id,
                host: concat!($service_id, ".googleapis.com"),
                api_name: concat!($service_id, ".googleapis.com"),
                mcp_path: "/mcp",
            },)+
            $($manual,)+
        ]
    };
}

/// One Vertex AI suite, mounted at `/mcp/{suite}` on the global
/// `aiplatform.googleapis.com` host, exposed under a `vertex-` id.
///
/// The docs also list 46 regional and 2 continental (`{us,eu}.rep`) hosts;
/// the global host answers discovery identically (probed 2026-08-31), and
/// tool metadata is host-invariant, so the registry pins no region. Regional
/// routing for authenticated calls is a config concern, not a registry one.
macro_rules! vertex_suite {
    ($suite:literal) => {
        Endpoint {
            service_id: concat!("vertex-", $suite),
            host: "aiplatform.googleapis.com",
            api_name: "aiplatform.googleapis.com",
            mcp_path: concat!("/mcp/", $suite),
        }
    };
}

/// Evidence-pinned endpoint table.
///
/// Every entry answered MCP `initialize` plus `tools/list` at its
/// [`Endpoint::mcp_url`] without credentials when probed live: the original
/// 47 derived hosts on 2026-08-19, and the 8 newer derived hosts plus the 10
/// manual entries (Customer Experience Agent Studio and the 9 Vertex AI
/// suites) on 2026-08-31.
pub static ENDPOINTS: &[Endpoint] = endpoints![
    derived: [
        "run",
        "compute",
        "container",
        "bigquery",
        "logging",
        "monitoring",
        "cloudtrace",
        "pubsub",
        "spanner",
        "firestore",
        "sqladmin",
        "alloydb",
        "bigtableadmin",
        "cloudresourcemanager",
        "cloudasset",
        "recommender",
        "cloudquotas",
        "redis",
        "dataform",
        "dataproc",
        "file",
        "netapp",
        "clouderrorreporting",
        "policytroubleshooter",
        "developerknowledge",
        "mapstools",
        "cloudcli",
        "geminicloudassist",
        "discoveryengine",
        "dataplex",
        "servicehealth",
        "cloudsupport",
        "databasecenter",
        "databaseinsights",
        "networkmanagement",
        "backupdr",
        "apihub",
        "cloudlocationfinder",
        "memorystore",
        "saasservicemgmt",
        "bigquerydatatransfer",
        "bigquerymigration",
        "datamigration",
        "datastream",
        "oracledatabase",
        "agentregistry",
        "cloudproductregistry",
        "cloudbilling",
        "datalineage",
        "androidmanagement",
        "design",
        "homedevelopers",
        "mapscodeassist",
        "paydeveloper",
        "stitch",
    ],
    manual: [
        // Customer Experience Agent Studio is served only from continental
        // `rep` hosts; there is no `ces.googleapis.com/mcp`. The US one is
        // pinned, matching the docs page. The EU variant,
        // `ces.eu.rep.googleapis.com/mcp`, answers discovery as well (probed
        // 2026-09-01) and is deliberately not a second entry: tool metadata
        // is host-invariant, but every call goes to the pinned host, so an
        // EU data-residency deployment needs the regional/rep routing
        // follow-up (a per-service host override), not a registry row.
        Endpoint {
            service_id: "ces",
            host: "ces.us.rep.googleapis.com",
            api_name: "ces.googleapis.com",
            mcp_path: "/mcp",
        },
        vertex_suite!("endpoints"),
        vertex_suite!("evaluation"),
        vertex_suite!("generate"),
        vertex_suite!("models"),
        // The notebook suite serves 49-char Colab Enterprise tool names, and
        // `vertex-notebook__{tool}` would break the 64-char namespaced-name
        // limit ([`crate::catalog::MAX_TOOL_NAME_LEN`]); 13 id chars is the
        // budget, so this one suite gets the short prefix.
        Endpoint {
            service_id: "vtx-notebook",
            host: "aiplatform.googleapis.com",
            api_name: "aiplatform.googleapis.com",
            mcp_path: "/mcp/notebook",
        },
        vertex_suite!("predict"),
        vertex_suite!("prompts"),
        vertex_suite!("retrieval"),
        vertex_suite!("tuning"),
    ],
];

/// Look up an endpoint by its service id.
pub fn find(service_id: &str) -> Option<&'static Endpoint> {
    ENDPOINTS.iter().find(|e| e.service_id == service_id)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Entries whose host or path is not derived from the service id.
    const MANUAL_IDS: &[&str] = &[
        "ces",
        "vertex-endpoints",
        "vertex-evaluation",
        "vertex-generate",
        "vertex-models",
        "vtx-notebook",
        "vertex-predict",
        "vertex-prompts",
        "vertex-retrieval",
        "vertex-tuning",
    ];

    #[test]
    fn registry_holds_every_probed_endpoint() {
        assert_eq!(ENDPOINTS.len(), 65, "registry must pin all 65 probed endpoints");
        assert_eq!(MANUAL_IDS.len(), 10, "manual entries: ces plus 9 Vertex suites");
    }

    #[test]
    fn service_ids_and_urls_are_unique() {
        let ids: HashSet<_> = ENDPOINTS.iter().map(|e| e.service_id).collect();
        assert_eq!(ids.len(), ENDPOINTS.len(), "duplicate service_id in registry");

        // The 9 Vertex suites share one host, so hosts alone are not unique;
        // the served URL (host + path) must be.
        let urls: HashSet<_> = ENDPOINTS.iter().map(|e| (e.host, e.mcp_path)).collect();
        assert_eq!(urls.len(), ENDPOINTS.len(), "duplicate host+path in registry");
    }

    #[test]
    fn derived_fields_follow_the_probed_shape() {
        for endpoint in ENDPOINTS.iter().filter(|e| !MANUAL_IDS.contains(&e.service_id)) {
            assert_eq!(endpoint.host, format!("{}.googleapis.com", endpoint.service_id));
            assert_eq!(endpoint.api_name, endpoint.host);
            assert_eq!(endpoint.mcp_path, "/mcp");
            assert_eq!(endpoint.mcp_url(), format!("https://{}/mcp", endpoint.host));
        }
    }

    #[test]
    fn manual_entries_keep_the_registry_invariants() {
        for id in MANUAL_IDS {
            let endpoint = find(id).unwrap_or_else(|| panic!("manual entry `{id}` missing"));
            assert!(endpoint.host.ends_with(".googleapis.com"), "host leaves Google: {id}");
            assert!(endpoint.api_name.ends_with(".googleapis.com"), "api_name shape: {id}");
            assert!(endpoint.mcp_path.starts_with("/mcp"), "path off the MCP mount: {id}");
        }
        assert_eq!(
            find("ces").map(|e| e.mcp_url()),
            Some("https://ces.us.rep.googleapis.com/mcp".to_owned())
        );
        assert_eq!(
            find("vertex-generate").map(|e| e.mcp_url()),
            Some("https://aiplatform.googleapis.com/mcp/generate".to_owned())
        );
    }

    #[test]
    fn service_ids_never_break_namespacing() {
        for endpoint in ENDPOINTS {
            assert!(
                !endpoint.service_id.contains("__"),
                "service id `{}` would break `{{service}}__{{tool}}` splitting",
                endpoint.service_id
            );
        }
    }

    #[test]
    fn find_resolves_known_ids_and_rejects_unknown() {
        assert_eq!(find("run").map(|e| e.host), Some("run.googleapis.com"));
        assert_eq!(
            find("developerknowledge").map(|e| e.api_name),
            Some("developerknowledge.googleapis.com")
        );
        assert_eq!(find("stitch").map(|e| e.host), Some("stitch.googleapis.com"));
        assert_eq!(find("ces").map(|e| e.host), Some("ces.us.rep.googleapis.com"));
        assert_eq!(find("vertex-generate").map(|e| e.mcp_path), Some("/mcp/generate"));
        assert!(find("nonexistent").is_none());
        assert!(find("").is_none());
    }
}
