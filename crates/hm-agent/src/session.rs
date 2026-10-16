//! The control session with the controller: hello, heartbeats, commands.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use hm_libvirt::{Change, Libvirt, Power};
use hm_proto::agent_control_client::AgentControlClient;
use hm_proto::command::Cmd;
use hm_proto::{
    AgentMessage, CommandResult, ConsoleKind, DomainState, Heartbeat, Hello, HostFacts, PowerState,
    agent_message, controller_message,
};
use hm_storage::{PoolSpec, Pools};
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
    pub controller: String,
    pub identity: HostIdentity,
    pub facts: HostFacts,
    pub lv: Libvirt,
    pub pools: Arc<Pools>,
    pub files: vm::VmFiles,
    /// Domain lifecycle changes from libvirt, forwarded while a session is up.
    pub changes: tokio::sync::Mutex<mpsc::Receiver<Change>>,
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

    async fn channel(&self) -> Result<Channel> {
        Endpoint::from_shared(self.controller.clone())?
            .tls_config(self.identity.tls())?
            .connect_timeout(Duration::from_secs(10))
            .http2_keep_alive_interval(Duration::from_secs(15))
            .keep_alive_while_idle(true)
            .connect()
            .await
            .with_context(|| format!("connecting to {}", self.controller))
    }

    /// Runs sessions forever, reconnecting with capped exponential backoff.
    pub async fn run(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = tokio::time::Instant::now();
            match self.clone().session().await {
                Ok(()) => tracing::info!("session ended"),
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "session failed"),
            }
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
        let hello = Hello {
            facts: Some(self.facts.clone()),
            domains: self.domains().await?,
        };
        tx.send(AgentMessage {
            msg: Some(agent_message::Msg::Hello(hello)),
        })
        .await?;
        let mut inbound = client.session(ReceiverStream::new(rx)).await?.into_inner();
        tracing::info!(host = %self.identity.host_id, controller = %self.controller, "session established");

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
            while let Some(msg) = inbound.message().await? {
                let Some(controller_message::Msg::Command(cmd)) = msg.msg else {
                    continue;
                };
                let (this, tx, client) = (
                    self.clone(),
                    tx.clone(),
                    AgentControlClient::new(channel.clone()),
                );
                tokio::spawn(async move {
                    let request_id = cmd.request_id.clone();
                    let error = match cmd.cmd {
                        Some(c) => match this.handle(c, &tx, client).await {
                            Ok(()) => String::new(),
                            Err(e) => {
                                tracing::warn!(error = %format!("{e:#}"), "command failed");
                                format!("{e:#}")
                            }
                        },
                        None => "empty command".into(),
                    };
                    let msg = AgentMessage {
                        msg: Some(agent_message::Msg::Result(CommandResult {
                            request_id,
                            error,
                        })),
                    };
                    let _ = tx.send(msg).await;
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
    async fn pool(&self, spec: Option<&hm_proto::Pool>) -> Result<hm_storage::Pool> {
        let spec = spec.context("command does not name a storage pool")?;
        let config = if spec.config_json.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&spec.config_json).context("invalid pool config")?
        };
        let spec = PoolSpec {
            id: spec.id.clone(),
            kind: spec.kind.clone(),
            config,
        };
        Ok(self.pools.get(&spec).await?)
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
        &self,
        cmd: Cmd,
        tx: &mpsc::Sender<AgentMessage>,
        client: AgentControlClient<Channel>,
    ) -> Result<()> {
        match cmd {
            Cmd::EnsureImage(c) => {
                tracing::info!(image = %c.image_id, url = %c.source_url, "ensuring image");
                let pool = self.pool(c.pool.as_ref()).await?;
                pool.files()
                    .ensure_image(&c.image_id, &c.source_url, &c.format)
                    .await?;
            }
            Cmd::EnsureVolume(c) => {
                let image = Some(c.image_id.as_str()).filter(|s| !s.is_empty());
                let pool = self.pool(c.pool.as_ref()).await?;
                pool.files()
                    .ensure_volume(&c.volume_id, c.size_bytes, image)
                    .await?;
            }
            Cmd::DeleteVolume(c) => {
                let pool = self.pool(c.pool.as_ref()).await?;
                pool.files().delete_volume(&c.volume_id).await?;
            }
            Cmd::DefineVm(c) => {
                let spec: hm_core::vm::VmSpec =
                    serde_json::from_str(&c.spec_json).context("invalid VM spec")?;
                let mut pools = std::collections::HashMap::new();
                for p in &c.pools {
                    pools.insert(p.id.clone(), self.pool(Some(p)).await?);
                }
                let def = vm::domain(
                    &pools,
                    &self.files,
                    &c.vm_id,
                    &c.tenant_id,
                    &c.name,
                    c.generation,
                    &spec,
                )
                .await?;
                self.lv.define(def.to_xml()).await?;
                tracing::info!(vm = %c.vm_id, name = %c.name, "domain defined");
            }
            Cmd::StartVm(c) => {
                self.lv.start(&c.vm_id).await?;
                self.report_power(&c.vm_id, tx).await;
            }
            Cmd::StopVm(c) => {
                if c.force {
                    self.lv.destroy(&c.vm_id).await?
                } else {
                    self.lv.shutdown(&c.vm_id).await?
                }
                self.report_power(&c.vm_id, tx).await;
            }
            Cmd::RebootVm(c) => self.lv.reboot(&c.vm_id).await?,
            Cmd::UndefineVm(c) => {
                self.lv.undefine(&c.vm_id).await?;
                let _ = tokio::fs::remove_file(self.files.seed_path(&c.vm_id)).await;
                tracing::info!(vm = %c.vm_id, "domain removed");
            }
            Cmd::OpenConsole(c) => {
                let kind =
                    ConsoleKind::try_from(c.kind).map_err(|_| anyhow!("unknown console kind"))?;
                let source = console::open(&self.lv, &c.vm_id, kind).await?;
                let (vm_id, session) = (c.vm_id, c.session_id);
                tokio::spawn(async move {
                    tracing::info!(vm = %vm_id, ?kind, "console session started");
                    if let Err(e) = console::relay(client, session, source).await {
                        tracing::warn!(vm = %vm_id, error = %format!("{e:#}"), "console session failed");
                    }
                    tracing::info!(vm = %vm_id, "console session ended");
                });
            }
        }
        Ok(())
    }
}
