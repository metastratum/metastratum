//! The control session with the controller: hello, heartbeats, commands.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use mst_libvirt::{Change, Libvirt, Power};
use mst_proto::agent_control_client::AgentControlClient;
use mst_proto::command::Cmd;
use mst_proto::{
    AgentMessage, CommandResult, ConsoleKind, DomainState, Heartbeat, Hello, HostFacts, PowerState,
    agent_message, controller_message,
};
use mst_storage::{PoolSpec, Pools};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};

use crate::enroll::HostIdentity;
use crate::{console, vm};

const HEARTBEAT: Duration = Duration::from_secs(5);
/// Every this many heartbeats carries the full domain list, a safety net for
/// anything the event stream missed.
const FULL_SYNC_EVERY: u32 = 6;

pub struct Agent {
    /// Controller replicas to try, in turn.
    pub controllers: Vec<String>,
    /// The one currently in use.
    pub current: std::sync::atomic::AtomicUsize,
    /// Replaced when the certificate is renewed.
    pub identity: std::sync::RwLock<HostIdentity>,
    /// Where the identity is stored.
    pub state_dir: std::path::PathBuf,
    /// Ends the session, to reconnect (with a renewed identity).
    pub reconnect: std::sync::Arc<tokio::sync::Notify>,
    /// Commands being handled (and not yet answered).
    pub in_flight: std::sync::atomic::AtomicIsize,
    pub facts: HostFacts,
    pub lv: Libvirt,
    pub pools: Arc<Pools>,
    pub files: vm::VmFiles,
    /// Largest image download.
    pub max_image_bytes: u64,
    /// Held while QEMU's TLS files are changed, and while a migration's own
    /// credentials are in place for QEMU to load.
    pub qemu_tls: tokio::sync::Mutex<()>,
    /// Domain definitions for incoming migrations, by VM, made here from the
    /// controller's spec (`PrepareMigration`): what arrives is defined from
    /// these, never from what the source host sends.
    pub incoming: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// The OVN southbound remote the controller manages access to, if any.
    pub ovn_sb_remote: std::sync::Mutex<String>,
    /// Domain lifecycle changes from libvirt, forwarded while a session is up.
    pub changes: tokio::sync::Mutex<mpsc::Receiver<Change>>,
    /// Whether libvirt should listen for TLS connections, as the controller
    /// last asked (see `apply_libvirt_tls`).
    pub libvirt_tls: tokio::sync::watch::Sender<Option<bool>>,
    /// Console sessions, by VM.
    pub consoles: Arc<console::Open>,
}

fn power(p: Power) -> PowerState {
    match p {
        Power::Running => PowerState::Running,
        Power::Stopped => PowerState::Stopped,
        Power::Paused => PowerState::Paused,
        Power::Crashed => PowerState::Crashed,
        Power::ShuttingDown => PowerState::ShuttingDown,
        Power::Unknown => PowerState::Unspecified,
    }
}

impl Agent {
    fn controller(&self) -> String {
        let i = self.current.load(std::sync::atomic::Ordering::Relaxed);
        self.controllers[i % self.controllers.len()].clone()
    }

