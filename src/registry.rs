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

/// One of the ten Vertex AI suites, mounted at `/mcp/{suite}` on the global
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
/// 47 derived hosts on 2026-08-19, the 8 newer derived hosts plus the 10
/// manual entries then needed on 2026-08-31, `designcenter` on 2026-09-07,
/// and the 13 found by sweeping every host in Google's public API discovery
/// document on 2026-09-07: 11 derived, the Cloud Storage mount and a tenth
/// Vertex suite. `ces` moved from its US `rep` host to the global one in the
/// same sweep, which is what made it derived.
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
        // Design Center, distinct from `design`: it serves application and
        // catalog management rather than fonts and icons, and unlike `design`
        // its host is a Service Usage API name, so enablement pruning can
        // reach it.
        "designcenter",
        // Found by the 2026-09-07 discovery-document sweep. Every one answers
        // `initialize` plus `tools/list` on the derived host and path.
        "accessapproval",
        "apigee",
        "billingbudgets",
        // Customer Experience Agent Studio. It was pinned to `ces.us.rep`
        // because no global host answered on 2026-08-31; one does now, and
        // it serves the same 60 tool names, so the entry is derived like the
        // rest and calls no longer single out one continent.
        "ces",
        "contactcenterinsights",
        "developerconnect",
        "dialogflow",
        "firebasedataconnect",
        "geminidataanalytics",
        "iam",
        "maintenance",
        "managedkafka",
    ],
    manual: [
        // Cloud Storage mounts MCP under the service's own URL prefix rather
        // than at the host root, so this is the one entry whose path is
        // neither `/mcp` nor `/mcp/{suite}`.
        Endpoint {
            service_id: "storage",
            host: "storage.googleapis.com",
            api_name: "storage.googleapis.com",
            mcp_path: "/storage/mcp",
        },
        vertex_suite!("agents"),
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
        "storage",
        "vertex-agents",
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
        assert_eq!(ENDPOINTS.len(), 79, "registry must pin all 79 probed endpoints");
        assert_eq!(
            MANUAL_IDS.len(),
            11,
            "manual entries: the Cloud Storage mount plus 10 Vertex suites"
        );
    }

    #[test]
    fn service_ids_and_urls_are_unique() {
        let ids: HashSet<_> = ENDPOINTS.iter().map(|e| e.service_id).collect();
        assert_eq!(ids.len(), ENDPOINTS.len(), "duplicate service_id in registry");

        // The 10 Vertex suites share one host, so hosts alone are not unique;
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
            // Cloud Storage mounts at `/storage/mcp`, the Vertex suites at
            // `/mcp/{suite}`, so the shared shape is the trailing segment.
            assert!(
                endpoint.mcp_path.starts_with("/mcp") || endpoint.mcp_path.ends_with("/mcp"),
                "path off the MCP mount: {id}"
            );
        }
        assert_eq!(
            find("storage").map(|e| e.mcp_url()),
            Some("https://storage.googleapis.com/storage/mcp".to_owned())
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
        assert_eq!(find("ces").map(|e| e.host), Some("ces.googleapis.com"));
        assert_eq!(find("vertex-generate").map(|e| e.mcp_path), Some("/mcp/generate"));
        assert!(find("nonexistent").is_none());
        assert!(find("").is_none());
    }
}
