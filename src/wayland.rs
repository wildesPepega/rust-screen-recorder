//! Direct KWin screencasting through the privileged `zkde_screencast_unstable_v1` protocol.
//!
//! This is the same mechanism Spectacle and KRFB use: KWin creates a PipeWire node for a
//! monitor, a window or a region *without* going through xdg-desktop-portal, so the KDE
//! "Select what to share" dialog never appears and no permission/restore token is stored
//! anywhere. KWin only grants the interface to executables whose .desktop file lists it in
//! `X-KDE-Wayland-Interfaces` (see `install.rs`).

use anyhow::{anyhow, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::time::Duration;
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

#[allow(non_upper_case_globals, non_camel_case_types, dead_code, clippy::all)]
pub mod proto {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

use proto::zkde_screencast_stream_unstable_v1::{self as zstream, ZkdeScreencastStreamUnstableV1};
use proto::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

const POINTER_HIDDEN: u32 = 1;
const POINTER_EMBEDDED: u32 = 2;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct OutputInfo {
    pub name: String,
    pub description: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamSource {
    Output(String),
    Window(String),
    Region { x: i32, y: i32, w: u32, h: u32 },
}

enum Cmd {
    Start { src: StreamSource, cursor: bool, reply: Sender<Result<(u64, u32)>> },
    Close(u64),
    Outputs(Sender<Vec<OutputInfo>>),
}

#[derive(Clone)]
pub struct Wayland {
    tx: Sender<Cmd>,
    pub available: bool,
}

impl Wayland {
    /// Connect and start the Wayland worker thread.
    pub fn start() -> Result<Wayland> {
        let conn = Connection::connect_to_env().map_err(|e| anyhow!("cannot connect to Wayland: {e}"))?;
        let mut queue: EventQueue<State> = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut st = State::default();
        queue.roundtrip(&mut st)?;
        queue.roundtrip(&mut st)?; // output names/geometry
        let available = st.screencast.is_some();
        if !available {
            log::error!(
                "KWin did not grant zkde_screencast_unstable_v1 to this executable. \
                 Run `rust-screen-recorder install` (writes the .desktop entry that KWin checks)."
            );
        }
        let (tx, rx) = bounded::<Cmd>(64);
        std::thread::Builder::new()
            .name("wayland".into())
            .spawn(move || {
                if let Err(e) = run(conn, queue, qh, st, rx) {
                    log::error!("wayland thread ended: {e:#}");
                }
            })?;
        Ok(Wayland { tx, available })
    }

    /// Ask KWin for a PipeWire stream. Returns (stream handle, pipewire node id).
    pub fn start_stream(&self, src: StreamSource, cursor: bool) -> Result<(u64, u32)> {
        if !self.available {
            return Err(anyhow!(
                "KWin screencast interface not available. Run `rust-screen-recorder install` and restart the daemon."
            ));
        }
        let (rtx, rrx) = bounded(1);
        self.tx.send(Cmd::Start { src, cursor, reply: rtx }).map_err(|_| anyhow!("wayland thread gone"))?;
        rrx.recv_timeout(Duration::from_secs(5)).map_err(|_| anyhow!("KWin did not answer the screencast request"))?
    }

    pub fn close_stream(&self, id: u64) {
        let _ = self.tx.send(Cmd::Close(id));
    }

    pub fn outputs(&self) -> Vec<OutputInfo> {
        let (rtx, rrx) = bounded(1);
        if self.tx.send(Cmd::Outputs(rtx)).is_err() {
            return vec![];
        }
        rrx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
    }
}

#[derive(Default)]
struct Output {
    proxy: Option<wl_output::WlOutput>,
    info: OutputInfo,
}

impl Default for OutputInfo {
    fn default() -> Self {
        OutputInfo { name: String::new(), description: String::new(), x: 0, y: 0, width: 0, height: 0 }
    }
}

#[derive(Default)]
struct State {
    screencast: Option<ZkdeScreencastUnstableV1>,
    outputs: HashMap<u32, Output>,
    next_id: u64,
    streams: HashMap<u64, ZkdeScreencastStreamUnstableV1>,
    pending: HashMap<u64, Sender<Result<(u64, u32)>>>,
}

fn run(conn: Connection, mut queue: EventQueue<State>, qh: QueueHandle<State>, mut st: State, rx: Receiver<Cmd>) -> Result<()> {
    loop {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Start { src, cursor, reply } => {
                    let Some(sc) = st.screencast.clone() else {
                        let _ = reply.send(Err(anyhow!("screencast not available")));
                        continue;
                    };
                    st.next_id += 1;
                    let id = st.next_id;
                    let pointer = if cursor { POINTER_EMBEDDED } else { POINTER_HIDDEN };
                    let stream = match &src {
                        StreamSource::Output(name) => {
                            let out = st
                                .outputs
                                .values()
                                .find(|o| name.is_empty() || &o.info.name == name)
                                .or_else(|| st.outputs.values().next())
                                .and_then(|o| o.proxy.clone());
                            let Some(out) = out else {
                                let _ = reply.send(Err(anyhow!("no monitor found")));
                                continue;
                            };
                            sc.stream_output(&out, pointer, &qh, id)
                        }
                        StreamSource::Window(uuid) => sc.stream_window(uuid.clone(), pointer, &qh, id),
                        StreamSource::Region { x, y, w, h } => {
                            if sc.version() < 3 {
                                let _ = reply.send(Err(anyhow!("KWin too old for region capture")));
                                continue;
                            }
                            sc.stream_region(*x, *y, *w, *h, 1.0, pointer, &qh, id)
                        }
                    };
                    st.streams.insert(id, stream);
                    st.pending.insert(id, reply);
                }
                Cmd::Close(id) => {
                    if let Some(s) = st.streams.remove(&id) {
                        s.close();
                    }
                    st.pending.remove(&id);
                }
                Cmd::Outputs(reply) => {
                    let mut v: Vec<OutputInfo> = st.outputs.values().map(|o| o.info.clone()).collect();
                    v.sort_by_key(|o| (o.x, o.y));
                    let _ = reply.send(v);
                }
            }
        }
        conn.flush()?;
        queue.dispatch_pending(&mut st)?;
        if let Some(guard) = queue.prepare_read() {
            let fd = guard.connection_fd().as_raw_fd();
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let r = unsafe { libc::poll(&mut pfd, 1, 15) };
            if r > 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        queue.dispatch_pending(&mut st)?;
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_registry::Event::Global { name, interface, version } => {
                if interface == "zkde_screencast_unstable_v1" {
                    // v6 replaces the node id with an object serial; v5 still delivers `created`.
                    st.screencast = Some(reg.bind::<ZkdeScreencastUnstableV1, _, _>(name, version.min(5), qh, ()));
                } else if interface == "wl_output" {
                    let o = reg.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, name);
                    st.outputs.insert(name, Output { proxy: Some(o), info: OutputInfo::default() });
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                st.outputs.remove(&name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(st: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, gname: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = st.outputs.get_mut(gname) else { return };
        match ev {
            wl_output::Event::Name { name } => o.info.name = name,
            wl_output::Event::Description { description } => o.info.description = description,
            wl_output::Event::Geometry { x, y, .. } => {
                o.info.x = x;
                o.info.y = y;
            }
            wl_output::Event::Mode { flags, width, height, .. } => {
                if let wayland_client::WEnum::Value(f) = flags {
                    if f.contains(wl_output::Mode::Current) {
                        o.info.width = width;
                        o.info.height = height;
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(_: &mut Self, _: &ZkdeScreencastUnstableV1, _: proto::zkde_screencast_unstable_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, u64> for State {
    fn event(st: &mut Self, _: &ZkdeScreencastStreamUnstableV1, ev: zstream::Event, id: &u64, _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zstream::Event::Created { node } => {
                log::info!("KWin screencast stream {id} → PipeWire node {node}");
                if let Some(r) = st.pending.remove(id) {
                    let _ = r.send(Ok((*id, node)));
                }
            }
            zstream::Event::Failed { error } => {
                log::error!("KWin screencast stream {id} failed: {error}");
                if let Some(r) = st.pending.remove(id) {
                    let _ = r.send(Err(anyhow!("KWin: {error}")));
                }
                st.streams.remove(id);
            }
            zstream::Event::Closed => {
                log::info!("KWin closed screencast stream {id}");
                st.streams.remove(id);
                if let Some(r) = st.pending.remove(id) {
                    let _ = r.send(Err(anyhow!("stream closed by KWin")));
                }
            }
            _ => {}
        }
    }
}
