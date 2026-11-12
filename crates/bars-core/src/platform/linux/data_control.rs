//! The Wayland clipboard, owned directly through the wlr/ext data-control
//! protocols. wl-clipboard-rs can't say when an app reads what it offers,
//! and that read is the proof a paste landed. One connection lives on its
//! own thread: it serves our offers, follows the current selection, and
//! reads every type on it so a paste can put it all back afterwards.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use parking_lot::Mutex;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self as ext_device, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self as ext_offer, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self as ext_source, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self as wlr_device, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self as wlr_offer, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self as wlr_source, ZwlrDataControlSourceV1},
};

/// Bytes per MIME type.
pub type Contents = Vec<(String, Arc<[u8]>)>;

/// The clipboard and primary selection as they were before a paste. `None`
/// leaves that selection alone; an empty list clears it.
pub struct Saved {
    regular: Option<Contents>,
    primary: Option<Contents>,
}

/// Names for plain text, best first. X11 apps (through Xwayland) ask for
/// the upper-case ones.
const TEXT_TYPES: &[&str] = &["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT", "COMPOUND_TEXT"];
/// Clipboard managers (Klipper, cliphist) skip offers carrying this.
const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";
/// X11 selection plumbing, not content.
const X11_META: &[&str] = &["TARGETS", "MULTIPLE", "TIMESTAMP", "SAVE_TARGETS", "DELETE", "INCR"];
/// Larger types aren't kept (nor restored).
const MAX_TYPE_BYTES: u64 = 32 << 20;
/// Reading the old clipboard gives up after this.
const SAVE_TIMEOUT: Duration = Duration::from_millis(1000);

fn is_text(mime: &str) -> bool {
    TEXT_TYPES.iter().any(|t| t.eq_ignore_ascii_case(mime))
}

/// Puts `text` on the clipboard (and the primary selection when `primary`)
/// as an offer hidden from clipboard managers. The receiver gets a message
/// each time an app reads the text.
pub fn offer_text(text: &str, primary: bool) -> anyhow::Result<Receiver<()>> {
    let bytes: Arc<[u8]> = text.as_bytes().into();
    let mut contents: Contents = TEXT_TYPES.iter().map(|t| (t.to_string(), bytes.clone())).collect();
    contents.push((PASSWORD_HINT.into(), b"secret".as_slice().into()));
    let (reply, rx) = bounded(1);
    client()?.send(Request::Offer {
        primary: primary.then(|| contents.clone()),
        regular: Some(contents),
        watch: TEXT_TYPES.iter().map(|t| t.to_string()).collect(),
        reply,
    })?;
    rx.recv_timeout(Duration::from_secs(1)).context("the clipboard didn't answer")
}

/// Reads everything on the clipboard (and the primary selection).
pub fn save(primary: bool) -> Option<Saved> {
    let client = client().ok()?;
    let read = |primary: bool| -> Option<Contents> {
        let (reply, rx) = bounded(1);
        client.send(Request::Save { primary, reply }).ok()?;
        match rx.recv_timeout(SAVE_TIMEOUT).ok()? {
            // Empty: restoring clears it again.
            None => Some(Vec::new()),
            Some(pipes) => read_pipes(pipes),
        }
    };
    Some(Saved {
        regular: read(false),
        primary: if primary { read(true) } else { None },
    })
}

/// Puts back what [`save`] read.
pub fn restore(saved: Saved) {
    if saved.regular.is_none() && saved.primary.is_none() {
        return;
    }
    let Ok(client) = client() else { return };
    let (reply, rx) = bounded(1);
    let request = Request::Offer {
        regular: saved.regular,
        primary: saved.primary,
        watch: Vec::new(),
        reply,
    };
    if client.send(request).is_ok() {
        let _ = rx.recv_timeout(Duration::from_secs(1));
    }
}

/// Our latest offer is still the clipboard (nobody copied since).
pub fn still_ours() -> bool {
    let Ok(client) = client() else { return false };
    let (reply, rx) = bounded(1);
    client.send(Request::StillOurs(reply)).is_ok() && rx.recv_timeout(Duration::from_secs(1)).unwrap_or(false)
}

