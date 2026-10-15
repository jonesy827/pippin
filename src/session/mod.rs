//! A live screen-sharing session: owns the control connection and the media
//! threads, and exposes input/resize/clipboard entry points to the UI.
//!
//! Threads:
//! * `control` connects, then reads the encrypted TCP control channel
//!   (cursor shapes, display layout, clipboard, media answers).
//! * `video-rx` receives SRTP video, assembles access units, maps SSRCs to
//!   tiles and hands work to `decoder`.
//! * `decoder` runs libavcodec and publishes pictures to the [`FrameStore`].
//! * `ctrl-rx` drains the audio/RTCP UDP port: decodes the Mac's sound for
//!   the [`audio::Player`](crate::audio::Player) and records sender reports.
//! * `feedback` sends RTCP (receiver reports, keyframe requests, NACKs, LTR
//!   acks), the audio keepalive and the cursor poll on a 500 ms tick.
//! * `clipboard` mirrors text between the local and remote pasteboards.
//! * `control-fifo` (debug, `PIPPIN_CONTROL_FIFO`) switches the operating
//!   point mid-session; `stats-json` (`--stats-json`) writes metrics.

mod control;
mod media;
mod metrics;
mod operating;

use std::collections::HashMap;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

use crate::proto::auth::AuthError;
use crate::proto::handshake::{self, ConnectParams, SessionMode};
use crate::proto::messages::{self, DisplayRequest};
use crate::proto::offers::{MediaOffer, OfferOptions};
use crate::proto::record::RecordSealer;
use crate::rtp::stats::ReceiverStats;
use crate::video::decoder::{DecoderKind, Picture};

pub use media::{Feedback, KeyframeCause};
pub use operating::{AfterLoss, Fps, OperatingPoint, SharedOp};

use crate::errors::Failure;

/// How long to wait for the first video packet before giving up.
const FIRST_VIDEO_TIMEOUT: Duration = Duration::from_secs(15);

/// How long shutdown waits for a session that is still connecting before
/// leaving it to finish in the background.
const DETACH_AFTER: Duration = Duration::from_millis(250);

/// How long sent control data may go unacknowledged before the connection
/// counts as dead.
const CONTROL_ACK_TIMEOUT: Duration = Duration::from_secs(25);

/// Connection settings.
#[derive(Clone)]
pub struct Config {
    pub host: Ipv4Addr,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub session_mode: SessionMode,
    pub decoder: DecoderKind,
    /// DRM render node for VAAPI; None lets FFmpeg pick.
    pub vaapi_device: Option<String>,
    /// Software decoder threads; see `DecoderOptions`.
    pub decode_threads: u32,
    /// Present a host frame's tiles together (see `--sync-tiles`).
    pub sync_tiles: bool,
    pub warmup: bool,
    /// Told whether the host accepted the password; not called when the
    /// connection fails for another reason.
    pub on_auth: Option<AuthHook>,
    /// This session replaces one that dropped. Missing video then is more
    /// likely a host still waking up than a firewall, so it's retried
    /// rather than reported as fatal.
    pub resumed: bool,
    /// The operating point, shared with the window and kept across
    /// reconnects.
    pub op: SharedOp,
    /// Append one JSON line of metrics per second here.
    pub stats_json: Option<PathBuf>,
    /// Debug FIFO to read operating-point commands from.
    pub control_fifo: Option<PathBuf>,
    /// Plays the Mac's sound; `None` asks the host not to send any.
    pub audio: Option<crate::audio::Player>,
}

pub type AuthHook = Arc<dyn Fn(bool) + Send + Sync>;

/// A cursor shape from the host. `rgba` is `None` when the host referenced
/// a cache entry we never received; the UI then shows its default cursor.
/// Cache hits carry the size and hotspot remembered from the original
/// image, since the host sends them as 0×0.
#[derive(Clone)]
pub struct CursorShape {
    pub cache_id: u32,
    pub width: u16,
    pub height: u16,
    pub hotspot: (u16, u16),
    pub rgba: Option<Arc<Vec<u8>>>,
    /// The image arrived in this message rather than from the cache, so any
    /// cursor the UI built for `cache_id` is stale.
    pub fresh: bool,
}

