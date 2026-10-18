//! Downloading images safely.
//!
//! Image URLs come from tenants and the files are hostile until proven
//! otherwise:
//!
//! - Hosts sit on the management network. A tenant's URL must not make a host
//!   fetch from it (and hand the response back as a bootable image), so tenant
//!   downloads only connect to public addresses. The check runs on the
//!   addresses actually connected to, for redirects too, so DNS tricks do not
//!   get around it.
//! - A qcow2 file can point at other host files: a backing file, or an
//!   external data file. Converting it would copy those into the image. Both
//!   are refused.
//! - `qemu-img` parses the untrusted file under CPU and memory limits, and
//!   downloads are capped in size.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::{Error, Result};

/// What a download may do.
#[derive(Clone, Copy, Debug)]
pub struct FetchPolicy {
    /// Connect to private, loopback and other non-public addresses. For images
    /// the provider publishes, which may live on internal mirrors.
    pub allow_private: bool,
    /// Largest download, in bytes.
    pub max_bytes: u64,
    /// Largest disk the image may describe (its virtual size), in bytes:
    /// the disk it is for. A small download can unpack into a huge one
    /// (compressed clusters), so this is checked before converting, and the
    /// conversion may write no more.
    pub max_virtual_bytes: u64,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            allow_private: false,
            max_bytes: 64 << 30,
            max_virtual_bytes: 1 << 40,
        }
    }
}

/// Whether `ip` is an ordinary internet address: not private, loopback,
/// link-local, shared (CGNAT), multicast, reserved or for documentation.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || (s[0] == 0x0064 && s[1] == 0xff9b) // NAT64: an IPv4 in disguise
                || s[0] == 0x0100) // discard-only
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || a == 0
        || a >= 240 // reserved
        || (a == 100 && (64..128).contains(&b)) // shared address space (CGNAT)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2) // documentation
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || (a == 198 && (b == 18 || b == 19))) // benchmarking
}

/// Resolves names, keeping public addresses only. Address literals in URLs
/// bypass resolvers; check those with [`check_url`].
pub struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let all: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> =
                all.iter().copied().filter(|a| is_public(a.ip())).collect();
            if public.is_empty() {
                return Err(format!("{host} resolves to no public address").into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Checks a URL before connecting: http(s) only, and no non-public address
/// literals (names are checked when resolved).
pub fn check_url(url: &reqwest::Url, policy: FetchPolicy) -> std::result::Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("{}: only http and https are allowed", url.scheme()));
    }
    if policy.allow_private {
        return Ok(());
    }
    let ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip),
        Some(url::Host::Domain(_)) => return Ok(()),
        None => return Err("the URL has no host".into()),
    };
    if is_public(ip) {
        Ok(())
    } else {
        Err(format!("{ip} is not a public address"))
    }
}

fn client(policy: FetchPolicy) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        // Never via a proxy from the environment: it would do the connecting.
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if let Err(e) = check_url(attempt.url(), policy) {
                attempt.error(e)
            } else {
                attempt.follow()
            }
        }));
    if !policy.allow_private {
        b = b.dns_resolver(Arc::new(PublicOnly));
    }
    b.build().map_err(|e| Error::Download(e.to_string()))
}

/// Downloads `url` to `dest`. Returns the SHA-256 of what it downloaded
/// (hex).
pub(crate) async fn download(url: &str, dest: &Path, policy: FetchPolicy) -> Result<String> {
    use sha2::Digest;
    let parsed = reqwest::Url::parse(url).map_err(|e| Error::Download(format!("{url}: {e}")))?;
    check_url(&parsed, policy).map_err(|e| Error::Download(format!("{url}: {e}")))?;
    let resp = client(policy)?
        .get(parsed)
        .send()
        .await
        .map_err(|e| Error::Download(chain(&e)))?;
    if !resp.status().is_success() {
        return Err(Error::Download(format!("{url}: HTTP {}", resp.status())));
    }
    if resp.content_length().is_some_and(|n| n > policy.max_bytes) {
        return Err(Error::Download(format!(
            "{url}: larger than the {} GiB limit",
            policy.max_bytes >> 30
        )));
    }
    let mut file = tokio::fs::File::create(dest).await?;
    let mut body = resp.bytes_stream();
    let mut total = 0u64;
    let mut hash = sha2::Sha256::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| Error::Download(chain(&e)))?;
        total += chunk.len() as u64;
        if total > policy.max_bytes {
            return Err(Error::Download(format!(
                "{url}: larger than the {} GiB limit",
                policy.max_bytes >> 30
            )));
        }
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(hex::encode(hash.finalize()))
}

fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut s = e.source();
    while let Some(c) = s {
        out = format!("{out}: {c}");
        s = c.source();
    }
    out
}

