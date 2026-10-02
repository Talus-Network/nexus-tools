//! Input validation policy for the Walrus tools.
//!
//! Every tool in this crate takes a caller-supplied endpoint URL, and
//! `upload-file` takes a caller-supplied filesystem path. Both are reachable
//! by anyone who can submit a DAG, so both are treated as untrusted input:
//!
//! * [`validation::deserialize_url_opt`] confines `publisher_url` /
//!   `aggregator_url` to an allowlist of hosts, so the ports cannot be used to
//!   reach the cloud metadata server, the container's own loopback, or an
//!   arbitrary host on the internet.
//! * [`validation::resolve_upload_path`] confines `upload-file`'s `file_path`
//!   to one configured directory, so the port cannot be used to read the
//!   container's mounted secrets and publish them to Walrus.
//!
//! Both policies are deny-by-default: the endpoint allowlist is a compile-time
//! constant holding only known Walrus hosts, and local-path uploads are refused
//! outright until an upload root is configured.

pub mod validation {
    use {
        reqwest::Url,
        serde::{de, Deserialize, Deserializer},
        std::path::{Component, Path, PathBuf},
    };

    /// Ops-configured default endpoints (see [`crate::client`]). Their hosts
    /// are allowed implicitly — a caller passing the URL the tool would have
    /// used anyway must not be refused.
    const ENV_PUBLISHER_URL: &str = "WALRUS_PUBLISHER_URL";
    const ENV_AGGREGATOR_URL: &str = "WALRUS_AGGREGATOR_URL";

    /// The directory `upload-file` may read from. Unset (the default) means
    /// local-path uploads are refused entirely.
    const ENV_UPLOAD_ROOT: &str = "WALRUS_UPLOAD_ROOT";

    /// The Walrus endpoints a caller may name. Deliberately a compile-time
    /// list and not a knob: which storage network these tools will talk to is a
    /// property of the tool, so widening it is a reviewed code change rather
    /// than a deployment variable someone can quietly set.
    ///
    /// An entry is an exact host, or — with a leading dot — that domain and its
    /// subdomains.
    ///
    ///   * `walrus.space` is Mysten's domain, carrying the testnet and mainnet
    ///     publishers and aggregators, including the SDK's defaults.
    ///   * `walrus-mainnet-publisher-1.staketab.org` is the mainnet publisher
    ///     the leader is configured against (tf-talus-nexus-v2, mainnet-v2
    ///     `publisher_url`); the hosted tools set no `WALRUS_PUBLISHER_URL`, so
    ///     mainnet uploads name it in the input port.
    const ALLOWED_HOSTS: &[&str] = &[
        "walrus.space",
        ".walrus.space",
        "walrus-mainnet-publisher-1.staketab.org",
    ];