pub enum Event {
    Connected,
    /// New decoded pictures are waiting in the [`FrameStore`].
    Frame,
    /// Size of the encoded canvas in pixels.
    Canvas { width: u32, height: u32 },
    Cursor(CursorShape),
    /// The session ended for good: bad credentials, a broken decoder, or a
    /// failure while first connecting.
    Closed(Option<String>),
    /// The connection failed or dropped in a way a fresh session may fix
    /// (host asleep, network change, timeout).
    Lost(String),
    /// The operating point's render scale changed; request the display
    /// again.
    Reconfigure,
}

pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, event: Event);
}

/// Latest decoded picture per tile, handed from the decoder to the renderer.
#[derive(Default)]
pub struct FrameStore {
    slots: Mutex<Vec<Option<Picture>>>,
    notified: AtomicBool,
}

impl FrameStore {
    /// Publishes one host frame's pictures together, so the renderer never
    /// shows some tiles from one frame and some from the previous (tearing).
    fn put_frame(&self, pics: impl IntoIterator<Item = (usize, Picture)>) -> bool {
        let mut slots = self.slots.lock();
        for (tile, pic) in pics {
            if slots.len() <= tile {
                slots.resize_with(tile + 1, || None);
            }
            slots[tile] = Some(pic);
        }
        !self.notified.swap(true, Ordering::AcqRel)
    }

    /// Takes every pending picture as `(tile, picture)`.
    pub fn take(&self) -> Vec<(usize, Picture)> {
        self.notified.store(false, Ordering::Release);
        let mut slots = self.slots.lock();
        slots.iter_mut().enumerate().filter_map(|(i, s)| s.take().map(|p| (i, p))).collect()
    }
}

/// The encrypted control-channel writer. The lock keeps record sequence
/// numbers in wire order across threads.
pub struct Control {
    writer: Mutex<(TcpStream, RecordSealer)>,
    record_key: [u8; 16],
    /// Our video SSRC, the one our SRs are sent as.
    video_ssrc: u32,
    /// The media offer, re-sent after a resize and changed by the
    /// operating point.
    offer: Mutex<MediaOffer>,
}

impl Control {
    pub fn send(&self, msg: &[u8]) {
        let mut w = self.writer.lock();
        let rec = w.1.seal(msg);
        if let Err(e) = w.0.write_all(&rec) {
            tracing::debug!("control send failed: {e}");
        }
    }

    pub fn reoffer(&self) {
        let msg = self.offer.lock().message();
        self.send(&msg);
    }

    /// Changes the offer the next [`reoffer`](Self::reoffer) sends.
    pub fn set_offer_options(&self, opts: OfferOptions) {
        self.offer.lock().set_options(opts);
    }
}

/// Monotonic millisecond clock shared between threads.
struct Clock(Instant);

impl Clock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis() as u64
    }
}

/// State shared by the session threads.
pub struct Shared {
    clock: Clock,
    stop: AtomicBool,
    control: OnceLock<Arc<Control>>,
    sink: Arc<dyn EventSink>,
    pub(crate) frames: Arc<FrameStore>,
    /// Tile count from the latest media answer.
    tiles: AtomicU32,
    last_video_packet: AtomicU64,
    last_frame: AtomicU64,
    /// When the media session was last re-offered (resize).
    renegotiated: AtomicU64,
    tile_ssrcs: Mutex<Vec<u32>>,
    /// RTCP receiver statistics per tile SSRC, for receiver reports.
    rx_stats: Mutex<HashMap<u32, ReceiverStats>>,
    sender_reports: Mutex<HashMap<u32, (u32, Instant)>>,
    /// Smoothed round trip from host report blocks echoing our SRs; `None`
    /// until the host sends one.
    rtt: Mutex<Option<Duration>>,
    /// Pictures decoded damaged from access units a hold had recovered,
    /// since the video receiver last logged it.
    recovered_damaged: AtomicU32,
    canvas: Mutex<Option<(u32, u32)>>,
    /// The control connection while the handshake runs, for shutdown().
    connecting: Mutex<Option<TcpStream>>,
    /// Clones of the media sockets, so shutdown() can wake their readers.
    udp: Mutex<Vec<UdpSocket>>,
    op: SharedOp,
    metrics: metrics::Metrics,
}