/// Reads the pipes [`Request::Save`] opened, all at once, until the deadline.
/// None when nothing could be read (so the clipboard isn't wiped).
fn read_pipes(pipes: Vec<(Vec<String>, OwnedFd)>) -> Option<Contents> {
    if pipes.is_empty() {
        return Some(Vec::new());
    }
    let deadline = Instant::now() + SAVE_TIMEOUT;
    let readers: Vec<_> = pipes
        .into_iter()
        .map(|(serve_as, fd)| {
            let (tx, rx) = bounded(1);
            thread::spawn(move || {
                let mut buf = Vec::new();
                let ok = File::from(fd).take(MAX_TYPE_BYTES + 1).read_to_end(&mut buf).is_ok();
                let _ = tx.send((ok && buf.len() as u64 <= MAX_TYPE_BYTES).then_some(buf));
            });
            (serve_as, rx)
        })
        .collect();
    let mut out = Contents::new();
    for (serve_as, rx) in readers {
        if let Ok(Some(buf)) = rx.recv_deadline(deadline) {
            let data: Arc<[u8]> = buf.into();
            out.extend(serve_as.into_iter().map(|m| (m, data.clone())));
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Which types of a selection to read, and the types each one is served
/// as: plain text once for all its names, one image format (apps offer
/// the same image in many encodings, each made on request), everything
/// else (HTML, file lists, app formats) as is.
fn plan_reads(mimes: &[String]) -> Vec<(String, Vec<String>)> {
    let mut plan = Vec::new();
    let texts: Vec<String> = mimes.iter().filter(|m| is_text(m)).cloned().collect();
    let best_text = TEXT_TYPES
        .iter()
        .find_map(|t| texts.iter().find(|m| m.eq_ignore_ascii_case(t)))
        .cloned();
    if let Some(best) = best_text {
        plan.push((best, texts));
    }
    let images: Vec<&String> = mimes.iter().filter(|m| m.starts_with("image/")).collect();
    if let Some(img) = images.iter().find(|m| m.as_str() == "image/png").or(images.first()) {
        plan.push(((*img).clone(), vec![(*img).clone()]));
    }
    for m in mimes {
        if !is_text(m) && !m.starts_with("image/") && !X11_META.contains(&m.as_str()) {
            plan.push((m.clone(), vec![m.clone()]));
        }
    }
    plan
}

// ---- The clipboard thread ---------------------------------------------------

/// A selection's pipes, each with the types it's served as.
type SavedPipes = Vec<(Vec<String>, OwnedFd)>;

enum Request {
    Offer {
        regular: Option<Contents>,
        primary: Option<Contents>,
        /// Types whose reads are reported.
        watch: Vec<String>,
        reply: Sender<Receiver<()>>,
    },
    /// Pipes carrying each planned type of the current selection, with the
    /// types it's served as. None when the selection is empty.
    Save {
        primary: bool,
        reply: Sender<Option<SavedPipes>>,
    },
    StillOurs(Sender<bool>),
}

struct Client {
    tx: Sender<Request>,
    /// Wakes the thread from poll() when a request is queued.
    wake: OwnedFd,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl Client {
    fn send(&self, req: Request) -> anyhow::Result<()> {
        self.tx.send(req).map_err(|_| anyhow!("the clipboard thread stopped"))?;
        let _ = nix::unistd::write(&self.wake, b"x");
        Ok(())
    }
}

/// The running clipboard thread, started (again, after the compositor
/// went away) on first use.
fn client() -> anyhow::Result<Arc<Client>> {
    static CLIENT: Mutex<Option<Arc<Client>>> = Mutex::new(None);
    let mut slot = CLIENT.lock();
    if let Some(c) = slot.as_ref().filter(|c| c.alive.load(std::sync::atomic::Ordering::Acquire)) {
        return Ok(c.clone());
    }
    let c = Arc::new(start()?);
    *slot = Some(c.clone());
    Ok(c)
}

#[derive(Clone)]
enum Manager {
    Wlr(ZwlrDataControlManagerV1),
    Ext(ExtDataControlManagerV1),
}

#[derive(Clone)]
enum Device {
    Wlr(ZwlrDataControlDeviceV1),
    Ext(ExtDataControlDeviceV1),
}

#[derive(Clone, PartialEq)]
enum Source {
    Wlr(ZwlrDataControlSourceV1),
    Ext(ExtDataControlSourceV1),
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Offer {
    Wlr(ZwlrDataControlOfferV1),
    Ext(ExtDataControlOfferV1),
}

impl Manager {
    fn device(&self, seat: &WlSeat, qh: &QueueHandle<State>) -> Device {
        match self {
            Manager::Wlr(m) => Device::Wlr(m.get_data_device(seat, qh, ())),
            Manager::Ext(m) => Device::Ext(m.get_data_device(seat, qh, ())),
        }
    }

    fn source(&self, qh: &QueueHandle<State>) -> Source {
        match self {
            Manager::Wlr(m) => Source::Wlr(m.create_data_source(qh, ())),
            Manager::Ext(m) => Source::Ext(m.create_data_source(qh, ())),
        }
    }

    /// wlr-data-control has the primary selection from version 2.
    fn has_primary(&self) -> bool {
        match self {
            Manager::Wlr(m) => m.version() >= 2,
            Manager::Ext(_) => true,
        }
    }
}

impl Device {
    fn set(&self, source: Option<&Source>, primary: bool) {
        match (self, source) {
            (Device::Wlr(d), Some(Source::Wlr(s))) if primary => d.set_primary_selection(Some(s)),
            (Device::Wlr(d), Some(Source::Wlr(s))) => d.set_selection(Some(s)),
            (Device::Wlr(d), None) if primary => d.set_primary_selection(None),
            (Device::Wlr(d), None) => d.set_selection(None),
            (Device::Ext(d), Some(Source::Ext(s))) if primary => d.set_primary_selection(Some(s)),
            (Device::Ext(d), Some(Source::Ext(s))) => d.set_selection(Some(s)),
            (Device::Ext(d), None) if primary => d.set_primary_selection(None),
            (Device::Ext(d), None) => d.set_selection(None),
            _ => {}
        }
    }

    fn destroy(&self) {
        match self {
            Device::Wlr(d) => d.destroy(),
            Device::Ext(d) => d.destroy(),
        }
    }
}

impl Source {
    fn offer(&self, mime: &str) {
        match self {
            Source::Wlr(s) => s.offer(mime.to_string()),
            Source::Ext(s) => s.offer(mime.to_string()),
        }
    }

    fn destroy(&self) {
        match self {
            Source::Wlr(s) => s.destroy(),
            Source::Ext(s) => s.destroy(),
        }
    }
}

impl Offer {
    fn receive(&self, mime: &str, fd: BorrowedFd) {
        match self {
            Offer::Wlr(o) => o.receive(mime.to_string(), fd),
            Offer::Ext(o) => o.receive(mime.to_string(), fd),
        }
    }

    fn destroy(&self) {
        match self {
            Offer::Wlr(o) => o.destroy(),
            Offer::Ext(o) => o.destroy(),
        }
    }
}

struct Served {
    source: Source,
    contents: Contents,
    primary: bool,
    reads: Option<(Sender<()>, Vec<String>)>,
}

#[derive(Default)]
struct State {
    /// Types announced for each offer the compositor made.
    offers: HashMap<Offer, Vec<String>>,
    selection: Option<Offer>,
    primary: Option<Offer>,
    served: Vec<Served>,
    /// Our latest clipboard offer, until it's replaced.
    latest: Option<Source>,
    device_gone: bool,
}

impl State {
    fn on_offer_created(&mut self, offer: Offer) {
        self.offers.insert(offer, Vec::new());
    }

    fn on_selection(&mut self, offer: Option<Offer>, primary: bool) {
        let slot = if primary { &mut self.primary } else { &mut self.selection };
        let old = std::mem::replace(slot, offer);
        let other = if primary { &self.selection } else { &self.primary };
        if let Some(old) = old.filter(|o| other.as_ref() != Some(o)) {
            old.destroy();
            self.offers.remove(&old);
        }
    }

    fn on_send(&mut self, source: &Source, mime: String, fd: OwnedFd) {
        let Some(s) = self.served.iter().find(|s| &s.source == source) else { return };
        if let Some((tx, watch)) = &s.reads
            && watch.contains(&mime)
        {
            let _ = tx.send(());
        }
        let Some(data) = s.contents.iter().find(|(m, _)| *m == mime).map(|(_, d)| d.clone()) else { return };
        // A slow reader mustn't hold up the clipboard thread.
        thread::spawn(move || {
            let _ = fcntl(&fd, FcntlArg::F_SETFL(OFlag::empty()));
            let _ = File::from(fd).write_all(&data);
        });
    }

    fn on_cancelled(&mut self, source: &Source) {
        self.served.retain(|s| &s.source != source);
        if self.latest.as_ref() == Some(source) {
            self.latest = None;
        }
        source.destroy();
    }
}

fn start() -> anyhow::Result<Client> {
    let conn = Connection::connect_to_env().context("no Wayland display")?;
    let (globals, queue) = registry_queue_init::<State>(&conn).context("Wayland registry")?;
    let qh = queue.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=8, ()).context("no seat")?;
    let manager = match globals.bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ()) {
        Ok(m) => Manager::Ext(m),
        Err(_) => Manager::Wlr(
            globals
                .bind::<ZwlrDataControlManagerV1, _, _>(&qh, 1..=2, ())
                .context("the compositor has no clipboard-control protocol")?,
        ),
    };
    let (tx, rx) = unbounded();
    let (wake_r, wake_w) = nix::unistd::pipe2(OFlag::O_NONBLOCK | OFlag::O_CLOEXEC)?;
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let alive2 = alive.clone();
    thread::Builder::new().name("bars-clipboard".into()).spawn(move || {
        if let Err(e) = run(queue, manager, seat, rx, wake_r) {
            log::warn!("clipboard: {e:#}");
        }
        alive2.store(false, std::sync::atomic::Ordering::Release);
    })?;
    Ok(Client { tx, wake: wake_w, alive })
}

fn run(
    mut queue: EventQueue<State>,
    manager: Manager,
    seat: WlSeat,
    rx: Receiver<Request>,
    wake: OwnedFd,
) -> anyhow::Result<()> {
    let qh = queue.handle();
    let mut state = State::default();
    let mut device = manager.device(&seat, &qh);
    // The compositor announces the current selection to a new device.
    queue.roundtrip(&mut state)?;
    loop {
        if state.device_gone {
            device.destroy();
            device = manager.device(&seat, &qh);
            state.device_gone = false;
            state.selection = None;
            state.primary = None;
            queue.roundtrip(&mut state)?;
        }
        while let Ok(req) = rx.try_recv() {
            // Read the selection as it is now, not as last dispatched.
            if matches!(req, Request::Save { .. }) {
                queue.roundtrip(&mut state)?;
            }
            handle(req, &mut state, &manager, &device, &qh);
        }
        queue.flush()?;
        queue.dispatch_pending(&mut state)?;
        let Some(guard) = queue.prepare_read() else { continue };
        let (wayland, woken) = {
            let mut fds = [
                PollFd::new(guard.connection_fd(), PollFlags::POLLIN),
                PollFd::new(wake.as_fd(), PollFlags::POLLIN),
            ];
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
            let ready = |f: &PollFd| f.revents().is_some_and(|r| !r.is_empty());
            (ready(&fds[0]), ready(&fds[1]))
        };
        if wayland {
            guard.read()?;
        } else {
            drop(guard);
        }
        if woken {
            let mut buf = [0u8; 64];
            while nix::unistd::read(&wake, &mut buf).is_ok_and(|n| n > 0) {}
        }
        queue.dispatch_pending(&mut state)?;
    }
}

fn handle(req: Request, state: &mut State, manager: &Manager, device: &Device, qh: &QueueHandle<State>) {
    match req {
        Request::Offer { regular, primary, watch, reply } => {
            let (tx, rx) = unbounded();
            let reads = (!watch.is_empty()).then_some((tx, watch));
            let selections = [(regular, false), (primary.filter(|_| manager.has_primary()), true)];
            for (contents, is_primary) in selections {
                let Some(contents) = contents else { continue };
                if contents.is_empty() {
                    device.set(None, is_primary);
                    continue;
                }
                let source = manager.source(qh);
                for (mime, _) in &contents {
                    source.offer(mime);
                }
                device.set(Some(&source), is_primary);
                if !is_primary {
                    state.latest = Some(source.clone());
                }
                state.served.push(Served {
                    source,
                    contents,
                    primary: is_primary,
                    reads: reads.clone(),
                });
            }
            let _ = reply.send(rx);
        }
        Request::Save { primary, reply } => {
            let offer = if primary { &state.primary } else { &state.selection };
            let pipes = offer.as_ref().map(|offer| {
                let mimes = state.offers.get(offer).cloned().unwrap_or_default();
                plan_reads(&mimes)
                    .into_iter()
                    .filter_map(|(read, serve_as)| {
                        let (r, w) = nix::unistd::pipe2(OFlag::O_CLOEXEC).ok()?;
                        offer.receive(&read, w.as_fd());
                        Some((serve_as, r))
                    })
                    .collect()
            });
            let _ = reply.send(pipes);
        }
        Request::StillOurs(reply) => {
            let ours = state
                .latest
                .as_ref()
                .is_some_and(|l| state.served.iter().any(|s| &s.source == l && !s.primary));
            let _ = reply.send(ours);
        }
    }
}

// ---- Wayland event plumbing -------------------------------------------------

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZwlrDataControlManagerV1, _: <ZwlrDataControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ExtDataControlManagerV1, _: <ExtDataControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

/// The two protocols are identical apart from their names.
macro_rules! data_control_dispatch {
    ($variant:ident, $device:ty, $device_mod:ident, $offer:ty, $offer_mod:ident, $source:ty, $source_mod:ident) => {
        impl Dispatch<$device, ()> for State {
            fn event(state: &mut Self, _: &$device, event: $device_mod::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                match event {
                    $device_mod::Event::DataOffer { id } => state.on_offer_created(Offer::$variant(id)),
                    $device_mod::Event::Selection { id } => state.on_selection(id.map(Offer::$variant), false),
                    $device_mod::Event::PrimarySelection { id } => state.on_selection(id.map(Offer::$variant), true),
                    $device_mod::Event::Finished => state.device_gone = true,
                    _ => {}
                }
            }

            event_created_child!(State, $device, [
                $device_mod::EVT_DATA_OFFER_OPCODE => ($offer, ()),
            ]);
        }

        impl Dispatch<$offer, ()> for State {
            fn event(state: &mut Self, offer: &$offer, event: $offer_mod::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                if let $offer_mod::Event::Offer { mime_type } = event {
                    state.offers.entry(Offer::$variant(offer.clone())).or_default().push(mime_type);
                }
            }
        }

        impl Dispatch<$source, ()> for State {
            fn event(state: &mut Self, source: &$source, event: $source_mod::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
                let source = Source::$variant(source.clone());
                match event {
                    $source_mod::Event::Send { mime_type, fd } => state.on_send(&source, mime_type, fd),
                    $source_mod::Event::Cancelled => state.on_cancelled(&source),
                    _ => {}
                }
            }
        }
    };
}

data_control_dispatch!(Wlr, ZwlrDataControlDeviceV1, wlr_device, ZwlrDataControlOfferV1, wlr_offer, ZwlrDataControlSourceV1, wlr_source);
data_control_dispatch!(Ext, ExtDataControlDeviceV1, ext_device, ExtDataControlOfferV1, ext_offer, ExtDataControlSourceV1, ext_source);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_one_read_per_kind_of_content() {
        let mimes: Vec<String> = ["text/plain", "UTF8_STRING", "text/html", "image/bmp", "image/png", "TARGETS"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let plan = plan_reads(&mimes);
        assert_eq!(plan[0], ("UTF8_STRING".into(), vec!["text/plain".into(), "UTF8_STRING".into()]));
        assert_eq!(plan[1], ("image/png".into(), vec!["image/png".into()]));
        assert_eq!(plan[2], ("text/html".into(), vec!["text/html".into()]));
        assert_eq!(plan.len(), 3);
    }
}
