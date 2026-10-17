//! `mst-controller doctor`: looks at a running installation against the
//! deployment checklist in SECURITY.md, and says what to fix.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use crate::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug)]
pub struct Finding {
    pub level: Level,
    pub what: String,
    /// How to fix it.
    pub fix: Option<String>,
}

#[derive(Default)]
pub struct Report {
    pub sections: Vec<(&'static str, Vec<Finding>)>,
}

impl Report {
    fn section(&mut self, name: &'static str) -> &mut Vec<Finding> {
        self.sections.push((name, Vec::new()));
        &mut self.sections.last_mut().expect("just pushed").1
    }

    pub fn worst(&self) -> Level {
        self.sections
            .iter()
            .flat_map(|(_, f)| f.iter().map(|f| f.level))
            .max()
            .unwrap_or(Level::Ok)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let (mut warns, mut fails) = (0, 0);
        for (name, findings) in &self.sections {
            let _ = writeln!(out, "{name}");
            for f in findings {
                let tag = match f.level {
                    Level::Ok => "[ ok ]",
                    Level::Warn => {
                        warns += 1;
                        "[warn]"
                    }
                    Level::Fail => {
                        fails += 1;
                        "[FAIL]"
                    }
                };
                let _ = writeln!(out, "  {tag} {}", f.what);
                if let Some(fix) = &f.fix {
                    for line in fix.lines() {
                        let _ = writeln!(out, "         {line}");
                    }
                }
            }
            out.push('\n');
        }
        let _ = writeln!(
            out,
            "Not checked here: that guests and tenants cannot reach the management network,\n\
             migration ports (49152-49215) open between hosts only, multi-factor\n\
             authentication at identity providers, the master key kept out of the\n\
             database's backups, and `cargo audit` / `npm audit` with every update."
        );
        let _ = writeln!(
            out,
            "\n{}",
            match (fails, warns) {
                (0, 0) => "Everything checked is in order.".to_owned(),
                (0, w) => format!("{w} warning{}.", if w == 1 { "" } else { "s" }),
                (f, w) => format!(
                    "{f} failure{}, {w} warning{}.",
                    if f == 1 { "" } else { "s" },
                    if w == 1 { "" } else { "s" }
                ),
            }
        );
        out
    }
}

fn ok(what: impl Into<String>) -> Finding {
    Finding {
        level: Level::Ok,
        what: what.into(),
        fix: None,
    }
}

fn warn(what: impl Into<String>, fix: impl Into<String>) -> Finding {
    Finding {
        level: Level::Warn,
        what: what.into(),
        fix: Some(fix.into()),
    }
}

fn fail(what: impl Into<String>, fix: impl Into<String>) -> Finding {
    Finding {
        level: Level::Fail,
        what: what.into(),
        fix: Some(fix.into()),
    }
}

pub async fn run(
    cfg: &Config,
    config_path: &Path,
    db: &mst_db::sqlx::PgPool,
) -> anyhow::Result<Report> {
    let mut r = Report::default();

    let s = r.section("Files");
    s.push(private_file("the configuration", config_path));
    s.push(private_file("the master key", &cfg.master_key_file));

    let s = r.section("API");
    s.push(match address_kind(cfg.api.listen.ip()) {
        Kind::Loopback | Kind::Private => ok(format!("listens on {}", cfg.api.listen)),
        Kind::Unspecified => warn(
            format!(
                "listens for plain HTTP on every address ({})",
                cfg.api.listen
            ),
            "set api.listen to a loopback or private address, behind the HTTPS proxy",
        ),
        Kind::Public => fail(
            format!(
                "listens for plain HTTP on a public address ({})",
                cfg.api.listen
            ),
            "set api.listen to a loopback or private address, behind the HTTPS proxy",
        ),
    });
    s.push(if cfg.api.trusted_proxies.is_empty() {
        warn(
            "no trusted proxy: session cookies are never marked Secure, and rate limits see the proxy's address",
            "put an HTTPS proxy in front and list it in api.trusted_proxies",
        )
    } else {
        ok(format!("{} trusted proxy entr{}", cfg.api.trusted_proxies.len(), if cfg.api.trusted_proxies.len() == 1 { "y" } else { "ies" }))
    });
    let replicas = mst_db::replicas::live(db).await?.len();
    s.push(match &cfg.api.redis_url {
        None if replicas > 1 => warn(
            format!("{replicas} controllers limit requests each on their own"),
            "set api.redis_url so they share the rate limits",
        ),
        None => ok("rate limits kept in memory (one controller)"),
        Some(url) if url.starts_with("redis://") && !is_local_url(url) => warn(
            "Redis is reached without TLS",
            "use a rediss:// URL in api.redis_url",
        ),
        Some(_) => ok("rate limits shared through Redis"),
    });
    if let Some(m) = cfg.metrics_listen {
        s.push(match address_kind(m.ip()) {
            Kind::Loopback | Kind::Private => ok(format!("metrics on {m}")),
            _ => warn(
                format!("metrics on {m}, which may be reachable from outside"),
                "set metrics_listen to a private address: metrics name hosts and count every tenant's resources",
            ),
        });
    }

    let s = r.section("Database");
    s.push(database_tls(&cfg.database_url));

    let s = r.section("OVN");
    let nb_ca = crate::ca::load_or_create_ovn_nb(db).await?;
    let sb_ca = crate::ca::load_or_create_ovn(db).await?;
    for remote in &cfg.ovn.northbound {
        s.push(if remote.starts_with("ssl:") {
            server_certificate("northbound", remote, nb_ca.cert_pem()).await
        } else {
            fail(
                format!("northbound {remote} is not over TLS: whoever reaches it can rewrite every tenant's network"),
                "install a bundle from `mst-controller ovn-certificate` on the OVN nodes and use ssl: servers in ovn.northbound",
            )
        });
    }
    if cfg.ovn.southbound.is_empty() {
        s.push(warn(
            "no southbound servers set: hosts keep the remote they were set up with, usually without TLS or RBAC",
            "set ovn.southbound to the OVN nodes' ssl: servers",
        ));
    }
    for remote in &cfg.ovn.southbound {
        s.push(server_certificate("southbound", remote, sb_ca.cert_pem()).await);
    }
    s.extend(plain_listeners(cfg).await);

    let s = r.section("Hosts and clusters");
    let hosts: Vec<_> = mst_db::hosts::list(db)
        .await?
        .into_iter()
        .filter(|h| h.state != "decommissioned")
        .collect();
    if hosts.is_empty() {
        s.push(ok("no hosts yet"));
    }
    let current = mst_proto::PROTOCOL_VERSION as i32;
    for h in &hosts {
        if h.connected_to.is_none() {
            let since = h
                .last_seen_at
                .map_or("never connected".to_owned(), |t| format!("last seen {t}"));
            s.push(warn(
                format!("{} has no agent session ({since})", h.name),
                "check the host and its mst-agent service",
            ));
        } else if h.agent_protocol < current {
            s.push(warn(
                format!(
                    "{}'s agent speaks protocol {} (this controller: {current})",
                    h.name, h.agent_protocol
                ),
                "upgrade the agent",
            ));
        }
    }
    if !hosts.is_empty()
        && hosts
            .iter()
            .all(|h| h.connected_to.is_some() && h.agent_protocol >= current)
    {
        s.push(ok(format!(
            "{} host{} connected with current agents",
            hosts.len(),
            if hosts.len() == 1 { "" } else { "s" }
        )));
    }
    for c in mst_db::clusters::list(db).await? {
        let members: Vec<_> = hosts
            .iter()
            .filter(|h| h.cluster_id == Some(c.id))
            .collect();
        if c.ha_enabled {
            let unfenced: Vec<_> = members
                .iter()
                .filter(|h| h.fence_kind.is_none())
                .map(|h| h.name.as_str())
                .collect();
            s.push(if unfenced.is_empty() {
                ok(format!("cluster {}: high availability, every host can be fenced", c.name))
            } else {
                warn(
                    format!("cluster {} has high availability, but {} cannot be fenced: their VMs are not restarted elsewhere if they fail", c.name, unfenced.join(", ")),
                    "set up fencing for them (Infrastructure → Hosts → Manage)",
                )
            });
        }
        if !members.is_empty()
            && c.vm_net_mbps.is_none()
            && c.vm_disk_iops.is_none()
            && c.vm_disk_mbps.is_none()
        {
            s.push(warn(
                format!("cluster {} has no per-VM limits: one VM can take a host's whole network or disk bandwidth", c.name),
                "set limits on the cluster (Infrastructure → Clusters)",
            ));
        }
    }

    let s = r.section("API tokens");
    let forever = mst_db::rbac::tokens_without_expiry(db).await?;
    s.push(if forever > 0 {
        warn(
            format!(
                "{forever} API token{} never expire{}",
                if forever == 1 { "" } else { "s" },
                if forever == 1 { "s" } else { "" }
            ),
            "their owners can replace them with expiring ones on their account page (their name, top right)",
        )
    } else {
        ok("every API token expires")
    });

    Ok(r)
}

/// Whether a file holding secrets is readable by its owner alone.
fn private_file(what: &str, path: &Path) -> Finding {
    if path.starts_with("/run/credentials") {
        return ok(format!("{what} is passed in as a systemd credential"));
    }
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Err(e) => fail(
            format!("{what} ({}) cannot be read: {e}", path.display()),
            "check the path in the configuration",
        ),
        Ok(m) => {
            let mode = m.permissions().mode() & 0o777;
            let fix = format!("chmod 600 {}", path.display());
            if mode & 0o007 != 0 {
                fail(
                    format!(
                        "{what} ({}) is readable by everyone (mode {mode:o})",
                        path.display()
                    ),
                    fix,
                )
            } else if mode & 0o070 != 0 {
                warn(
                    format!(
                        "{what} ({}) is readable by its group (mode {mode:o})",
                        path.display()
                    ),
                    fix,
                )
            } else {
                ok(format!("{what} is readable by its owner only"))
            }
        }
    }
}

