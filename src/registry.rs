//! Static registry of Google Cloud remote MCP endpoints.

/// A single remote MCP endpoint served under `https://{host}/mcp`.
pub struct Endpoint {
    /// Short service identifier used as the tool-name prefix (e.g. `run`).
    pub service_id: &'static str,
    /// Endpoint host (e.g. `run.googleapis.com`).
    pub host: &'static str,
    /// Service Usage API name used for enablement matching.
    pub api_name: &'static str,
}

impl Endpoint {
    /// Full MCP URL this endpoint is served from.
    pub fn mcp_url(&self) -> String {
        format!("https://{}/mcp", self.host)
    }
}

/// Expands bare service ids into [`Endpoint`] entries.
///
/// Every probed host follows `{service_id}.googleapis.com`, and Service Usage
/// reports that same string as the API name, so both fields are derived from
/// the id at compile time rather than repeated by hand.
macro_rules! endpoints {
    ($($service_id:literal),+ $(,)?) => {
        &[$(Endpoint {
            service_id: $service_id,
            host: concat!($service_id, ".googleapis.com"),
            api_name: concat!($service_id, ".googleapis.com"),
        }),+]
    };
}

/// Evidence-pinned endpoint table.
///
/// All 47 hosts were probed live on 2026-08-19 and answered MCP `initialize`
/// plus `tools/list` at `https://{host}/mcp` without credentials.
pub static ENDPOINTS: &[Endpoint] = endpoints![
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
];

/// Look up an endpoint by its service id.
pub fn find(service_id: &str) -> Option<&'static Endpoint> {
    ENDPOINTS.iter().find(|e| e.service_id == service_id)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn registry_holds_every_probed_endpoint() {
        assert_eq!(ENDPOINTS.len(), 47, "registry must pin all 47 probed hosts");
    }

    #[test]
    fn service_ids_and_hosts_are_unique() {
        let ids: HashSet<_> = ENDPOINTS.iter().map(|e| e.service_id).collect();
        assert_eq!(
            ids.len(),
            ENDPOINTS.len(),
            "duplicate service_id in registry"
        );

        let hosts: HashSet<_> = ENDPOINTS.iter().map(|e| e.host).collect();
        assert_eq!(hosts.len(), ENDPOINTS.len(), "duplicate host in registry");
    }

    #[test]
    fn derived_fields_follow_the_probed_shape() {
        for endpoint in ENDPOINTS {
            assert_eq!(
                endpoint.host,
                format!("{}.googleapis.com", endpoint.service_id)
            );
            assert_eq!(endpoint.api_name, endpoint.host);
            assert_eq!(endpoint.mcp_url(), format!("https://{}/mcp", endpoint.host));
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
        assert!(find("nonexistent").is_none());
        assert!(find("").is_none());
    }
}
