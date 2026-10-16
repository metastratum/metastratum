//! Console tunnels: the agent connects the VM's VNC server or serial console
//! to a `ConsoleTunnel` stream it dials back to the controller.

use anyhow::{Context, Result};
use hm_libvirt::{Libvirt, Stream};
use hm_proto::agent_control_client::AgentControlClient;
use hm_proto::{ConsoleKind, TunnelChunk, tunnel_chunk};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

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
            // libvirt streams are blocking: bridge them with threads. The
            // reader exits when the stream ends; a later session opened with
            // the force flag also ends this one.
            let stream = std::sync::Arc::new(stream);
            let reader = {
                let (stream, to_ctl) = (stream.clone(), to_ctl.clone());
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; 16 * 1024];
                    while let Ok(n) = stream.recv(&mut buf) {
                        if n == 0 || to_ctl.blocking_send(data(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                })
            };
            let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(64);
            let writer = {
                let stream = stream.clone();
                std::thread::spawn(move || {
                    while let Some(d) = in_rx.blocking_recv() {
                        if stream.send(&d).is_err() {
                            break;
                        }
                    }
                })
            };
            while let Ok(Some(chunk)) = from_ctl.message().await {
                if let Some(tunnel_chunk::Msg::Data(d)) = chunk.msg
                    && in_tx.send(d).await.is_err()
                {
                    break;
                }
            }
            drop(in_tx);
            let _ = tokio::task::spawn_blocking(move || writer.join()).await;
            drop(reader);
        }
    }
    Ok(())
}