enum Kind {
    Loopback,
    Private,
    Unspecified,
    Public,
}

fn address_kind(ip: IpAddr) -> Kind {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    if ip.is_loopback() {
        Kind::Loopback
    } else if ip.is_unspecified() {
        Kind::Unspecified
    } else {
        match ip {
            IpAddr::V4(v4) if v4.is_private() || v4.is_link_local() => Kind::Private,
            IpAddr::V6(v6) if v6.is_unique_local() || v6.is_unicast_link_local() => Kind::Private,
            _ => Kind::Public,
        }
    }
}

fn is_local_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|h| h == "localhost" || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()))
}

fn database_tls(url: &str) -> Finding {
    let Ok(u) = reqwest::Url::parse(url) else {
        return warn("the database URL cannot be read here", "check database_url");
    };
    let host = u.host_str().unwrap_or("");
    if host.is_empty() || host.starts_with('/') || is_local_url(url) {
        return ok("on this machine");
    }
    match u
        .query_pairs()
        .find(|(k, _)| k == "sslmode")
        .map(|(_, v)| v.into_owned())
    {
        Some(m) if m == "verify-full" => {
            ok(format!("{host}, over TLS with its certificate checked"))
        }
        other => warn(
            format!(
                "{host} is reached with sslmode={}: its certificate is not checked",
                other.as_deref().unwrap_or("prefer")
            ),
            "add ?sslmode=verify-full to database_url",
        ),
    }
}