impl Shared {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn since_ms(&self, stamp: &AtomicU64) -> u64 {
        self.clock.now_ms().saturating_sub(stamp.load(Ordering::Relaxed))
    }

    fn touch(&self, stamp: &AtomicU64) {
        stamp.store(self.clock.now_ms(), Ordering::Relaxed);
    }

    /// The current operating point.
    fn op(&self) -> OperatingPoint {
        *self.op.lock()
    }

    fn op_handle(&self) -> &SharedOp {
        &self.op
    }

    fn set_canvas(&self, w: u32, h: u32) {
        let mut c = self.canvas.lock();
        if *c != Some((w, h)) {
            *c = Some((w, h));
            self.sink.emit(Event::Canvas { width: w, height: h });
        }
    }
}

/// Handle used by the UI. Cheap to clone.
#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
    threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Session {
    /// Starts connecting in the background. Progress and failures arrive as
    /// [`Event`]s.
    pub fn start(cfg: Config, display: DisplayRequest, sink: Arc<dyn EventSink>) -> Self {
        let shared = Arc::new(Shared {
            clock: Clock(Instant::now()),
            stop: AtomicBool::new(false),
            control: OnceLock::new(),
            sink,
            frames: Arc::new(FrameStore::default()),
            tiles: AtomicU32::new(crate::proto::offers::TILES_PER_FRAME as u32),
            last_video_packet: AtomicU64::new(0),
            last_frame: AtomicU64::new(0),
            renegotiated: AtomicU64::new(0),
            tile_ssrcs: Mutex::new(Vec::new()),
            rx_stats: Mutex::new(HashMap::new()),
            sender_reports: Mutex::new(HashMap::new()),
            rtt: Mutex::new(None),
            recovered_damaged: AtomicU32::new(0),
            canvas: Mutex::new(None),
            connecting: Mutex::new(None),
            udp: Mutex::new(Vec::new()),
            op: cfg.op.clone(),
            metrics: metrics::Metrics::default(),
        });
        let threads = Arc::new(Mutex::new(Vec::new()));
        let session = Session { shared: shared.clone(), threads: threads.clone() };
        let main = thread::Builder::new()
            .name("control".into())
            .spawn(move || {
                let host = cfg.host;
                // A session shut down while still connecting may hold the
                // UDP ports until its handshake gives up; wait for it.
                join_detached();
                let result = run(cfg, display, shared.clone(), &threads);
                if !shared.stopped() {
                    // The reader only returns Ok once stopped; treat anything
                    // else like a clean close by the host.
                    let e = result.err().unwrap_or_else(|| Failure::HostClosed.into());
                    tracing::debug!("session ended: {e:?}");
                    let err = crate::errors::classify(&e, host);
                    let msg = err.to_string();
                    shared.sink.emit(if err.retryable() { Event::Lost(msg) } else { Event::Closed(Some(msg)) });
                }
                shared.stop.store(true, Ordering::Release);
            })
            .expect("spawn control thread");
        session.threads.lock().push(main);
        session
    }

    pub fn frames(&self) -> &FrameStore {
        &self.shared.frames
    }

    pub fn decoder(&self) -> &'static str {
        self.shared.metrics.decoder()
    }

    fn control(&self) -> Option<&Arc<Control>> {
        self.shared.control.get()
    }

    pub fn key(&self, down: bool, keysym: u32) {
        if let Some(c) = self.control() {
            c.send(&messages::key_event(down, keysym));
        }
    }

    /// Pointer position in canvas pixels plus the RFB button mask.
    pub fn pointer(&self, buttons: u8, x: u16, y: u16) {
        if let Some(c) = self.control() {
            c.send(&messages::encrypted_pointer_event(&c.record_key, buttons, x, y));
        }
    }

    /// Asks the host to resize its virtual display.
    pub fn resize(&self, req: DisplayRequest) {
        if let Some(c) = self.control() {
            tracing::info!(
                "requesting {}x{} @{}x (backing {:?})",
                req.width,
                req.height,
                req.scale,
                req.backing()
            );
            c.send(&messages::set_display_configuration(&req));
        }
    }

    /// Stops every session thread and waits for them, so the sockets are
    /// closed and the UDP ports free once this returns. Also needed after
    /// the session stopped itself, before a new one binds the same ports.
    ///
    /// A session still connecting can be stuck for a while (TCP connect,
    /// waiting for the console user to accept), so after aborting what it
    /// can, shutdown gives it a moment and then leaves it to exit on its own
    /// via the stop flag rather than freezing the window.
    pub fn shutdown(&self) {
        let first = !self.shared.stop.swap(true, Ordering::AcqRel);
        if first && let Some(c) = self.control() {
            // Reset rather than close so the host tears the session down
            // immediately instead of waiting out its own timeouts.
            let w = c.writer.lock();
            let _ = socket2::SockRef::from(&w.0).set_linger(Some(Duration::ZERO));
            let _ = w.0.shutdown(std::net::Shutdown::Both);
        }
        if self.control().is_none() {
            if let Some(s) = self.shared.connecting.lock().take() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            self.close_udp();
            let deadline = Instant::now() + DETACH_AFTER;
            while !self.threads.lock().iter().all(|t| t.is_finished()) {
                if Instant::now() >= deadline {
                    tracing::debug!("session still connecting; leaving it to stop on its own");
                    // The next session joins it before binding the ports.
                    DETACHED.lock().push(self.threads.clone());
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        join_all(&self.threads);
        self.close_udp();
    }

    /// Wakes anything blocked on the media sockets and drops our clones.
    /// The ports themselves are freed once the threads holding the other
    /// handles exit.
    fn close_udp(&self) {
        for s in self.shared.udp.lock().drain(..) {
            let _ = socket2::SockRef::from(&s).shutdown(std::net::Shutdown::Both);
        }
    }
}

type Threads = Arc<Mutex<Vec<JoinHandle<()>>>>;

/// Sessions that shutdown() left to finish on their own.
static DETACHED: Mutex<Vec<Threads>> = Mutex::new(Vec::new());

/// Waits for every detached session. Their stop flag is set, so each exits
/// within its handshake's I/O timeout.
fn join_detached() {
    let pending: Vec<_> = DETACHED.lock().drain(..).collect();
    if !pending.is_empty() {
        tracing::debug!("waiting for a previous session to finish connecting");
    }
    for threads in pending {
        join_all(&threads);
    }
}

fn join_all(threads: &Mutex<Vec<JoinHandle<()>>>) {
    // Threads can register more threads while we join (the control thread
    // spawns the media threads), so drain until none are left.
    loop {
        let batch: Vec<_> = threads.lock().drain(..).collect();
        if batch.is_empty() {
            break;
        }
        for t in batch {
            // A session detached before its control thread got going can
            // find its own handle here; that one can't be joined.
            if t.thread().id() != thread::current().id() {
                let _ = t.join();
            }
        }
    }
}

fn bind_udp(port: u16) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // No SO_REUSEADDR: UDP has no TIME_WAIT to skip, and on Linux it would
    // let a second socket share the port and silently steal the video. A
    // leftover bind should fail here instead.
    let _ = s.set_recv_buffer_size(8 << 20);
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())
        .with_context(|| format!("binding UDP port {port} (is another screen-sharing client running?)"))?;
    if let Ok(n) = s.recv_buffer_size()
        && n < 2 << 20 {
            // Only bursts after a switch to 4K have been seen to need more.
            tracing::info!(
                "UDP receive buffer is {n} bytes; for 4K, raise net.core.rmem_max \
                 (e.g. sysctl -w net.core.rmem_max=8388608) to avoid drops after resolution switches"
            );
        }
    let s: UdpSocket = s.into();
    s.set_read_timeout(Some(Duration::from_millis(100)))?;
    Ok(s)
}

