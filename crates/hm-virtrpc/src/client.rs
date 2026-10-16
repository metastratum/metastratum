//! Connection to a libvirt daemon over its unix socket.
//!
//! One reader task demultiplexes everything the daemon sends: replies (matched
//! to calls by serial number), asynchronous event messages, stream data and
//! keepalive pings. Writes are serialised through a lock so frames never
//! interleave.
//!
//! All reads go through `recvmsg` and never past the end of a message: libvirt
//! passes file descriptors (e.g. from `virDomainOpenGraphicsFD`) attached to
//! single bytes that follow the reply, and a read that consumed those bytes
//! without ancillary data would silently drop the descriptors.

use std::collections::{HashMap, VecDeque};
use std::io::{self, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
use tokio::io::Interest;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::protocol::{AuthListRet, ConnectOpenArgs, Error as RemoteError, remote_auth_type};
use crate::xdr::{self, Xdr};
use crate::{Error, Result};

pub const REMOTE_PROGRAM: u32 = 0x2000_8086;
pub const REMOTE_VERSION: u32 = 1;
const KEEPALIVE_PROGRAM: u32 = 0x6b65_6570;
const KEEPALIVE_PING: i32 = 1;
const KEEPALIVE_PONG: i32 = 2;

const HEADER_LEN: usize = 24;
/// Largest message the daemon accepts (`VIR_NET_MESSAGE_MAX`).
const MESSAGE_MAX: usize = 33_554_432;
/// Stream payloads are kept below the legacy limit every daemon accepts.
const STREAM_CHUNK: usize = 262_120;
const NUM_FDS_MAX: u32 = 32;

mod msg_type {
    pub const CALL: i32 = 0;
    pub const REPLY: i32 = 1;
    pub const MESSAGE: i32 = 2;
    pub const STREAM: i32 = 3;
    pub const REPLY_WITH_FDS: i32 = 5;
}

mod status {
    pub const OK: i32 = 0;
    pub const ERROR: i32 = 1;
    pub const CONTINUE: i32 = 2;
}

#[derive(Clone, Copy, Debug)]
struct Header {
    prog: u32,
    vers: u32,
    proc: i32,
    kind: i32,
    serial: u32,
    status: i32,
}

impl Header {
    fn encode(&self, w: &mut Vec<u8>) {
        xdr::put_u32(w, self.prog);
        xdr::put_u32(w, self.vers);
        xdr::put_i32(w, self.proc);
        xdr::put_i32(w, self.kind);
        xdr::put_u32(w, self.serial);
        xdr::put_i32(w, self.status);
    }

    fn decode(r: &mut xdr::Reader<'_>) -> xdr::Result<Self> {
        Ok(Self {
            prog: xdr::get_u32(r)?,
            vers: xdr::get_u32(r)?,
            proc: xdr::get_i32(r)?,
            kind: xdr::get_i32(r)?,
            serial: xdr::get_u32(r)?,
            status: xdr::get_i32(r)?,
        })
    }
}

fn frame(header: Header, body: &[u8]) -> Vec<u8> {
    let mut w = Vec::with_capacity(4 + HEADER_LEN + body.len());
    xdr::put_u32(&mut w, (4 + HEADER_LEN + body.len()) as u32);
    header.encode(&mut w);
    w.extend_from_slice(body);
    w
}

/// A reply body and any file descriptors passed with it.
struct Reply {
    body: Vec<u8>,
    fds: Vec<OwnedFd>,
}

/// An asynchronous message from the daemon, such as a domain event.
#[derive(Clone, Debug)]
pub struct Event {
    /// Procedure number identifying the message type.
    pub proc: i32,
    pub body: Arc<Vec<u8>>,
}

enum StreamMsg {
    Data(Vec<u8>),
    Error(Error),
}

#[derive(Default)]
struct State {
    closed: bool,
    pending: HashMap<u32, oneshot::Sender<Result<Reply>>>,
    streams: HashMap<u32, mpsc::UnboundedSender<StreamMsg>>,
}

/// Everything the reader task and the stream handles share.
struct Shared {
    sock: UnixStream,
    write_lock: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
}

impl Shared {
    async fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut off = 0;
        while off < bytes.len() {
            self.sock.writable().await?;
            match self.sock.try_write(&bytes[off..]) {
                Ok(n) => off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Reads exactly `buf.len()` bytes, collecting any passed descriptors.
    async fn recv_exact(&self, buf: &mut [u8], fds: &mut VecDeque<OwnedFd>) -> io::Result<()> {
        let mut off = 0;
        while off < buf.len() {
            self.sock.readable().await?;
            let res = self.sock.try_io(Interest::READABLE, || {
                let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
                let mut control = RecvAncillaryBuffer::new(&mut space);
                let got = rustix::net::recvmsg(
                    &self.sock,
                    &mut [IoSliceMut::new(&mut buf[off..])],
                    &mut control,
                    RecvFlags::CMSG_CLOEXEC,
                )
                .map_err(io::Error::from)?;
                for msg in control.drain() {
                    if let RecvAncillaryMessage::ScmRights(received) = msg {
                        fds.extend(received);
                    }
                }
                Ok(got.bytes)
            });
            match res {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn fail_all(&self) {
        let mut st = self.state.lock().unwrap();
        st.closed = true;
        for (_, waiter) in st.pending.drain() {
            let _ = waiter.send(Err(Error::Closed));
        }
        st.streams.clear();
    }
}

fn remote_error(body: &[u8]) -> Error {
    match xdr::from_bytes::<RemoteError>(body) {
        Ok(e) => Error::Libvirt {
            code: e.code,
            domain: e.domain,
            message: e.message.unwrap_or_default(),
        },
        Err(e) => Error::Xdr(e),
    }
}

async fn read_loop(shared: Arc<Shared>) -> Result<()> {
    let mut fds = VecDeque::new();
    loop {
        let mut len = [0u8; 4];
        shared.recv_exact(&mut len, &mut fds).await?;
        let len = u32::from_be_bytes(len) as usize;
        if !(4 + HEADER_LEN..=MESSAGE_MAX).contains(&len) {
            return Err(Error::Protocol(format!("bad message length {len}")));
        }
        let mut msg = vec![0u8; len - 4];
        shared.recv_exact(&mut msg, &mut fds).await?;
        let mut r = xdr::Reader::new(&msg);
        let h = Header::decode(&mut r)?;
        let mut body = &msg[HEADER_LEN..];

        let mut passed = Vec::new();
        if h.kind == msg_type::REPLY_WITH_FDS {
            let n = xdr::get_u32(&mut r)?;
            if n > NUM_FDS_MAX {
                return Err(Error::Protocol(format!("{n} descriptors passed")));
            }
            body = &body[4..];
            // Each descriptor rides on one byte sent after the message.
            for _ in 0..n {
                shared.recv_exact(&mut [0u8; 1], &mut fds).await?;
                passed.push(
                    fds.pop_front()
                        .ok_or_else(|| Error::Protocol("descriptor missing".into()))?,
                );
            }
        }

        match (h.prog, h.kind) {
            (KEEPALIVE_PROGRAM, msg_type::MESSAGE) if h.proc == KEEPALIVE_PING => {
                let pong = Header {
                    prog: KEEPALIVE_PROGRAM,
                    vers: 1,
                    proc: KEEPALIVE_PONG,
                    kind: msg_type::MESSAGE,
                    serial: 0,
                    status: status::OK,
                };
                shared.send(&frame(pong, &[])).await?;
            }
            (REMOTE_PROGRAM, msg_type::REPLY | msg_type::REPLY_WITH_FDS) => {
                let waiter = shared.state.lock().unwrap().pending.remove(&h.serial);
                if let Some(waiter) = waiter {
                    let reply = match h.status {
                        status::OK => Ok(Reply {
                            body: body.to_vec(),
                            fds: passed,
                        }),
                        _ => Err(remote_error(body)),
                    };
                    let _ = waiter.send(reply);
                }
            }
            (REMOTE_PROGRAM, msg_type::MESSAGE) => {
                let _ = shared.events.send(Event {
                    proc: h.proc,
                    body: Arc::new(body.to_vec()),
                });
            }
            (REMOTE_PROGRAM, msg_type::STREAM) => {
                let mut st = shared.state.lock().unwrap();
                let msg = match h.status {
                    status::CONTINUE => Some(StreamMsg::Data(body.to_vec())),
                    status::ERROR => Some(StreamMsg::Error(remote_error(body))),
                    // OK: end of stream.
                    _ => None,
                };
                match msg {
                    Some(m) => {
                        if let Some(tx) = st.streams.get(&h.serial) {
                            let _ = tx.send(m);
                        }
                    }
                    None => {
                        st.streams.remove(&h.serial);
                    }
                }
            }
            _ => tracing::debug!(
                prog = h.prog,
                kind = h.kind,
                proc = h.proc,
                "ignoring message"
            ),
        }
    }
}

struct Inner {
    shared: Arc<Shared>,
    serial: AtomicU32,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Ends the reader task, which fails anything still waiting.
        let _ = rustix::net::shutdown(&self.shared.sock, rustix::net::Shutdown::Both);
    }
}

/// A connection to a libvirt daemon. Cheap to clone; the connection closes
/// when the last clone is dropped.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    /// Connects to a daemon socket, authenticates (polkit or none) and opens
    /// the hypervisor connection `uri`, e.g. `qemu:///system`.
    pub async fn connect(socket: impl AsRef<Path>, uri: &str) -> Result<Self> {
        let sock = UnixStream::connect(socket.as_ref()).await?;
        let (events, _) = broadcast::channel(256);
        let shared = Arc::new(Shared {
            sock,
            write_lock: tokio::sync::Mutex::new(()),
            state: Mutex::default(),
            events,
        });
        let reader = shared.clone();
        tokio::spawn(async move {
            if let Err(e) = read_loop(reader.clone()).await {
                tracing::debug!(error = %e, "libvirt connection closed");
            }
            reader.fail_all();
        });
        let client = Self {
            inner: Arc::new(Inner {
                shared,
                serial: AtomicU32::new(1),
            }),
        };

        let auth: AuthListRet = client.auth_list().await?;
        if !auth.types.is_empty() && !auth.types.contains(&remote_auth_type::REMOTE_AUTH_NONE) {
            if auth.types.contains(&remote_auth_type::REMOTE_AUTH_POLKIT) {
                client.auth_polkit().await?;
            } else {
                return Err(Error::Auth(format!(
                    "unsupported authentication methods {:?}",
                    auth.types
                )));
            }
        }
        client
            .connect_open(&ConnectOpenArgs {
                name: Some(uri.to_owned()),
                flags: 0,
            })
            .await?;
        Ok(client)
    }

    pub fn is_closed(&self) -> bool {
        self.inner.shared.state.lock().unwrap().closed
    }

    /// Subscribes to asynchronous messages (events registered by callers).
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.inner.shared.events.subscribe()
    }

    fn next_serial(&self) -> u32 {
        self.inner.serial.fetch_add(1, Ordering::Relaxed)
    }

    async fn call_raw<A: Xdr>(&self, proc: i32, serial: u32, args: &A) -> Result<Reply> {
        let body = xdr::to_bytes(args)?;
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.inner.shared.state.lock().unwrap();
            if st.closed {
                return Err(Error::Closed);
            }
            st.pending.insert(serial, tx);
        }
        let header = Header {
            prog: REMOTE_PROGRAM,
            vers: REMOTE_VERSION,
            proc,
            kind: msg_type::CALL,
            serial,
            status: status::OK,
        };
        if let Err(e) = self.inner.shared.send(&frame(header, &body)).await {
            self.inner
                .shared
                .state
                .lock()
                .unwrap()
                .pending
                .remove(&serial);
            return Err(e.into());
        }
        rx.await.map_err(|_| Error::Closed)?
    }

    /// Calls a procedure and decodes its reply.
    pub async fn call<A: Xdr, R: Xdr>(&self, proc: i32, args: &A) -> Result<R> {
        let reply = self.call_raw(proc, self.next_serial(), args).await?;
        Ok(xdr::from_bytes(&reply.body)?)
    }

    /// Calls a procedure whose reply passes file descriptors.
    pub async fn call_with_fds<A: Xdr, R: Xdr>(
        &self,
        proc: i32,
        args: &A,
    ) -> Result<(R, Vec<OwnedFd>)> {
        let reply = self.call_raw(proc, self.next_serial(), args).await?;
        Ok((xdr::from_bytes(&reply.body)?, reply.fds))
    }

    /// Calls a procedure that opens a data stream (e.g. a console).
    pub async fn open_stream<A: Xdr>(&self, proc: i32, args: &A) -> Result<Stream> {
        let serial = self.next_serial();
        let (tx, rx) = mpsc::unbounded_channel();
        // Register first: stream data may arrive right behind the reply.
        self.inner
            .shared
            .state
            .lock()
            .unwrap()
            .streams
            .insert(serial, tx);
        if let Err(e) = self.call_raw(proc, serial, args).await {
            self.inner
                .shared
                .state
                .lock()
                .unwrap()
                .streams
                .remove(&serial);
            return Err(e);
        }
        Ok(Stream {
            rx: StreamRx { rx },
            tx: StreamTx {
                shared: self.inner.shared.clone(),
                proc,
                serial,
                finished: false,
            },
        })
    }
}

/// A bidirectional data stream (console, upload/download).
pub struct Stream {
    pub rx: StreamRx,
    pub tx: StreamTx,
}

impl Stream {
    pub fn split(self) -> (StreamRx, StreamTx) {
        (self.rx, self.tx)
    }
}

pub struct StreamRx {
    rx: mpsc::UnboundedReceiver<StreamMsg>,
}

impl StreamRx {
    /// The next chunk of data; `None` once the stream has ended.
    pub async fn recv(&mut self) -> Option<Result<Vec<u8>>> {
        match self.rx.recv().await? {
            StreamMsg::Data(d) => Some(Ok(d)),
            StreamMsg::Error(e) => Some(Err(e)),
        }
    }
}

pub struct StreamTx {
    shared: Arc<Shared>,
    proc: i32,
    serial: u32,
    finished: bool,
}

impl StreamTx {
    fn header(&self, status: i32) -> Header {
        Header {
            prog: REMOTE_PROGRAM,
            vers: REMOTE_VERSION,
            proc: self.proc,
            kind: msg_type::STREAM,
            serial: self.serial,
            status,
        }
    }

    pub async fn send(&self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(STREAM_CHUNK) {
            self.shared
                .send(&frame(self.header(status::CONTINUE), chunk))
                .await?;
        }
        Ok(())
    }

    /// Ends the stream normally.
    pub async fn finish(mut self) -> Result<()> {
        self.finished = true;
        self.shared
            .send(&frame(self.header(status::OK), &[]))
            .await?;
        Ok(())
    }
}

impl Drop for StreamTx {
    /// Dropping an unfinished stream aborts it on the daemon side.
    fn drop(&mut self) {
        self.shared
            .state
            .lock()
            .unwrap()
            .streams
            .remove(&self.serial);
        if self.finished {
            return;
        }
        let (shared, abort) = (self.shared.clone(), frame(self.header(status::ERROR), &[]));
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = shared.send(&abort).await;
            });
        }
    }
}
