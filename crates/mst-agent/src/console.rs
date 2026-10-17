//! Console tunnels: the agent connects the VM's VNC server or serial console
//! to a `ConsoleTunnel` stream it dials back to the controller.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use mst_libvirt::{Libvirt, Stream};
use mst_proto::agent_control_client::AgentControlClient;
use mst_proto::{ConsoleKind, TunnelChunk, tunnel_chunk};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

/// The console sessions open on each VM, so that they can be closed before
/// the VM's domain goes away (destroyed, removed, migrated): libvirt (11.3)
/// can crash writing to a console stream whose domain is gone, taking the
/// host's other VMs' commands, and VMs being started, with it.
#[derive(Default)]
pub struct Open {
    next: std::sync::atomic::AtomicU64,
    by_vm: std::sync::Mutex<HashMap<String, Vec<Session>>>,
}

/// A console session: its number, and the task relaying it.
type Session = (u64, tokio::task::JoinHandle<()>);

impl Open {
    /// Runs a console session (`relay`) for `vm`, until it ends or is closed.
    pub fn spawn(
        self: &Arc<Self>,
        vm: String,
        relay: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let open = self.clone();
        let key = vm.clone();
        // Held while spawning, so that a session ending at once still finds
        // itself to remove.
        let mut by_vm = self.by_vm.lock().unwrap();
        let task = tokio::spawn(async move {
            relay.await;
            let mut by_vm = open.by_vm.lock().unwrap();
            if let Some(sessions) = by_vm.get_mut(&key) {
                sessions.retain(|(i, _)| *i != id);
                if sessions.is_empty() {
                    by_vm.remove(&key);
                }
            }
        });
        by_vm.entry(vm).or_default().push((id, task));
    }

    /// Ends `vm`'s console sessions, and waits until their streams are
    /// let go of.
    pub async fn close(&self, vm: &str) {
        let sessions = self.by_vm.lock().unwrap().remove(vm).unwrap_or_default();
        for (_, task) in sessions {
            task.abort();
            let _ = task.await;
            tracing::info!(%vm, "console session closed");
        }
    }
}

pub enum Source {
    Vnc(tokio::net::UnixStream),
    Serial(Stream),
}

/// Opens the console device. Done before acknowledging the command, so the
/// controller learns about failures (VM not running, ...) immediately.
pub async fn open(lv: &Libvirt, vm_id: &str, kind: ConsoleKind) -> Result<Source> {
    match kind {
        ConsoleKind::Vnc => {
            let fd = lv.open_vnc(vm_id).await?;
            let std = std::os::unix::net::UnixStream::from(fd);
            std.set_nonblocking(true)?;
            Ok(Source::Vnc(tokio::net::UnixStream::from_std(std)?))
        }
        ConsoleKind::Serial => Ok(Source::Serial(lv.open_serial(vm_id).await?)),
        ConsoleKind::Unspecified => anyhow::bail!("unspecified console kind"),
    }
}

fn data(bytes: Vec<u8>) -> TunnelChunk {
    TunnelChunk {
        msg: Some(tunnel_chunk::Msg::Data(bytes)),
    }
}

/// Dials the tunnel and relays until either side closes.
pub async fn relay(
    mut client: AgentControlClient<Channel>,
    session_id: String,
    source: Source,
) -> Result<()> {
    let (to_ctl, rx) = mpsc::channel::<TunnelChunk>(64);
    to_ctl
        .send(TunnelChunk {
            msg: Some(tunnel_chunk::Msg::SessionId(session_id)),
        })
        .await?;
    let mut from_ctl = client
        .console_tunnel(ReceiverStream::new(rx))
        .await
        .context("dialling console tunnel")?
        .into_inner();

    match source {
        Source::Vnc(sock) => {
            let (mut rd, mut wr) = sock.into_split();
            let up = async {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match rd.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if to_ctl.send(data(buf[..n].to_vec())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            };
            let down = async {
                while let Ok(Some(chunk)) = from_ctl.message().await {
                    if let Some(tunnel_chunk::Msg::Data(d)) = chunk.msg
                        && wr.write_all(&d).await.is_err()
                    {
                        break;
                    }
                }
            };
            tokio::select! { () = up => {}, () = down => {} }
        }
        Source::Serial(stream) => {
            // Dropping `tx` when either side ends aborts the console stream.
            let (mut rx, tx) = stream.split();
            let up = async {
                while let Some(Ok(d)) = rx.recv().await {
                    if to_ctl.send(data(d)).await.is_err() {
                        break;
                    }
                }
            };
            let down = async {
                while let Ok(Some(chunk)) = from_ctl.message().await {
                    if let Some(tunnel_chunk::Msg::Data(d)) = chunk.msg
                        && tx.send(&d).await.is_err()
                    {
                        break;
                    }
                }
            };
            tokio::select! { () = up => {}, () = down => {} }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closing_ends_a_vms_sessions_only() {
        let open = Arc::new(Open::default());
        // Dropped when its session is ended: the stream let go of.
        let (a_tx, mut a_rx) = mpsc::channel::<()>(1);
        let (b_tx, mut b_rx) = mpsc::channel::<()>(1);
        open.spawn("a".into(), async move {
            let _held = a_tx;
            std::future::pending::<()>().await;
        });
        open.spawn("b".into(), async move {
            let _held = b_tx;
            std::future::pending::<()>().await;
        });
        open.close("a").await;
        assert!(a_rx.recv().await.is_none(), "a's session ended");
        assert!(
            b_rx.try_recv()
                .is_err_and(|e| e == mpsc::error::TryRecvError::Empty)
        );

        // Sessions that end by themselves are forgotten.
        open.spawn("c".into(), async {});
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while open.by_vm.lock().unwrap().contains_key("c") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("c forgotten");
        assert_eq!(open.by_vm.lock().unwrap().len(), 1, "b still open");
    }
}