fn spawn(threads: &Mutex<Vec<JoinHandle<()>>>, name: &str, f: impl FnOnce() + Send + 'static) {
    let t = thread::Builder::new().name(name.into()).spawn(f).expect("spawn thread");
    threads.lock().push(t);
}

fn run(cfg: Config, display: DisplayRequest, shared: Arc<Shared>, threads: &Mutex<Vec<JoinHandle<()>>>) -> Result<()> {
    let addr = SocketAddrV4::new(cfg.host, cfg.port);
    let ctrl_dest = SocketAddrV4::new(cfg.host, cfg.port);
    let video_dest = SocketAddrV4::new(cfg.host, cfg.port + 1);

    // The media burst starts right after the answer, so the UDP sockets
    // must exist before the handshake.
    let ctrl_sock = bind_udp(cfg.port)?;
    let video_sock = bind_udp(cfg.port + 1)?;
    shared.udp.lock().extend([ctrl_sock.try_clone()?, video_sock.try_clone()?]);
    if shared.stopped() {
        // shutdown() may have closed the list before we filled it.
        return Ok(());
    }

    if cfg.warmup
        && let Err(e) = handshake::warmup(addr) {
            tracing::warn!("warmup connection failed: {e:#}");
        }
    if shared.stopped() {
        return Ok(());
    }

    // Keep the local firewall's return path open for the host's media while
    // negotiating; the host may start streaming at any point.
    let punching = Arc::new(AtomicBool::new(true));
    let punch = {
        let (c, v, flag) = (ctrl_sock.try_clone()?, video_sock.try_clone()?, punching.clone());
        thread::spawn(move || {
            while flag.load(Ordering::Relaxed) {
                let _ = c.send_to(&[0], ctrl_dest);
                let _ = v.send_to(&[0], video_dest);
                thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let params = ConnectParams {
        addr,
        user: cfg.user.clone(),
        password: cfg.password.clone(),
        session_mode: cfg.session_mode,
        display,
        audio: cfg.audio.is_some(),
        offer: shared.op().offer_options(),
        on_open: Some(Box::new({
            let shared = shared.clone();
            move |s: &TcpStream| {
                *shared.connecting.lock() = s.try_clone().ok();
                // Closed while we were connecting: abort right away.
                if shared.stopped() {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
            }
        })),
        stopped: Some(Box::new({
            let shared = shared.clone();
            move || shared.stopped()
        })),
    };
    let negotiated = handshake::connect(&params);
    punching.store(false, Ordering::Relaxed);
    let _ = punch.join();
    // A stopped session's verdict is stale: the caller has moved on.
    if let Some(hook) = cfg.on_auth.as_ref().filter(|_| !shared.stopped()) {
        match &negotiated {
            Ok(_) => hook(true),
            Err(e) => {
                if let Some(AuthError::Rejected(code)) = e.downcast_ref::<AuthError>() {
                    tracing::info!("host rejected the credentials (code {code})");
                    hook(false);
                }
            }
        }
    }
    *shared.connecting.lock() = None;
    let neg = negotiated?;
    if shared.stopped() {
        return Ok(());
    }
    tracing::debug!("server framebuffer {}x{}", neg.server_size.0, neg.server_size.1);

    if let Some(c) = neg.canvas {
        if c.tiles > 0 {
            shared.tiles.store(c.tiles, Ordering::Relaxed);
        }
        shared.set_canvas(c.width, c.height);
        shared.metrics.set_answer_ltrp(c.ltrp);
    }
    // We write to the host every tick (cursor poll), so a host that went to
    // sleep or a vanished network shows up as unacknowledged data; give up
    // after this long instead of the kernel's ~15 minutes of retransmits.
    let _ = socket2::SockRef::from(&neg.stream).set_tcp_user_timeout(Some(CONTROL_ACK_TIMEOUT));
    let mut reader = neg.stream.try_clone()?;
    reader.set_read_timeout(None)?;
    let control = Arc::new(Control {
        writer: Mutex::new((neg.stream, neg.sealer)),
        record_key: neg.record_key,
        video_ssrc: neg.offer.video_ssrc,
        offer: Mutex::new(neg.offer.clone()),
    });
    let _ = shared.control.set(control.clone());
    // shutdown() may have run between the check above and the set, seen no
    // control connection, and left us to finish on our own.
    if shared.stopped() {
        let _ = control.writer.lock().0.shutdown(std::net::Shutdown::Both);
        return Ok(());
    }

    let (fb_tx, fb_rx): (Sender<Feedback>, Receiver<Feedback>) = crossbeam_channel::unbounded();
    let (dec_tx, dec_rx) = crossbeam_channel::bounded(256);
    let (clip_tx, clip_rx) = crossbeam_channel::unbounded();

    {
        let (shared, fb, sock) = (shared.clone(), fb_tx.clone(), video_sock.try_clone()?);
        let (key, dec_tx) = (neg.offer.video_key_s, dec_tx.clone());
        spawn(threads, "video-rx", move || media::video_rx(shared, sock, key, dec_tx, fb));
    }
    {
        let (shared, fb) = (shared.clone(), fb_tx.clone());
        let opts = crate::video::decoder::DecoderOptions {
            kind: cfg.decoder,
            threads: cfg.decode_threads,
            vaapi_device: cfg.vaapi_device.clone(),
        };
        let sync = cfg.sync_tiles;
        spawn(threads, "decoder", move || media::decoder(shared, dec_rx, fb, opts, sync));
    }
    {
        let (shared, sock) = (shared.clone(), ctrl_sock.try_clone()?);
        let key = neg.offer.video_key_s;
        let audio = cfg.audio.clone().map(|p| (p, neg.offer.audio_key_s));
        spawn(threads, "ctrl-rx", move || media::ctrl_rx(shared, sock, key, audio));
    }
    {
        let shared = shared.clone();
        let sockets = media::FeedbackSockets { ctrl: ctrl_sock, video: video_sock, ctrl_dest, video_dest };
        let offer = neg.offer.clone();
        spawn(threads, "feedback", move || media::feedback(shared, sockets, offer, fb_rx, dec_tx));
    }
    {
        let (shared, control) = (shared.clone(), control.clone());
        spawn(threads, "clipboard", move || crate::clipboard::run(shared, control, clip_rx));
    }
    if let Some(path) = cfg.control_fifo.clone() {
        let (shared, fb) = (shared.clone(), fb_tx.clone());
        spawn(threads, "control-fifo", move || operating::control_fifo(shared, path, fb));
    }
    if let Some(path) = cfg.stats_json.clone() {
        let shared = shared.clone();
        spawn(threads, "stats-json", move || metrics::writer(shared, path));
    }

    // Subscribe to pasteboard changes and prime with the current contents.
    control.send(&crate::proto::clipboard::enable_auto_pasteboard());
    control.send(&crate::proto::clipboard::request_pasteboard());
    shared.sink.emit(Event::Connected);
    {
        let (shared, host, answered, resumed) = (shared.clone(), cfg.host, neg.canvas.is_some(), cfg.resumed);
        spawn(threads, "video-watchdog", move || video_watchdog(shared, host, answered, resumed));
    }

    let mut rx = control::ControlReader::new(shared.clone(), control, clip_tx, fb_tx);
    for msg in neg.pending {
        rx.dispatch(&msg);
    }
    rx.run(&mut reader, neg.opener)
}

/// Ends the session with an explanation if no video packet arrives soon
/// after connecting. Leaves `stop` to the UI so its shutdown still runs.
fn video_watchdog(shared: Arc<Shared>, host: Ipv4Addr, answered: bool, resumed: bool) {
    let deadline = Instant::now() + FIRST_VIDEO_TIMEOUT;
    while Instant::now() < deadline {
        if shared.stopped() || shared.last_video_packet.load(Ordering::Relaxed) != 0 {
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }
    let failure = if answered { Failure::NoVideo } else { Failure::NoMediaAnswer };
    tracing::debug!("{failure} after {FIRST_VIDEO_TIMEOUT:?} (media answer: {answered})");
    let msg = crate::errors::classify(&failure.into(), host).to_string();
    shared.sink.emit(if resumed { Event::Lost(msg) } else { Event::Closed(Some(msg)) });
}

impl Shared {
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped()
    }
}
