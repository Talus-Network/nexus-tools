//! Input validation policy for the Walrus tools.
//!
//! Anyone who can submit a DAG controls `publisher_url` / `aggregator_url` and
//! `upload-file`'s `file_path`. Picking your own Walrus endpoint is the point of
//! the URL ports, so they stay open to any *public* endpoint and closed to the
//! private side of the network — the cloud metadata server, the container's
//! loopback, the VPC. `file_path` has no such legitimate use on a hosted tool
//! and is off unless an upload root is configured.
//!
//! The endpoint policy is in two parts because refusing `169.254.169.254` and
//! `metadata.google.internal` by name is a one-line bypass away from useless:
//! any public name can resolve to a private address. So the host is also looked
//! up, and the addresses it answered with are pinned onto the HTTP client —
//! otherwise the connection does its own lookup and a name with alternating
//! records passes the check, then connects to the private address.

pub mod validation {
    use {
        reqwest::Url,
        serde::{de, Deserialize, Deserializer},
        std::{
            net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
            path::{Component, Path, PathBuf},
            time::Duration,
        },
    };

    /// Unset (the default) refuses local-path uploads entirely.
    const ENV_UPLOAD_ROOT: &str = "WALRUS_UPLOAD_ROOT";

    /// `.internal` is where the GCE metadata server lives.
    const PRIVATE_DOMAIN_SUFFIXES: &[&str] =
        &[".internal", ".local", ".localhost", ".home.arpa", ".arpa"];

    /// Keeps a caller from parking a request on an unresponsive resolver for the
    /// whole tool timeout.
    const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

    /// An endpoint a caller named that the tool will not talk to.
    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    pub struct EndpointError(String);

    /// Rejecting here rather than inside `invoke` keeps the tools' output schemas
    /// unchanged. The resolved half of the policy runs in
    /// [`crate::client::WalrusConfig::build`], which has an async context to do
    /// the lookup in.
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

    /// The half of the policy that needs no I/O.
    ///
    /// The query and fragment rules are what they are because the SDK builds
    /// request URLs by string concatenation (`{base}/v1/blobs/{id}`): a base
    /// carrying either one swallows everything appended after it, so
    /// `http://host/#` reads the host's root instead of a blob. That is how the
    /// 2026-09-30 metadata probes got a response at all. A path concatenates the
    /// way the SDK expects, so an aggregator served under a prefix still works.
    pub(crate) fn check_endpoint_url(raw: &str) -> Result<(), EndpointError> {
        let url = Url::parse(raw).map_err(|e| EndpointError(e.to_string()))?;

        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(EndpointError(format!(
                "endpoint scheme `{scheme}` is not allowed"
            )));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(EndpointError(
                "endpoint must not carry credentials".to_string(),
            ));
        }
        if url.query().is_some() {
            return Err(EndpointError(
                "endpoint must not carry a query string".to_string(),
            ));
        }
        if url.fragment().is_some() {
            return Err(EndpointError(
                "endpoint must not carry a fragment".to_string(),
            ));
        }