/// What an `ssl:` OVN server presents, against the CA its clients trust.
async fn server_certificate(database: &str, remote: &str, ca_pem: &str) -> Finding {
    const WARN_BEFORE: i64 = 30 * 86400;
    let renew = "issue a new bundle with `mst-controller ovn-certificate` and run its setup.sh on the OVN node";
    let presented = match mst_ovsdb::tls::presented(remote, ca_pem.as_bytes()).await {
        Ok(p) => p,
        Err(e) => {
            return fail(
                format!("{database} {remote} cannot be reached: {e}"),
                "check the OVN node and ovn.* in the configuration",
            );
        }
    };
    let Ok(not_after) = mst_pki::not_after(&presented.certificate) else {
        return fail(
            format!("{database} {remote} presents an unreadable certificate"),
            renew,
        );
    };
    let left = not_after - chrono::Utc::now().timestamp();
    let expires = chrono::DateTime::from_timestamp(not_after, 0)
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    if left <= 0 {
        fail(
            format!("{database} {remote}: its certificate expired on {expires}"),
            renew,
        )
    } else if let Err(e) = presented.trusted {
        fail(
            format!("{database} {remote}: its certificate is not from metastratum's CA ({e})"),
            renew,
        )
    } else if left < WARN_BEFORE {
        warn(
            format!("{database} {remote}: its certificate expires on {expires}"),
            renew,
        )
    } else {
        ok(format!(
            "{database} {remote} over TLS, certificate valid until {expires}"
        ))
    }
}

