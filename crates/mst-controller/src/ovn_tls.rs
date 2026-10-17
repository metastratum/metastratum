//! TLS for OVN, from the controller's own CAs:
//!
//! - the northbound database trusts the OVN northbound CA, from which only
//!   controllers get certificates (it has no access control of its own);
//! - the southbound database trusts the OVN CA, from which hosts get
//!   certificates named after their chassis, for OVN's RBAC.
//!
//! Each replica issues itself a northbound client certificate and renews it.
//! The OVN servers' own certificates come from `ovn-certificate`, as a bundle
//! to install on each OVN central node; the controller watches what the
//! servers present and warns before it expires.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use mst_pki::Ca;

/// How long a replica's northbound client certificate lasts.
const CLIENT_LIFETIME: Duration = Duration::from_secs(30 * 86400);
/// When it is renewed: with a third of its lifetime left.
const CLIENT_RENEW_EVERY: Duration = Duration::from_secs(20 * 86400);
/// How often the servers' certificates are looked at.
const WATCH_EVERY: Duration = Duration::from_secs(3600);
/// Warn this long before a server's certificate expires.
const WARN_BEFORE: Duration = Duration::from_secs(30 * 86400);

/// TLS for connecting to the northbound database: a fresh client certificate
/// from the northbound CA, trusting servers with certificates from it.
pub fn nb_client(nb_ca: &Ca) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let (cert, key) = nb_ca.issue_client(crate::ca::SERVER_NAME, CLIENT_LIFETIME)?;
    Ok(mst_ovsdb::tls::client_config(
        nb_ca.cert_pem().as_bytes(),
        cert.as_bytes(),
        key.as_bytes(),
    )?)
}

/// Renews this replica's northbound client certificate, for new connections.
pub async fn keep_nb_client_fresh(nb: Arc<mst_ovsdb::Northbound>, nb_ca: Arc<Ca>) {
    loop {
        tokio::time::sleep(CLIENT_RENEW_EVERY).await;
        match nb_client(&nb_ca) {
            Ok(tls) => {
                nb.set_tls(tls);
                tracing::info!("renewed the OVN northbound client certificate");
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "renewing the OVN northbound client certificate")
            }
        }
    }
}

/// Looks at the certificates the `ssl:` northbound and southbound servers
/// present, hourly: exports their expiry as a metric, and warns when one
/// expires within a month, has expired, or is not from the CA its clients
/// trust.
pub async fn watch_servers(nb: Vec<String>, nb_ca_pem: String, sb: Vec<String>, sb_ca_pem: String) {
    let remotes: Vec<(&str, &str, &str)> = nb
        .iter()
        .map(|r| ("northbound", r.as_str(), nb_ca_pem.as_str()))
        .chain(
            sb.iter()
                .map(|r| ("southbound", r.as_str(), sb_ca_pem.as_str())),
        )
        .filter(|(_, r, _)| r.starts_with("ssl:"))
        .collect();
    if remotes.is_empty() {
        return;
    }
    loop {
        for (database, remote, ca_pem) in &remotes {
            check_server(database, remote, ca_pem).await;
        }
        tokio::time::sleep(WATCH_EVERY).await;
    }
}

async fn check_server(database: &str, remote: &str, ca_pem: &str) {
    let presented = match mst_ovsdb::tls::presented(remote, ca_pem.as_bytes()).await {
        Ok(p) => p,
        // Unreachable servers are reported by whatever uses them.
        Err(e) => {
            return tracing::debug!(database, remote, error = %e, "OVN server certificate not seen");
        }
    };
    let Ok(not_after) = mst_pki::not_after(&presented.certificate) else {
        return tracing::warn!(database, remote, "OVN server certificate unreadable");
    };
    metrics::gauge!(
        "mst_ovn_certificate_expiry_timestamp_seconds",
        "database" => database.to_owned(),
        "remote" => remote.to_owned(),
    )
    .set(not_after as f64);
    let left = not_after - chrono::Utc::now().timestamp();
    let expires = chrono::DateTime::from_timestamp(not_after, 0).unwrap_or_default();
    let renew = "issue a new one with `mst-controller ovn-certificate` and run its setup.sh on the OVN node";
    if left <= 0 {
        tracing::error!(database, remote, %expires, "the OVN server's certificate has expired; {renew}");
    } else if left < WARN_BEFORE.as_secs() as i64 {
        tracing::warn!(database, remote, %expires, days = left / 86400, "the OVN server's certificate expires soon; {renew}");
    } else if let Err(e) = presented.trusted {
        tracing::warn!(database, remote, error = %e, "the OVN server's certificate is not from metastratum's CA, so connections are refused; {renew}");
    }
}

/// What `ovn-certificate` writes for an OVN central node.
pub struct Bundle<'a> {
    /// The node's name and the addresses it is reached at (certificates'
    /// subject names; the first is also its common name).
    pub names: &'a [String],
    pub lifetime: Duration,
    pub nb_port: u16,
    pub sb_port: u16,
}