/// `qemu-img` on an untrusted file, under limits: a crafted image must not
/// exhaust the host. `prlimit` (util-linux) applies them when present.
pub(crate) async fn untrusted_qemu_img(
    args: &[&str],
    cpu_secs: u64,
    mem_bytes: u64,
    file_bytes: Option<u64>,
) -> Result<Vec<u8>> {
    if which("prlimit") {
        let cpu = format!("--cpu={cpu_secs}");
        let mem = format!("--as={mem_bytes}");
        let fsize = file_bytes.map(|b| format!("--fsize={b}"));
        let mut full = vec![cpu.as_str(), mem.as_str()];
        full.extend(fsize.as_deref());
        full.extend(["--", "qemu-img"]);
        full.extend_from_slice(args);
        crate::run("prlimit", &full).await
    } else {
        tracing::warn!("prlimit is missing; running qemu-img on an untrusted image without limits");
        crate::run("qemu-img", args).await
    }
}

fn which(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

#[derive(Deserialize)]
struct Info {
    #[serde(rename = "virtual-size")]
    virtual_size: u64,
    #[serde(rename = "backing-filename")]
    backing_filename: Option<String>,
    #[serde(rename = "format-specific")]
    format_specific: Option<FormatSpecific>,
}

#[derive(Deserialize)]
struct FormatSpecific {
    data: Option<serde_json::Value>,
}

/// Refuses images that refer to other files. Returns the size of the disk
/// the image describes.
pub(crate) async fn check_untrusted(path: &Path, format: &str) -> Result<u64> {
    let p = path.to_string_lossy();
    let out = untrusted_qemu_img(
        &["info", "--output=json", "-f", format, &p],
        30,
        1 << 30,
        None,
    )
    .await?;
    let info: Info =
        serde_json::from_slice(&out).map_err(|e| Error::InvalidImage(e.to_string()))?;
    if info.backing_filename.is_some() {
        return Err(Error::InvalidImage(
            "image references a backing file".into(),
        ));
    }
    if info
        .format_specific
        .and_then(|f| f.data)
        .is_some_and(|d| d.get("data-file").is_some())
    {
        return Err(Error::InvalidImage(
            "image references an external data file".into(),
        ));
    }
    Ok(info.virtual_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses_are_public() {
        for ok in ["1.1.1.1", "151.101.2.132", "2a04:4e42::644"] {
            assert!(is_public(ok.parse().unwrap()), "{ok}");
        }
        for bad in [
            "10.77.0.1",
            "192.168.1.1",
            "172.20.0.5",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!is_public(bad.parse().unwrap()), "{bad}");
        }
    }

    #[test]
    fn urls_are_checked_before_connecting() {
        let p = FetchPolicy::default();
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        assert!(check_url(&url("https://cloud.debian.org/x.qcow2"), p).is_ok());
        assert!(check_url(&url("http://10.77.0.1:18080/"), p).is_err());
        assert!(check_url(&url("http://[::1]/"), p).is_err());
        assert!(check_url(&url("file:///etc/shadow"), p).is_err());
        let trusted = FetchPolicy {
            allow_private: true,
            ..p
        };
        assert!(check_url(&url("http://10.77.0.1/"), trusted).is_ok());
        assert!(check_url(&url("file:///etc/shadow"), trusted).is_err());
    }

    #[tokio::test]
    async fn names_resolving_to_private_addresses_are_refused() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join(format!("mst-fetch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // `localhost` resolves to loopback only.
        let err = download(
            "http://localhost:9/x",
            &dir.join("x"),
            FetchPolicy::default(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no public address"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn images_pointing_at_host_files_are_refused() {
        if !which("qemu-img") {
            return eprintln!("qemu-img not installed; skipping");
        }
        let dir = std::env::temp_dir().join(format!("mst-untrusted-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
        let target = p("host-secret");
        std::fs::write(&target, vec![0u8; 1 << 20]).unwrap();
        let mk = |args: Vec<String>| {
            let out = std::process::Command::new("qemu-img")
                .args(&args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        mk(vec![
            "create".into(),
            "-f".into(),
            "qcow2".into(),
            p("plain.qcow2"),
            "1M".into(),
        ]);
        mk(vec![
            "create".into(),
            "-f".into(),
            "qcow2".into(),
            "-F".into(),
            "raw".into(),
            "-b".into(),
            target.clone(),
            p("backed.qcow2"),
        ]);
        mk(vec![
            "create".into(),
            "-f".into(),
            "qcow2".into(),
            "-o".into(),
            format!("data_file={target},data_file_raw=on"),
            p("data.qcow2"),
            "1M".into(),
        ]);
        check_untrusted(&dir.join("plain.qcow2"), "qcow2")
            .await
            .unwrap();
        let e = check_untrusted(&dir.join("backed.qcow2"), "qcow2")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("backing file"), "{e}");
        let e = check_untrusted(&dir.join("data.qcow2"), "qcow2")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("data file"), "{e}");
        // Declared raw, the same bytes are just data.
        check_untrusted(&dir.join("data.qcow2"), "raw")
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