/// Plain OVSDB listeners on the OVN nodes: OVN's usual ports, and those of
/// any `tcp:` server configured. Whoever reaches one needs no certificate.
async fn plain_listeners(cfg: &Config) -> Vec<Finding> {
    let servers: Vec<&String> = cfg
        .ovn
        .northbound
        .iter()
        .chain(&cfg.ovn.southbound)
        .collect();
    let mut probes = BTreeSet::new();
    for s in &servers {
        let Some((scheme, hostport)) = s.split_once(':') else {
            continue;
        };
        let Some((host, port)) = hostport.rsplit_once(':') else {
            continue;
        };
        if scheme == "unix" {
            continue;
        }
        for p in ["6641", "6642", port] {
            probes.insert((host.to_owned(), p.to_owned()));
        }
    }
    let mut out = Vec::new();
    for (host, port) in probes {
        let remote = format!("tcp:{host}:{port}");
        let open = tokio::time::timeout(Duration::from_secs(5), async {
            let c = mst_ovsdb::Client::connect(&remote, None).await.ok()?;
            c.call("list_dbs", serde_json::json!([])).await.ok()
        })
        .await
        .ok()
        .flatten();
        if let Some(dbs) = open {
            let configured_plain = servers.iter().any(|s| **s == remote);
            if !configured_plain {
                out.push(fail(
                    format!("{remote} serves {dbs} without TLS"),
                    "remove the plain listener on the OVN node (ovn-nbctl / ovn-sbctl list Connection)",
                ));
            }
        }
    }
    if out.is_empty() && !servers.is_empty() {
        out.push(ok("no plain listeners besides those configured"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_classified() {
        let kind = |s: &str| match address_kind(s.parse().unwrap()) {
            Kind::Loopback => "loopback",
            Kind::Private => "private",
            Kind::Unspecified => "unspecified",
            Kind::Public => "public",
        };
        assert_eq!(kind("127.0.0.1"), "loopback");
        assert_eq!(kind("::1"), "loopback");
        assert_eq!(kind("10.0.0.21"), "private");
        assert_eq!(kind("fd00::21"), "private");
        assert_eq!(kind("0.0.0.0"), "unspecified");
        assert_eq!(kind("203.0.113.7"), "public");
    }

    #[test]
    fn database_tls_is_judged_by_its_url() {
        let level = |u: &str| database_tls(u).level;
        assert_eq!(level("postgres://m:m@localhost/m"), Level::Ok);
        assert_eq!(level("postgres://m:m@127.0.0.1:5432/m"), Level::Ok);
        assert_eq!(level("postgres://m:m@db.example.net/m"), Level::Warn);
        assert_eq!(
            level("postgres://m:m@db.example.net/m?sslmode=require"),
            Level::Warn
        );
        assert_eq!(
            level("postgres://m:m@db.example.net/m?sslmode=verify-full"),
            Level::Ok
        );
    }

    #[test]
    fn secret_files_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mst-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("key");
        std::fs::write(&f, "x").unwrap();
        let at = |mode| {
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
            private_file("the key", &f).level
        };
        assert_eq!(at(0o600), Level::Ok);
        assert_eq!(at(0o640), Level::Warn);
        assert_eq!(at(0o644), Level::Fail);
        assert_eq!(
            private_file("x", Path::new("/run/credentials/mst/master.key")).level,
            Level::Ok
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_counts_and_ranks() {
        let mut r = Report::default();
        r.section("A").push(ok("fine"));
        assert_eq!(r.worst(), Level::Ok);
        assert!(r.render().contains("Everything checked is in order."));
        r.section("B").push(warn("hmm", "do this"));
        r.section("C").push(fail("bad", "do that"));
        assert_eq!(r.worst(), Level::Fail);
        let text = r.render();
        assert!(
            text.contains("[FAIL] bad") && text.contains("         do that"),
            "{text}"
        );
        assert!(text.contains("1 failure, 1 warning."), "{text}");
    }
}