    /// Deserializer for the optional `publisher_url` / `aggregator_url` input
    /// ports. Rejecting here rather than inside `invoke` keeps the tools'
    /// output schemas unchanged and means a refused endpoint never reaches the
    /// HTTP client at all.
    pub fn deserialize_url_opt<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<String>::deserialize(deserializer)?;
        if let Some(ref s) = opt {
            check_endpoint_url(s).map_err(de::Error::custom)?;
        }
        Ok(opt)
    }

    /// [`check_endpoint_url_against`] with the deployment's own endpoint hosts
    /// read from the environment.
    pub(crate) fn check_endpoint_url(raw: &str) -> Result<(), String> {
        check_endpoint_url_against(&deployment_endpoint_hosts(), raw)
    }

    /// Accept `raw` only if it is a bare `https://host[:port]` whose host is on
    /// [`ALLOWED_HOSTS`] or in `deployment_hosts`.
    ///
    /// The shape restrictions are not cosmetic. The SDK builds request URLs by
    /// string concatenation (`{base}/v1/blobs/{id}`), so a base carrying a
    /// fragment swallows everything appended after it: `https://host/#` reads
    /// the host's root instead of a blob. Refusing any path, query, fragment or
    /// userinfo leaves exactly one thing for the allowlist to decide, which is
    /// the host.
    pub(crate) fn check_endpoint_url_against(
        deployment_hosts: &[String],
        raw: &str,
    ) -> Result<(), String> {
        let url = Url::parse(raw).map_err(|e| e.to_string())?;

        if url.scheme() != "https" {
            return Err(format!("endpoint must use https, got `{}`", url.scheme()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("endpoint must not carry credentials".to_string());
        }
        if url.query().is_some() {
            return Err("endpoint must not carry a query string".to_string());
        }
        if url.fragment().is_some() {
            return Err("endpoint must not carry a fragment".to_string());
        }
        if !url.path().is_empty() && url.path() != "/" {
            return Err(format!(
                "endpoint must not carry a path, got `{}`",
                url.path()
            ));
        }

        let Some(host) = url.host_str() else {
            return Err("endpoint must have a host".to_string());
        };
        let host = host.to_ascii_lowercase();

        let allowed = ALLOWED_HOSTS.iter().any(|entry| host_matches(&host, entry))
            || deployment_hosts.contains(&host);
        if !allowed {
            return Err(format!(
                "endpoint host `{host}` is not a known Walrus endpoint"
            ));
        }

        Ok(())
    }

    /// Hosts of the endpoints this deployment is configured to use. A caller
    /// naming the URL the tool would have used anyway must not be refused, and
    /// these are set by whoever deployed the tool rather than by the caller.
    fn deployment_endpoint_hosts() -> Vec<String> {
        [ENV_PUBLISHER_URL, ENV_AGGREGATOR_URL]
            .iter()
            .filter_map(|var| std::env::var(var).ok())
            .filter_map(|raw| {
                Url::parse(&raw)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            })
            .filter(|host| !host.is_empty())
            .collect()
    }

    /// `host` matches `entry` exactly, or — for a dot-prefixed `entry` — is
    /// that domain or any subdomain of it. Both are expected lowercase.
    fn host_matches(host: &str, entry: &str) -> bool {
        match entry.strip_prefix('.') {
            Some(domain) => host == domain || host.ends_with(entry),
            None => host == entry,
        }
    }

    /// [`resolve_upload_path_in`] with the upload root read from the
    /// environment.
    pub(crate) fn resolve_upload_path(file_path: &str) -> Result<PathBuf, String> {
        let root = std::env::var(ENV_UPLOAD_ROOT)
            .ok()
            .filter(|v| !v.trim().is_empty());
        resolve_upload_path_in(root.as_deref(), file_path)
    }

    /// Resolve `upload-file`'s `file_path` input to a real file inside `root`.
    ///
    /// `file_path` is relative to `root` and may not escape it. Containment is
    /// checked after `canonicalize`, so a symlink pointing out of the root is
    /// rejected along with a literal `../`.
    ///
    /// With no `root` configured the port is refused outright. The container
    /// this tool runs in holds its signing key on a mounted volume and nothing
    /// a caller would legitimately want to upload, so a reachable local-read
    /// port is a secret-exfiltration primitive and nothing else; it has to be
    /// turned on deliberately by whoever actually has files to publish.
    pub(crate) fn resolve_upload_path_in(
        root: Option<&str>,
        file_path: &str,
    ) -> Result<PathBuf, String> {
        let Some(root) = root else {
            return Err(format!(
                "uploading from a local path is disabled; \
                 set {ENV_UPLOAD_ROOT} to the directory this tool may read from"
            ));
        };

        let root = Path::new(root)
            .canonicalize()
            .map_err(|e| format!("{ENV_UPLOAD_ROOT} `{root}` is not usable: {e}"))?;

        let candidate = Path::new(file_path);
        if candidate
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(format!(
                "file_path must be a relative path inside {ENV_UPLOAD_ROOT}, got `{file_path}`"
            ));
        }

        let resolved = root
            .join(candidate)
            .canonicalize()
            .map_err(|_| format!("File does not exist: {file_path}"))?;

        if !resolved.starts_with(&root) {
            return Err(format!(
                "file_path resolves outside {ENV_UPLOAD_ROOT}: `{file_path}`"
            ));
        }
        if !resolved.is_file() {
            return Err(format!("Not a regular file: {file_path}"));
        }

        Ok(resolved)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Stand-in for the hosts a deployment's own `WALRUS_PUBLISHER_URL` /
        /// `WALRUS_AGGREGATOR_URL` contribute.
        fn deployed(entries: &[&str]) -> Vec<String> {
            entries.iter().map(|e| e.to_string()).collect()
        }

        // --- endpoint policy ---

        #[test]
        fn official_walrus_endpoints_are_allowed() {
            for url in [
                "https://publisher.walrus-testnet.walrus.space",
                "https://aggregator.walrus-testnet.walrus.space",
                "https://aggregator.walrus-mainnet.walrus.space",
                "https://walrus-mainnet-publisher-1.staketab.org",
                "https://walrus.space",
            ] {
                assert!(
                    check_endpoint_url_against(&[], url).is_ok(),
                    "expected {url} to be allowed"
                );
            }
        }

        #[test]
        fn metadata_server_is_refused() {
            // The 2026-09-30 probes, verbatim. The trailing `#` is what made
            // the SDK's `{base}/v1/blobs/{id}` concatenation read the host root.
            for url in [
                "http://169.254.169.254/#",
                "http://metadata.google.internal/#",
                "https://metadata.google.internal",
                "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
            ] {
                assert!(
                    check_endpoint_url_against(&[], url).is_err(),
                    "expected {url} to be refused"
                );
            }
        }

        #[test]
        fn loopback_is_refused() {
            for url in [
                "http://127.0.0.1:8080",
                "https://127.0.0.1:8080",
                "http://localhost:8080",
                "https://localhost",
            ] {
                assert!(
                    check_endpoint_url_against(&[], url).is_err(),
                    "expected {url} to be refused"
                );
            }
        }

        #[test]
        fn arbitrary_internet_hosts_are_refused() {
            for url in [
                "https://abc123.oast.site",
                "https://webhook.site/deadbeef",
                "https://evil.example.com",
            ] {
                assert!(
                    check_endpoint_url_against(&[], url).is_err(),
                    "expected {url} to be refused"
                );
            }
        }

        #[test]
        fn lookalike_domains_do_not_match_the_suffix() {
            for url in [
                "https://notwalrus.space",
                "https://walrus.space.evil.com",
                "https://walrus-space.evil.com",
                "https://walrus-mainnet-publisher-1.staketab.org.evil.com",
            ] {
                assert!(
                    check_endpoint_url_against(&[], url).is_err(),
                    "expected {url} to be refused"
                );
            }
        }

        #[test]
        fn cloud_run_path_suffix_bypass_is_refused() {
            // Probed on 2026-10-01 against the substring `.run.app` check in
            // `client::is_cloud_run_url`.
            assert!(check_endpoint_url_against(&[], "https://evil.example.com/.run.app/").is_err());
            assert!(check_endpoint_url_against(
                &deployed(&["walrus-publisher-mainnet-oozmyfiqvq-ue.a.run.app"]),
                "https://evil.example.com/.run.app/"
            )
            .is_err());
        }

        #[test]
        fn the_deployments_own_publisher_is_allowed() {
            let host = "walrus-publisher-mainnet-oozmyfiqvq-ue.a.run.app";
            assert!(
                check_endpoint_url_against(&[], &format!("https://{host}")).is_err(),
                "a Cloud Run host is not allowed on its own"
            );
            assert!(
                check_endpoint_url_against(&deployed(&[host]), &format!("https://{host}")).is_ok(),
                "the host this deployment is configured with is allowed"
            );
            // Deployment hosts match exactly, never as a suffix, so configuring
            // one Cloud Run publisher does not open up every `*.run.app`.
            assert!(check_endpoint_url_against(
                &deployed(&[host]),
                "https://someone-elses-service-uc.a.run.app"
            )
            .is_err());
        }

        #[test]
        fn url_shape_is_restricted_even_for_allowed_hosts() {
            let host = "https://publisher.walrus-testnet.walrus.space";
            for url in [
                format!("{host}/#"),
                format!("{host}/v1/blobs"),
                format!("{host}?a=b"),
                format!("{host}#frag"),
            ] {
                assert!(
                    check_endpoint_url_against(&[], &url).is_err(),
                    "expected {url} to be refused"
                );
            }
            // Credentials in the authority would point the request elsewhere
            // while keeping an allowed host in the string.
            assert!(check_endpoint_url_against(
                &[],
                "https://publisher.walrus-testnet.walrus.space@evil.example.com"
            )
            .is_err());
        }

        #[test]
        fn only_https_is_accepted() {
            for url in [
                "http://publisher.walrus-testnet.walrus.space",
                "file:///etc/passwd",
                "gopher://walrus.space:70/",
                "ftp://walrus.space/",
            ] {
                assert!(
                    check_endpoint_url_against(&[], url).is_err(),
                    "expected {url} to be refused"
                );
            }
        }

        #[test]
        fn suffix_entries_match_subdomains_only_at_a_label_boundary() {
            assert!(host_matches("example.com", ".example.com"));
            assert!(host_matches("a.example.com", ".example.com"));
            assert!(!host_matches("notexample.com", ".example.com"));
            assert!(!host_matches("example.com.evil.com", ".example.com"));
            assert!(host_matches("example.com", "example.com"));
            assert!(!host_matches("a.example.com", "example.com"));
        }

        // --- upload path policy ---

        fn upload_root() -> PathBuf {
            let root =
                std::env::temp_dir().join(format!("walrus-upload-test-{}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            root.canonicalize().unwrap()
        }

        #[test]
        fn local_upload_is_disabled_without_a_root() {
            let err = resolve_upload_path_in(None, "/app/secrets/toolkit-config.json").unwrap_err();
            assert!(err.contains("disabled"), "{err}");
        }

        #[test]
        fn file_inside_the_root_resolves() {
            let root = upload_root();
            let file = root.join("payload.json");
            std::fs::write(&file, b"{}").unwrap();

            let resolved =
                resolve_upload_path_in(Some(root.to_str().unwrap()), "payload.json").unwrap();
            assert_eq!(resolved, file);
        }

        #[test]
        fn absolute_and_traversing_paths_are_refused() {
            let root = upload_root();
            let root = root.to_str().unwrap();

            for file_path in [
                "/app/secrets/toolkit-config.json",
                "/proc/self/environ",
                "/etc/passwd",
                "../../../etc/passwd",
                "nested/../../escape",
            ] {
                let err = resolve_upload_path_in(Some(root), file_path).unwrap_err();
                assert!(err.contains("relative path inside"), "{file_path}: {err}");
            }
        }

        #[test]
        fn symlink_out_of_the_root_is_refused() {
            let root = upload_root();
            let link = root.join("escape-link");
            let _ = std::fs::remove_file(&link);
            #[cfg(unix)]
            std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();

            let err =
                resolve_upload_path_in(Some(root.to_str().unwrap()), "escape-link").unwrap_err();
            assert!(err.contains("resolves outside"), "{err}");
        }

        #[test]
        fn a_directory_is_not_uploadable() {
            let root = upload_root();
            std::fs::create_dir_all(root.join("subdir")).unwrap();

            let err = resolve_upload_path_in(Some(root.to_str().unwrap()), "subdir").unwrap_err();
            assert!(err.contains("Not a regular file"), "{err}");
        }

        #[test]
        fn missing_file_inside_the_root_reports_absence() {
            let root = upload_root();
            let err =
                resolve_upload_path_in(Some(root.to_str().unwrap()), "nope.json").unwrap_err();
            assert!(err.contains("File does not exist"), "{err}");
        }

        #[test]
        fn an_unusable_root_is_reported_as_such() {
            let err = resolve_upload_path_in(Some("/nonexistent-upload-root"), "x").unwrap_err();
            assert!(err.contains("not usable"), "{err}");
        }
    }
}