        let Some(host) = url.host_str() else {
            return Err(EndpointError("endpoint must have a host".to_string()));
        };
        check_endpoint_host(host)
    }

    /// The hosts that can be refused without resolving them.
    ///
    /// The single-label rule is the non-obvious one: a bare `metadata` resolves
    /// through the container's DNS search list, which on GCE ends at
    /// `google.internal`, so a name with no dot in it reaches the metadata
    /// server.
    fn check_endpoint_host(host: &str) -> Result<(), EndpointError> {
        if let Some(ip) = host_as_ip(host) {
            return if is_public_ip(ip) {
                Ok(())
            } else {
                Err(EndpointError(format!(
                    "endpoint address `{ip}` is not a public address"
                )))
            };
        }

        let host = host.to_ascii_lowercase();
        if host == "localhost" {
            return Err(EndpointError(
                "endpoint host `localhost` is not a public host".to_string(),
            ));
        }
        if let Some(suffix) = PRIVATE_DOMAIN_SUFFIXES
            .iter()
            .find(|suffix| host.ends_with(*suffix))
        {
            return Err(EndpointError(format!(
                "endpoint host `{host}` is in the internal domain `{suffix}`"
            )));
        }
        if !host.contains('.') {
            return Err(EndpointError(format!(
                "endpoint host `{host}` is a single-label name,                  which resolves through the local search domain"
            )));
        }

        Ok(())
    }

    /// Resolve `raw`'s host, refusing it unless every address it answers with is
    /// public, and return the addresses for the caller to pin onto its HTTP
    /// client.
    ///
    /// `None` means there is nothing to pin: an IP literal needs no lookup, and
    /// [`check_endpoint_url`] has already settled whether it is public.
    pub(crate) async fn resolve_public_endpoint(
        raw: &str,
    ) -> Result<Option<(String, Vec<SocketAddr>)>, EndpointError> {
        let url = Url::parse(raw).map_err(|e| EndpointError(e.to_string()))?;
        let Some(host) = url.host_str() else {
            return Err(EndpointError("endpoint must have a host".to_string()));
        };
        if host_as_ip(host).is_some() {
            return Ok(None);
        }

        let port = url.port_or_known_default().unwrap_or(443);
        let lookup = tokio::net::lookup_host((host, port));
        let addrs: Vec<SocketAddr> = tokio::time::timeout(RESOLVE_TIMEOUT, lookup)
            .await
            .map_err(|_| EndpointError(format!("endpoint host `{host}` did not resolve in time")))?
            .map_err(|e| EndpointError(format!("endpoint host `{host}` did not resolve: {e}")))?
            .collect();

        if addrs.is_empty() {
            return Err(EndpointError(format!(
                "endpoint host `{host}` resolved to no addresses"
            )));
        }
        if let Some(addr) = addrs.iter().find(|addr| !is_public_ip(addr.ip())) {
            return Err(EndpointError(format!(
                "endpoint host `{host}` resolves to `{}`, which is not a public address",
                addr.ip()
            )));
        }

        Ok(Some((host.to_string(), addrs)))
    }

    /// Parse `host` as an IP literal, accepting the bracketed IPv6 form `Url`
    /// produces. Oddities like `http://2130706433/` need no handling: `Url`
    /// normalizes those to dotted-quad while parsing.
    fn host_as_ip(host: &str) -> Option<IpAddr> {
        host.strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host)
            .parse()
            .ok()
    }

    /// True if `addr` is routable on the public internet. The ranges are spelled
    /// out because `IpAddr::is_global` is still unstable.
    pub(crate) fn is_public_ip(addr: IpAddr) -> bool {
        match addr {
            IpAddr::V4(ip) => is_public_ipv4(ip),
            IpAddr::V6(ip) => is_public_ipv6(ip),
        }
    }

    fn is_public_ipv4(ip: Ipv4Addr) -> bool {
        let o = ip.octets();
        !(ip.is_loopback()
            || ip.is_private()
            // 169.254.0.0/16 link-local, where every cloud metadata server lives
            || ip.is_link_local()
            || ip.is_multicast()
            || ip.is_documentation()
            // 0.0.0.0/8 "this network", which includes the unspecified address
            || o[0] == 0
            // 100.64.0.0/10 carrier-grade NAT
            || (o[0] == 100 && (o[1] & 0xc0) == 64)
            // 192.0.0.0/24 IETF protocol assignments
            || (o[0] == 192 && o[1] == 0 && o[2] == 0)
            // 192.88.99.0/24 former 6to4 relay anycast
            || (o[0] == 192 && o[1] == 88 && o[2] == 99)
            // 198.18.0.0/15 benchmarking
            || (o[0] == 198 && (o[1] & 0xfe) == 18)
            // 240.0.0.0/4 reserved, up to and including the broadcast address
            || o[0] >= 240)
    }

    fn is_public_ipv6(ip: Ipv6Addr) -> bool {
        // A v6 address carrying a v4 one reaches that v4 address, so the v4
        // ranges are what decide.
        if let Some(v4) = embedded_ipv4(ip) {
            return is_public_ipv4(v4);
        }

        let s = ip.segments();
        !(ip.is_unspecified()
            || ip.is_loopback()
            || ip.is_multicast()
            // fc00::/7 unique local
            || (s[0] & 0xfe00) == 0xfc00
            // fe80::/10 link-local
            || (s[0] & 0xffc0) == 0xfe80
            // 2001:db8::/32 documentation
            || (s[0] == 0x2001 && s[1] == 0x0db8)
            // 100::/64 discard-only
            || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0))
    }

    /// The v4 address a v6 address stands in for, across the mapped, compatible,
    /// 6to4 and NAT64 forms.
    fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
        // Both ::ffff:a.b.c.d (mapped) and ::a.b.c.d (compatible).
        if let Some(v4) = ip.to_ipv4() {
            return Some(v4);
        }

        let s = ip.segments();
        let embedded = |hi: u16, lo: u16| {
            Ipv4Addr::new(
                (hi >> 8) as u8,
                (hi & 0xff) as u8,
                (lo >> 8) as u8,
                (lo & 0xff) as u8,
            )
        };

        // 2002::/16 6to4
        if s[0] == 0x2002 {
            return Some(embedded(s[1], s[2]));
        }
        // 64:ff9b::/96 NAT64
        if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
            return Some(embedded(s[6], s[7]));
        }

        None
    }

    /// [`resolve_upload_path_in`] with the root read from the environment.
    pub(crate) fn resolve_upload_path(file_path: &str) -> Result<PathBuf, String> {
        let root = std::env::var(ENV_UPLOAD_ROOT)
            .ok()
            .filter(|v| !v.trim().is_empty());
        resolve_upload_path_in(root.as_deref(), file_path)
    }

    /// Resolve `upload-file`'s `file_path` to a real file inside `root`.
    ///
    /// With no `root` configured the port is refused outright: a hosted instance
    /// of this tool has its signing key on a mounted volume and nothing a caller
    /// would legitimately want published, so a local-read port there is only an
    /// exfiltration primitive. It has to be turned on deliberately by whoever
    /// actually has files to publish.
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

        // --- endpoint policy: shape ---

        #[test]
        fn public_walrus_endpoints_are_accepted() {
            for url in [
                "https://publisher.walrus-testnet.walrus.space",
                "https://aggregator.walrus-mainnet.walrus.space",
                "https://walrus-mainnet-publisher-1.staketab.org",
                // Plaintext and odd ports are common among community operators.
                "http://walrus-testnet.suicore.com",
                "https://walrus.example.com:9000",
                "https://cdn.example.com/walrus",
            ] {
                assert!(
                    check_endpoint_url(url).is_ok(),
                    "expected {url} to be accepted: {:?}",
                    check_endpoint_url(url).err()
                );
            }
        }

        #[test]
        fn metadata_servers_are_refused() {
            // The 2026-09-30 probes, verbatim.
            for url in [
                "http://169.254.169.254/#",
                "http://metadata.google.internal/#",
                "https://metadata.google.internal",
                "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
                // ECS task metadata, and Alibaba's.
                "http://169.254.170.2/v2/credentials/",
                "http://100.100.100.200/latest/meta-data/",
                // Decimal and hex spellings of 169.254.169.254.
                "http://2852039166/",
                "http://0xA9FEA9FE/",
                "http://metadata/computeMetadata/v1/",
            ] {
                assert!(
                    check_endpoint_url(url).is_err(),
                    "expected {url} to be refused"
                );
            }
        }

        #[test]
        fn loopback_and_private_ranges_are_refused() {
            for url in [
                "http://127.0.0.1:8080",
                "http://localhost:8080",
                "https://LOCALHOST",
                "http://[::1]:8080",
                "http://10.0.0.5",
                "http://172.16.0.1",
                "http://192.168.1.1",
                "http://0.0.0.0",
                "http://[fd00::1]",
                "http://[fe80::1]",
                "http://[::ffff:169.254.169.254]",
                "http://[::ffff:127.0.0.1]",
                "http://[2002:a9fe:a9fe::]",
                "http://[64:ff9b::a9fe:a9fe]",
            ] {
                assert!(
                    check_endpoint_url(url).is_err(),
                    "expected {url} to be refused"
                );
            }
        }

        #[test]
        fn internal_domain_suffixes_are_refused() {
            for url in [
                "https://walrus.tools.internal",
                "http://walrus.nexus.local",
                "http://publisher.localhost",
                "https://1.0.0.127.in-addr.arpa",
            ] {
                assert!(
                    check_endpoint_url(url).is_err(),
                    "expected {url} to be refused"
                );
            }
        }

        #[test]
        fn concatenation_breaking_shapes_are_refused() {
            let host = "https://aggregator.walrus-testnet.walrus.space";
            for url in [
                format!("{host}/#"),
                format!("{host}?a=b"),
                format!("{host}#frag"),
            ] {
                assert!(
                    check_endpoint_url(&url).is_err(),
                    "expected {url} to be refused"
                );
            }
            // Credentials put the real target on the right of the `@` while a
            // plausible host sits on the left.
            assert!(check_endpoint_url(
                "https://aggregator.walrus-testnet.walrus.space@169.254.169.254"
            )
            .is_err());
        }

        #[test]
        fn non_http_schemes_are_refused() {
            for url in [
                "file:///etc/passwd",
                "gopher://example.com:70/",
                "ftp://example.com/",
            ] {
                assert!(
                    check_endpoint_url(url).is_err(),
                    "expected {url} to be refused"
                );
            }
        }

        // --- endpoint policy: address classification ---

        #[test]
        fn public_addresses_are_recognised() {
            for addr in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
                let ip: IpAddr = addr.parse().unwrap();
                assert!(is_public_ip(ip), "expected {addr} to be public");
            }
        }

        #[test]
        fn non_public_ranges_are_recognised() {
            for addr in [
                "0.0.0.0",
                "0.1.2.3",
                "10.0.0.1",
                "100.100.100.200",
                "127.0.0.1",
                "169.254.169.254",
                "172.31.255.255",
                "192.0.0.1",
                "192.0.2.1",
                "192.88.99.1",
                "192.168.0.1",
                "198.18.0.1",
                "198.51.100.1",
                "203.0.113.1",
                "224.0.0.1",
                "240.0.0.1",
                "255.255.255.255",
                "::",
                "::1",
                "fc00::1",
                "fd12:3456::1",
                "fe80::1",
                "ff02::1",
                "2001:db8::1",
                "100::1",
            ] {
                let ip: IpAddr = addr.parse().unwrap();
                assert!(!is_public_ip(ip), "expected {addr} to be non-public");
            }
        }

        // --- endpoint policy: resolution ---

        #[tokio::test]
        async fn an_ip_literal_endpoint_needs_no_pin() {
            assert_eq!(
                resolve_public_endpoint("https://8.8.8.8").await.unwrap(),
                None
            );
        }

        #[tokio::test]
        async fn a_name_resolving_to_a_private_address_is_refused() {
            // localhost is the one name guaranteed to resolve to loopback
            // everywhere; the shape being tested is a name with private records.
            let err = resolve_public_endpoint("http://localhost:8080")
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("not a public address"), "{err}");
        }

        #[tokio::test]
        async fn a_name_that_does_not_resolve_is_refused() {
            let err = resolve_public_endpoint("https://no-such-host.invalid")
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("did not resolve"), "{err}");
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