impl Bundle<'_> {
    /// Writes the node's certificates, the CAs its clients' certificates come
    /// from, and `setup.sh`, which installs them.
    pub fn write(&self, nb_ca: &Ca, sb_ca: &Ca, dir: &Path) -> anyhow::Result<()> {
        let (nb_cert, nb_key) = nb_ca.issue_peer(self.names, self.lifetime)?;
        let (sb_cert, sb_key) = sb_ca.issue_peer(self.names, self.lifetime)?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let write = |file: &str, data: &str, mode: u32| -> anyhow::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let path = dir.join(file);
            // Replace rather than rewrite: an existing file keeps its mode.
            let _ = std::fs::remove_file(&path);
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&path)
                .and_then(|mut f| f.write_all(data.as_bytes()))
                .with_context(|| format!("writing {}", path.display()))
        };
        write("nb-ca.pem", nb_ca.cert_pem(), 0o644)?;
        write("nb.pem", &nb_cert, 0o644)?;
        write("nb-key.pem", &nb_key, 0o600)?;
        write("sb-ca.pem", sb_ca.cert_pem(), 0o644)?;
        write("sb.pem", &sb_cert, 0o644)?;
        write("sb-key.pem", &sb_key, 0o600)?;
        write("setup.sh", &self.setup_script(), 0o755)?;
        Ok(())
    }

    fn setup_script(&self) -> String {
        let (nb, sb) = (self.nb_port, self.sb_port);
        format!(
            r#"#!/bin/sh
# Switches on TLS for this OVN central node's databases, with certificates
# from metastratum (mst-controller ovn-certificate). Run it as root on the
# node, from anywhere. Run it again with a new bundle to renew.
#
#   northbound: pssl:{nb}, for the controllers only (nb-ca.pem)
#   southbound: pssl:{sb}, for hosts' ovn-controller, each limited by OVN's
#               RBAC to its own chassis (sb-ca.pem)
#
# A plain listener on either port is replaced. Other plain listeners stay;
# remove them once nothing uses them (ovn-nbctl / ovn-sbctl list Connection).
# The keys are readable by root only: if ovsdb-server runs as another user,
# give them to it.
set -eu
cd "$(dirname "$0")"
dir=/etc/ovn/metastratum
install -d -m 755 "$dir"
install -m 644 nb-ca.pem nb.pem sb-ca.pem sb.pem "$dir/"
install -m 600 nb-key.pem sb-key.pem "$dir/"
ovn-nbctl set-ssl "$dir/nb-key.pem" "$dir/nb.pem" "$dir/nb-ca.pem"
ovn-sbctl set-ssl "$dir/sb-key.pem" "$dir/sb.pem" "$dir/sb-ca.pem"

# listen <nbctl|sbctl> <global table> <port> [column=value...]
listen() {{
  ctl=ovn-$1 global=$2 port=$3
  shift 3
  for c in $($ctl --bare --columns=_uuid find Connection target="\"ptcp:$port\""); do
    $ctl remove "$global" . connections "$c"
  done
  if [ -z "$($ctl --bare --columns=_uuid find Connection target="\"pssl:$port\"")" ]; then
    $ctl -- --id=@c create Connection target="\"pssl:$port\"" "$@" \
      -- add "$global" . connections @c >/dev/null
  fi
}}
listen nbctl NB_Global {nb}
listen sbctl SB_Global {sb} role=ovn-controller
echo "northbound on pssl:{nb}, southbound on pssl:{sb}, with certificates in $dir"
"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_has_certificates_from_each_ca_and_a_script() {
        let nb_ca = Ca::generate("nb", Duration::from_secs(86400)).unwrap();
        let sb_ca = Ca::generate("sb", Duration::from_secs(86400)).unwrap();
        let dir = std::env::temp_dir().join(format!("mst-ovn-bundle-{}", std::process::id()));
        let names = ["ovn1".to_owned(), "10.0.0.31".to_owned()];
        let bundle = Bundle {
            names: &names,
            lifetime: Duration::from_secs(3600),
            nb_port: 6641,
            sb_port: 6642,
        };
        bundle.write(&nb_ca, &sb_ca, &dir).unwrap();
        bundle.write(&nb_ca, &sb_ca, &dir).unwrap(); // again, to renew
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
        let issuer = |f: &str| {
            let der = mst_pki::pem_to_der(&read(f)).unwrap();
            let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
            cert.issuer().to_string()
        };
        assert!(issuer("nb.pem").contains("nb"), "{}", issuer("nb.pem"));
        assert!(issuer("sb.pem").contains("sb"), "{}", issuer("sb.pem"));
        assert_eq!(read("nb-ca.pem"), nb_ca.cert_pem());
        let mode = |f: &str| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(dir.join(f)).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode("nb-key.pem"), 0o600);
        assert_eq!(mode("setup.sh"), 0o755);
        let script = read("setup.sh");
        assert!(
            script.contains("listen sbctl SB_Global 6642 role=ovn-controller"),
            "{script}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