    /// After a failed session: try the next replica.
    fn next_controller(&self) {
        if self.controllers.len() > 1 {
            self.current
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    async fn domains(&self) -> Result<Vec<DomainState>> {
        Ok(self
            .lv
            .list_managed()
            .await?
            .into_iter()
            .map(|d| DomainState {
                vm_id: d.meta.vm_id,
                generation: d.meta.generation,
                power: power(d.state.power) as i32,
                reason: d.state.reason.unwrap_or_default().to_owned(),
            })
            .collect())
    }

    pub(crate) async fn channel(&self) -> Result<Channel> {
        let tls = self.identity.read().unwrap().tls();
        Endpoint::from_shared(self.controller())?
            .tls_config(tls)?
            .connect_timeout(Duration::from_secs(10))
            .http2_keep_alive_interval(Duration::from_secs(15))
            .keep_alive_while_idle(true)
            .connect()
            .await
            .with_context(|| format!("connecting to {}", self.controller()))
    }

    /// Runs sessions forever, reconnecting with capped exponential backoff.
    pub async fn run(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = tokio::time::Instant::now();
            match self.clone().session().await {
                Ok(()) => tracing::info!("session ended"),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), controller = %self.controller(), "session failed")
                }
            }
            self.next_controller();
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(1);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    async fn session(self: Arc<Self>) -> Result<()> {
        let channel = self.channel().await?;
        let mut client = AgentControlClient::new(channel.clone());
        let (tx, rx) = mpsc::channel::<AgentMessage>(64);
        let mut facts = self.facts.clone();
        // What may have changed on the host since the agent started: the TLS
        // listener, and OVS's identity and bridges (OVS resets its system-id
        // at start on some distributions).
        facts.migration_ready = crate::tls::ready_for_migration();
        facts.ovn_chassis = crate::facts::ovn_chassis().await;
        facts.physical_networks = crate::facts::physical_networks().await;
        let hello = Hello {
            protocol_version: mst_proto::PROTOCOL_VERSION,
            capabilities: mst_proto::CAPABILITIES
                .iter()
                .map(|c| (*c).to_owned())
                .collect(),
            facts: Some(facts),
            domains: self.domains().await?,
        };
        tx.send(AgentMessage {
            msg: Some(agent_message::Msg::Hello(hello)),
        })
        .await?;
        let mut inbound = client.session(ReceiverStream::new(rx)).await?.into_inner();
        let host_id = self.identity.read().unwrap().host_id.clone();
        tracing::info!(host = %host_id, controller = %self.controller(), "session established");

        let heartbeat = {
            let (this, tx) = (self.clone(), tx.clone());
            tokio::spawn(async move {
                for beat in 1u32.. {
                    tokio::time::sleep(HEARTBEAT).await;
                    let complete = beat % FULL_SYNC_EVERY == 0;
                    if complete && this.full_sync(&tx).await.is_err() {
                        break;
                    }
                    if !complete {
                        let hb = Heartbeat {
                            domains: vec![],
                            complete: false,
                        };
                        if tx
                            .send(AgentMessage {
                                msg: Some(agent_message::Msg::Heartbeat(hb)),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            })
        };

        // Push power changes the moment libvirt reports them.
        let forward = {
            let (this, tx) = (self.clone(), tx.clone());
            tokio::spawn(async move {
                let mut changes = this.changes.lock().await;
                while let Some(change) = changes.recv().await {
                    match change {
                        Change::Domain(uuid) => this.report_power(&uuid, &tx).await,
                        Change::Resync => {
                            if this.full_sync(&tx).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            })
        };

        let result = async {
            loop {
                let msg = tokio::select! {
                    m = inbound.message() => m?,
                    () = self.reconnect.notified() => {
                        // Let commands in flight answer on this session first.
                        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
                        while self.in_flight.load(std::sync::atomic::Ordering::Relaxed) > 0
                            && tokio::time::Instant::now() < deadline
                        {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                        tracing::info!("reconnecting with the renewed certificate");
                        return Ok(());
                    }
                };
                let Some(msg) = msg else { break };
                let cmd = match msg.msg {
                    Some(controller_message::Msg::Command(cmd)) => cmd,
                    Some(controller_message::Msg::Welcome(w)) => {
                        tracing::info!(protocol = w.protocol_version, version = %w.controller_version, "controller says hello");
                        *self.ovn_sb_remote.lock().unwrap() = w.ovn_sb_remote.clone();
                        if let Some(inventory) = w.inventory.clone() {
                            let this = self.clone();
                            tokio::spawn(async move { crate::cleanup::reconcile(&this, &inventory).await });
                        }
                        if !w.crl_pem.is_empty()
                            && let Err(e) = {
                                let _held = self.qemu_tls.lock().await;
                                crate::tls::install_crl(&w.crl_pem).await
                            }
                        {
                            tracing::warn!(error = %format!("{e:#}"), "installing the revocation list");
                        }
                        if !w.ovn_sb_remote.is_empty() {
                            let this = self.clone();
                            tokio::spawn(async move { this.keep_ovn_access().await });
                        }
                        continue;
                    }
                    None => continue,
                };
                let (this, tx, client) = (
                    self.clone(),
                    tx.clone(),
                    AgentControlClient::new(channel.clone()),
                );
                this_in_flight(&self.in_flight, 1);
                tokio::spawn(async move {
                    let request_id = cmd.request_id.clone();
                    let (error, payload) = match cmd.cmd {
                        Some(c) => match this.handle(c, &tx, client).await {
                            Ok(payload) => (String::new(), payload),
                            Err(e) => {
                                tracing::warn!(error = %format!("{e:#}"), "command failed");
                                (format!("{e:#}"), String::new())
                            }
                        },
                        None => ("empty command".into(), String::new()),
                    };
                    let msg = AgentMessage {
                        msg: Some(agent_message::Msg::Result(CommandResult {
                            request_id,
                            error,
                            payload,
                        })),
                    };
                    let _ = tx.send(msg).await;
                    this_in_flight(&this.in_flight, -1);
                });
            }
            Ok(())
        }
        .await;
        heartbeat.abort();
        forward.abort();
        result
    }

    /// Sends the full list of managed domains. Errs only if the session is
    /// gone; libvirt errors are logged and retried next time.
    async fn full_sync(&self, tx: &mpsc::Sender<AgentMessage>) -> Result<(), ()> {
        match self.domains().await {
            Ok(domains) => {
                let hb = Heartbeat {
                    domains,
                    complete: true,
                };
                tx.send(AgentMessage {
                    msg: Some(agent_message::Msg::Heartbeat(hb)),
                })
                .await
                .map_err(|_| ())
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "listing domains");
                Ok(())
            }
        }
    }

    /// Prepares the pool a command refers to.
    pub(crate) async fn pool(&self, spec: Option<&mst_proto::Pool>) -> Result<mst_storage::Pool> {
        let spec = pool_spec(spec.context("command does not name a storage pool")?)?;
        let pool = self.pools.get(&spec).await?;
        // QEMU authenticates to Ceph with a libvirt secret; the pool ID
        // doubles as its UUID.
        if let mst_storage::Pool::Rbd(rbd) = &pool {
            self.lv
                .ensure_ceph_secret(&spec.id, &format!("metastratum pool {}", spec.id), &rbd.key)
                .await?;
        }
        Ok(pool)
    }

    async fn report_power(&self, vm_id: &str, tx: &mpsc::Sender<AgentMessage>) {
        if let Ok(st) = self.lv.state(vm_id).await {
            let d = DomainState {
                vm_id: vm_id.to_owned(),
                generation: 0,
                power: power(st.power) as i32,
                reason: st.reason.unwrap_or_default().to_owned(),
            };
            let _ = tx
                .send(AgentMessage {
                    msg: Some(agent_message::Msg::DomainState(d)),
                })
                .await;
        }
    }

    async fn handle(
        self: &Arc<Self>,
        cmd: Cmd,
        tx: &mpsc::Sender<AgentMessage>,
        client: AgentControlClient<Channel>,
    ) -> Result<String> {
        match cmd {
            Cmd::EnsureImage(c) => {
                tracing::info!(image = %c.image_id, url = %c.source_url, "ensuring image");
                let pool = self.pool(c.pool.as_ref()).await?;
                let policy = mst_storage::FetchPolicy {
                    allow_private: c.trusted,
                    max_bytes: self.max_image_bytes,
                    max_virtual_bytes: match c.max_virtual_bytes {
                        0 => mst_storage::FetchPolicy::default().max_virtual_bytes,
                        n => n,
                    },
                };
                pool.ensure_image(&c.image_id, &c.source_url, &c.format, policy)
                    .await?;
            }
            Cmd::EnsureVolume(c) => {
                let image = Some(c.image_id.as_str()).filter(|s| !s.is_empty());
                let pool = self.pool(c.pool.as_ref()).await?;
                pool.ensure_volume(&c.volume_id, c.size_bytes, image)
                    .await?;
            }
            Cmd::DeleteVolume(c) => {
                let pool = self.pool(c.pool.as_ref()).await?;
                pool.delete_volume(&c.volume_id).await?;
            }
            Cmd::SetMemory(c) => {
                tracing::info!(vm = %c.vm_id, memory_mib = c.memory_mib, "setting memory");
                self.lv.set_memory(&c.vm_id, c.memory_mib).await?;
            }
            Cmd::SetVcpus(c) => {
                tracing::info!(vm = %c.vm_id, vcpus = c.vcpus, "setting virtual CPUs");
                self.lv.set_vcpus(&c.vm_id, c.vcpus).await?;
            }
            Cmd::AttachDisk(c) => {
                let v = c.vm.context("no VM to attach the disk to")?;
                let def = self.domain_def(&v).await?;
                if self.lv.power(&v.vm_id).await? != Power::Stopped {
                    let xml = def
                        .disk_xml(&c.target)
                        .with_context(|| format!("the VM has no disk at {}", c.target))?;
                    self.lv.attach_device(&v.vm_id, &xml).await?;
                }
                self.lv.define(def.to_xml()).await?;
                tracing::info!(vm = %v.vm_id, target = %c.target, "disk attached");
            }
            Cmd::DetachDisk(c) => {
                let v = c.vm.context("no VM to detach the disk from")?;
                let def = self.domain_def(&v).await?;
                if self.lv.power(&v.vm_id).await? != Power::Stopped {
                    self.lv
                        .detach_disk(&v.vm_id, &c.target, Duration::from_secs(30))
                        .await?;
                }
                self.lv.define(def.to_xml()).await?;
                tracing::info!(vm = %v.vm_id, target = %c.target, "disk detached");
            }
            Cmd::MemoryStats(c) => {
                let m = self.lv.memory_stats(&c.vm_id).await?;
                return Ok(serde_json::to_string(&serde_json::json!({
                    "actual_mib": m.actual_mib,
                    "driver": m.driver,
                    "available_mib": m.available_mib,
                    "usable_mib": m.usable_mib,
                    "unused_mib": m.unused_mib,
                    "rss_mib": m.rss_mib,
                }))?);
            }
            Cmd::DefineVm(c) => {
                let def = self.domain_def(&c).await?;
                self.lv.define(def.to_xml()).await?;
                tracing::info!(vm = %c.vm_id, name = %c.name, "domain defined");
            }
            Cmd::PrepareMigration(c) => {
                // Everything the domain needs here, except the domain itself:
                // pools mounted, Ceph secrets set, the seed ISO written.
                let c = c.vm.context("no VM to prepare for")?;
                let def = self.domain_def(&c).await?;
                self.incoming
                    .lock()
                    .unwrap()
                    .insert(c.vm_id.clone(), def.to_xml());
                tracing::info!(vm = %c.vm_id, "prepared for incoming migration");
            }
            Cmd::MigrateVm(c) => {
                tracing::info!(vm = %c.vm_id, to = %c.dest_address, disks = ?c.copy_disks, "migrating");
                let to = mst_libvirt::MigrateTo {
                    address: &c.dest_address,
                    copy_disks: &c.copy_disks,
                    max_bandwidth_mib: None,
                };
                self.lv.migrate(&c.vm_id, &to).await?;
                tracing::info!(vm = %c.vm_id, to = %c.dest_address, "migrated");
                self.consoles.close(&c.vm_id).await;
                // The domain is gone from here; say so at once.
                let _ = self.full_sync(tx).await;
            }
            Cmd::MigrationStep(c) => {
                use mst_proto::migration_step::Step;
                let which = Step::try_from(c.step).context("unknown migration step")?;
                let step = match which {
                    Step::Begin => mst_libvirt::MigrationStep::Begin,
                    // Not the XML the source sent (a host taken over could
                    // name other VMs' disks, or host devices, in it): the
                    // one made here from the controller's spec.
                    Step::Prepare => mst_libvirt::MigrationStep::Prepare {
                        xml: self.incoming_def(&c.vm_id)?,
                    },
                    Step::Perform => mst_libvirt::MigrationStep::Perform { uri: c.uri },
                    Step::Finish => mst_libvirt::MigrationStep::Finish {
                        uri: c.uri,
                        name: c.name,
                        cancelled: c.cancelled,
                    },
                    Step::Confirm => mst_libvirt::MigrationStep::Confirm {
                        cancelled: c.cancelled,
                    },
                };
                tracing::info!(vm = %c.vm_id, step = ?which, cancelled = c.cancelled, "migration step");
                // Confirmed, the source's domain goes away: its consoles first.
                if which == Step::Confirm && !c.cancelled {
                    self.consoles.close(&c.vm_id).await;
                }
                let to = mst_libvirt::MigrateTo {
                    address: &c.dest_address,
                    copy_disks: &c.copy_disks,
                    max_bandwidth_mib: None,
                };
                let out = match (&c.tls, which) {
                    (Some(tls), Step::Prepare) => {
                        // The destination's QEMU loads its TLS credentials
                        // while the incoming domain is readied.
                        let _held = self.qemu_tls.lock().await;
                        crate::tls::install_migration_tls(tls, crate::tls::MigrationEnd::Server)?;
                        let r = self.lv.migration_step(&c.vm_id, &to, step, c.cookie).await;
                        self.restore_qemu_identity();
                        r?
                    }
                    (Some(tls), Step::Perform) => {
                        // The source's QEMU loads them as the copy starts:
                        // keep them in place until data flows.
                        let held = self.qemu_tls.lock().await;
                        crate::tls::install_migration_tls(tls, crate::tls::MigrationEnd::Client)?;
                        let perform = self.lv.migration_step(&c.vm_id, &to, step, c.cookie);
                        tokio::pin!(perform);
                        let deadline = tokio::time::Instant::now() + Duration::from_secs(1800);
                        let done = loop {
                            tokio::select! {
                                r = &mut perform => break Some(r),
                                () = tokio::time::sleep(Duration::from_millis(250)) => {
                                    let flowing = self.lv.job_stats(&c.vm_id).await.is_ok_and(|st| {
                                        st.active && (st.data_processed > 0 || st.disk_processed > 0)
                                    });
                                    if flowing || tokio::time::Instant::now() > deadline {
                                        break None;
                                    }
                                }
                            }
                        };
                        self.restore_qemu_identity();
                        drop(held);
                        match done {
                            Some(r) => r?,
                            None => perform.await?,
                        }
                    }
                    _ => {
                        self.lv
                            .migration_step(&c.vm_id, &to, step, c.cookie)
                            .await?
                    }
                };
                if which == Step::Finish && !c.cancelled {
                    // Its persistent definition came from the source too, in
                    // the cookie: replace it with the one made here.
                    let xml = self.incoming_def(&c.vm_id)?;
                    self.lv.define(xml).await?;
                    self.incoming.lock().unwrap().remove(&c.vm_id);
                }
                if matches!(which, Step::Finish | Step::Confirm) {
                    // The domain arrived here, or left: say so at once.
                    let _ = self.full_sync(tx).await;
                }
                use base64::Engine;
                return Ok(serde_json::to_string(&serde_json::json!({
                    "cookie": base64::engine::general_purpose::STANDARD.encode(&out.cookie),
                    "xml": out.xml,
                    "uri": out.uri,
                }))?);
            }
            Cmd::SetDiskLimits(c) => {
                let limits = disk_limits(c.limits.as_ref());
                tracing::info!(vm = %c.vm_id, ?limits, "setting disk limits");
                match self.lv.set_disk_limits(&c.vm_id, &c.targets, limits).await {
                    // Not here (any more): its next definition carries them.
                    Err(mst_libvirt::conn::Error::NoDomain(_)) => {}
                    r => r?,
                }
            }
            Cmd::RemovePool(c) => {
                let spec = pool_spec(c.pool.as_ref().context("no pool named")?)?;
                return crate::cleanup::remove_pool(self, spec, c.remove_data).await;
            }
            Cmd::BackupVm(c) => return crate::backup::backup(self, &c).await,
            Cmd::RestoreVolume(c) => crate::backup::restore(self, &c).await?,
            Cmd::DeleteBackup(c) => crate::backup::delete(self, &c).await?,
            Cmd::SnapshotVm(c) => return crate::snapshot::snapshot(self, &c).await,
            Cmd::RevertSnapshot(c) => crate::snapshot::revert(self, &c).await?,
            Cmd::DeleteSnapshot(c) => crate::snapshot::delete(self, &c).await?,
            Cmd::ResizeDisk(c) => crate::vm::resize_disk(self, &c).await?,
            Cmd::CloneVolume(c) => crate::vm::clone_volume(self, &c).await?,
            Cmd::ExportVolume(c) => return crate::transfer::export(self, &c).await,
            Cmd::EndExport(c) => crate::transfer::end(&c.volume_id),
            Cmd::CopyVolume(c) => crate::transfer::copy(self, &c).await?,
            Cmd::DeleteImage(c) => {
                let shared = c.pools.iter().map(pool_spec).collect::<Result<Vec<_>>>()?;
                crate::cleanup::delete_image(self, &c.image_id, &shared).await?;
            }
            Cmd::Decommission(_) => {
                tracing::warn!("decommissioning this host");
                let report = crate::cleanup::decommission(self).await?;
                crate::cleanup::stop_self();
                return Ok(serde_json::to_string(&report)?);
            }
            Cmd::InstallCrl(c) => {
                let _held = self.qemu_tls.lock().await;
                crate::tls::install_crl(&c.crl_pem).await?;
                tracing::info!("installed a new revocation list");
            }
            Cmd::LibvirtTls(c) => {
                self.libvirt_tls.send_replace(Some(c.listen));
            }
            Cmd::MigrationStats(c) => {
                let st = match self.lv.job_stats(&c.vm_id).await {
                    Ok(st) => st,
                    // Gone: the migration finished (or never started here).
                    Err(mst_libvirt::conn::Error::NoDomain(_)) => mst_libvirt::JobStats::default(),
                    Err(e) => return Err(e.into()),
                };
                return Ok(serde_json::to_string(&serde_json::json!({
                    "active": st.active,
                    "elapsed_ms": st.elapsed_ms,
                    "data_total": st.data_total,
                    "data_processed": st.data_processed,
                    "data_remaining": st.data_remaining,
                    "dirty_pages_per_s": st.dirty_pages_per_s,
                    "downtime_ms": st.downtime_ms,
                }))?);
            }
            Cmd::AbortMigration(c) => {
                tracing::info!(vm = %c.vm_id, "aborting migration");
                self.lv.abort_job(&c.vm_id).await?;
            }
            Cmd::DomainInfo(c) => {
                let info = match self.lv.state(&c.vm_id).await {
                    Ok(st) => {
                        let job = self
                            .lv
                            .job_stats(&c.vm_id)
                            .await
                            .map(|j| j.active)
                            .unwrap_or(false);
                        serde_json::json!({"defined": true, "power": session_power_name(st.power), "job_active": job})
                    }
                    Err(mst_libvirt::conn::Error::NoDomain(_)) => {
                        serde_json::json!({"defined": false, "power": "missing", "job_active": false})
                    }
                    Err(e) => return Err(e.into()),
                };
                return Ok(info.to_string());
            }
            Cmd::RenewCertificate(_) => {
                self.renew().await?;
                self.reconnect_soon();
            }
            Cmd::BaselineCpu(c) => {
                let cpus = c
                    .cpus_json
                    .iter()
                    .map(|j| {
                        serde_json::from_str::<mst_core::vm::CpuSpec>(j).map(|s| vm::cpu_model(&s))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .context("invalid CPU model")?;
                let base = self
                    .lv
                    .baseline_cpu(&cpus)
                    .await?
                    .context("libvirt found no common CPU model")?;
                return Ok(serde_json::to_string(&vm::cpu_spec(base))?);
            }
            Cmd::StartVm(c) => {
                self.lv.start(&c.vm_id).await?;
                self.report_power(&c.vm_id, tx).await;
            }
            Cmd::StopVm(c) => {
                if c.force {
                    self.consoles.close(&c.vm_id).await;
                    self.lv.destroy(&c.vm_id).await?
                } else {
                    self.lv.shutdown(&c.vm_id).await?
                }
                self.report_power(&c.vm_id, tx).await;
            }
            Cmd::RebootVm(c) => self.lv.reboot(&c.vm_id).await?,
            Cmd::UndefineVm(c) => {
                self.consoles.close(&c.vm_id).await;
                self.lv.undefine(&c.vm_id).await?;
                let _ = tokio::fs::remove_file(self.files.seed_path(&c.vm_id)).await;
                tracing::info!(vm = %c.vm_id, "domain removed");
            }
            Cmd::OpenConsole(c) => {
                let kind =
                    ConsoleKind::try_from(c.kind).map_err(|_| anyhow!("unknown console kind"))?;
                let source = console::open(&self.lv, &c.vm_id, kind).await?;
                let (vm_id, session) = (c.vm_id, c.session_id);
                self.consoles.spawn(vm_id.clone(), async move {
                    tracing::info!(vm = %vm_id, ?kind, "console session started");
                    if let Err(e) = console::relay(client, session, source).await {
                        tracing::warn!(vm = %vm_id, error = %format!("{e:#}"), "console session failed");
                    }
                    tracing::info!(vm = %vm_id, "console session ended");
                });
            }
        }
        Ok(String::new())
    }

    /// Renews the host certificate: a new key, signed by the controller
    /// over the current session's identity. The new identity is stored, set
    /// up for libvirt and QEMU, and used from the next session on.
    pub async fn renew(&self) -> Result<()> {
        let current = self.identity.read().unwrap().clone();
        let (key_pem, csr_pem) =
            mst_pki::new_key_and_csr(&current.host_id).context("making a key")?;
        let mut client = AgentControlClient::new(self.channel().await?);
        let cert_pem = client
            .renew(mst_proto::RenewRequest { csr_pem })
            .await
            .context("renewing the certificate")?
            .into_inner()
            .cert_pem;
        let renewed = HostIdentity {
            key_pem,
            cert_pem,
            ..current
        };
        renewed
            .save(&self.state_dir)
            .context("storing the renewed certificate")?;
        {
            let _held = self.qemu_tls.lock().await;
            crate::tls::install(&renewed)?;
        }
        crate::tls::reload_libvirt().await;
        *self.identity.write().unwrap() = renewed;
        tracing::info!("host certificate renewed");
        Ok(())
    }

    /// Puts the host's identity back for QEMU after a migration's own.
    fn restore_qemu_identity(&self) {
        let id = self.identity.read().unwrap().clone();
        if let Err(e) = crate::tls::restore_qemu_identity(&id) {
            tracing::error!(error = %format!("{e:#}"), "restoring QEMU's TLS files");
        }
    }

    /// Opens or closes libvirt's TLS listener as the controller asks. One
    /// change at a time, and only the latest request: those that arrive
    /// while one waits or runs replace each other.
    pub async fn apply_libvirt_tls(self: Arc<Self>) {
        let mut wanted = self.libvirt_tls.subscribe();
        while wanted.changed().await.is_ok() {
            // Changing it restarts libvirt, which would cut off a migration
            // (going either way): wait for those to end.
            for _ in 0..720 {
                if !self.migrations_running().await {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            let Some(listen) = *wanted.borrow_and_update() else {
                continue;
            };
            if let Err(e) = crate::tls::set_listening(listen).await {
                tracing::warn!(error = %format!("{e:#}"), "changing libvirt's TLS listener");
            }
        }
    }

    /// Whether a domain here has a job going (a migration in or out).
    async fn migrations_running(&self) -> bool {
        let Ok(domains) = self.lv.list_managed().await else {
            return true;
        };
        for d in domains {
            if self
                .lv
                .job_stats(&d.meta.vm_id)
                .await
                .is_ok_and(|st| st.active)
            {
                return true;
            }
        }
        false
    }

    /// Sets up ovn-controller's access to the southbound database, when the
    /// controller manages it.
    async fn keep_ovn_access(&self) {
        let remote = self.ovn_sb_remote.lock().unwrap().clone();
        if remote.is_empty() {
            return;
        }
        if let Err(e) = crate::ovn::ensure(self, &remote).await {
            tracing::warn!(error = %format!("{e:#}"), "setting up OVN southbound access; will retry");
        }
    }

    /// Renews the certificate once two thirds of its lifetime have passed
    /// (and the OVN one alike).
    pub async fn keep_certificate_fresh(self: std::sync::Arc<Self>) {
        loop {
            self.keep_ovn_access().await;
            let due = {
                let id = self.identity.read().unwrap();
                mst_pki::pem_to_der(&id.cert_pem)
                    .and_then(|der| mst_pki::validity_of(&der))
                    .map(|(from, until)| {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        until - now < (until - from) / 3
                    })
                    .unwrap_or(false)
            };
            if due {
                match self.renew().await {
                    Ok(()) => self.reconnect_soon(),
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "renewing the host certificate; will retry")
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }

    /// Ends the current session shortly (letting answers in flight go out),
    /// so the next one uses the current identity.
    pub fn reconnect_soon(&self) {
        let reconnect = self.reconnect.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            reconnect.notify_one();
        });
    }

    /// The domain for a VM, with what it needs on this host set up.
    /// The definition made for an incoming VM by `PrepareMigration`.
    fn incoming_def(&self, vm_id: &str) -> anyhow::Result<String> {
        self.incoming
            .lock()
            .unwrap()
            .get(vm_id)
            .cloned()
            .context("not prepared for this VM (the agent restarted?); migrate it again")
    }

    async fn domain_def(&self, c: &mst_proto::DefineVm) -> Result<mst_libvirt::DomainDef> {
        let spec: mst_core::vm::VmSpec =
            serde_json::from_str(&c.spec_json).context("invalid VM spec")?;
        let mut pools = std::collections::HashMap::new();
        for p in &c.pools {
            pools.insert(p.id.clone(), self.pool(Some(p)).await?);
        }
        vm::domain(
            &pools,
            &self.files,
            c,
            &spec,
            disk_limits(c.disk_limits.as_ref()),
        )
        .await
    }
}

fn session_power_name(p: Power) -> &'static str {
    match p {
        Power::Running => "running",
        Power::Stopped => "stopped",
        Power::Paused => "paused",
        Power::Crashed => "crashed",
        Power::ShuttingDown => "shutting_down",
        Power::Unknown => "unknown",
    }
}

fn this_in_flight(n: &std::sync::atomic::AtomicIsize, d: isize) {
    n.fetch_add(d, std::sync::atomic::Ordering::Relaxed);
}

/// Disk limits as libvirt takes them (0 in the protocol: none).
fn disk_limits(l: Option<&mst_proto::DiskLimits>) -> mst_libvirt::DiskLimits {
    let some = |v: u64| (v > 0).then_some(v);
    l.map(|l| mst_libvirt::DiskLimits {
        iops: some(l.iops),
        bytes_per_sec: some(l.bytes_per_sec),
    })
    .unwrap_or_default()
}

/// A pool as the controller describes it, for the storage layer.
pub(crate) fn pool_spec(p: &mst_proto::Pool) -> Result<PoolSpec> {
    let config = if p.config_json.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&p.config_json).context("invalid pool config")?
    };
    Ok(PoolSpec {
        id: p.id.clone(),
        kind: p.kind.clone(),
        config,
    })
}
