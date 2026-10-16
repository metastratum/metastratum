//! The control session with the controller: hello, heartbeats, commands.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use hm_libvirt::{Libvirt, Power};
use hm_proto::agent_control_client::AgentControlClient;
use hm_proto::command::Cmd;
use hm_proto::{
    AgentMessage, CommandResult, ConsoleKind, DomainState, Heartbeat, Hello, HostFacts, PowerState,
    agent_message, controller_message,
};
use hm_storage::LocalPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};

use crate::enroll::HostIdentity;
use crate::{console, vm};

const HEARTBEAT: Duration = Duration::from_secs(5);

pub struct Agent {
    pub controller: String,
    pub identity: HostIdentity,
    pub facts: HostFacts,
    pub lv: Libvirt,
    pub pool: LocalPool,
    pub files: vm::VmFiles,
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
                power: power(d.power) as i32,
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
                loop {
                    tokio::time::sleep(HEARTBEAT).await;
                    let domains = match this.domains().await {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::warn!(error = %e, "listing domains");
                            continue;
                        }
                    };
                    let msg = AgentMessage {
                        msg: Some(agent_message::Msg::Heartbeat(Heartbeat { domains })),
                    };
                    if tx.send(msg).await.is_err() {
                        break;
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
        result
    }

    async fn report_power(&self, vm_id: &str, tx: &mpsc::Sender<AgentMessage>) {
        if let Ok(p) = self.lv.power(vm_id).await {
            let d = DomainState {
                vm_id: vm_id.to_owned(),
                generation: 0,
                power: power(p) as i32,
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
                self.pool
                    .ensure_image(&c.image_id, &c.source_url, &c.format)
                    .await?;
            }
            Cmd::EnsureVolume(c) => {
                let image = Some(c.image_id.as_str()).filter(|s| !s.is_empty());
                self.pool
                    .ensure_volume(&c.volume_id, c.size_bytes, image)
                    .await?;
            }
            Cmd::DeleteVolume(c) => self.pool.delete_volume(&c.volume_id).await?,
            Cmd::DefineVm(c) => {
                let spec: hm_core::vm::VmSpec =
                    serde_json::from_str(&c.spec_json).context("invalid VM spec")?;
                let def = vm::domain(
                    &self.pool,
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
