//! Counters for experiments with the operating point, and `--stats-json`:
//! one JSON object per second (JSON lines) describing the link, the loss
//! recovery and what the viewer saw.
//!
//! Fields of each line (counts are for the interval since the previous
//! line, `dt` seconds long, about 1; rates are per second):
//!
//! * `t`: Unix time in seconds at the end of the interval, to line up with
//!   `tc netem` changes. `session_t`: seconds since this session started (a
//!   reconnect starts a new session and a new count).
//! * `op`: the operating point in force at the end of the interval:
//!   `render_scale`, `fps` (what was offered; 60 or 30), `recovery`
//!   (fir, pli or fir+pli), `hold` (the mode: auto, on, off), `holding`
//!   (whether access units with gaps are held right now; auto can switch
//!   it off), `ltr` (LTR offered and acked), `after_loss` (idr: drop
//!   pictures until the next keyframe; conceal: keep decoding), `decoder`
//!   (the video decoder backend in use: vaapi, cuda or software; null
//!   before the decoder opened).
//! * What the host settled on: `canvas` (`[width, height]` of the encoded
//!   canvas, from its answer or the latest display layout; null before
//!   either), `answer_ltrp` (ltrpEnabled in the host's latest answer; null
//!   if it had none), `max_tile_fps` (the highest of the tiles' `fps`).
//! * `video_pps`, `video_bytes_ps`: authenticated video SRTP packets and
//!   their size as received, every SSRC (including ones not adopted). The
//!   size is the UDP payload: RTP header, encrypted payload and SRTP auth
//!   tag, without IP and UDP headers. Undecryptable packets are left out.
//! * `tiles`: per adopted tile, `loss_pct` (packets missing over packets
//!   expected in the interval, from the RFC 3550 receiver statistics; a
//!   retransmission or late packet that fills a hole counts as received,
//!   so this is the loss left after repair), `loss_pre_pct` (sequence
//!   number holes as first seen over packets expected: the loss before any
//!   repair; a reordered packet counts as a hole too), `jitter_ms` (RFC
//!   3550 interarrival jitter at the end) and `fps` (distinct RTP
//!   timestamps per second: the host's pictures for that tile of which at
//!   least one packet arrived, whole or not).
//! * `nacks`: packets NACKed for the first time; `nack_retries`: packets
//!   NACKed a second time; `nacked_arrived`: NACKed packets that arrived
//!   within 1 s of their first NACK (retransmissions, or late originals),
//!   whether or not a hold was still waiting for them. Zero here with
//!   `nacks` > 0 means the host isn't retransmitting.
//!   `nack_arrival_p50_ms`, `nack_arrival_max_ms`: time from the first NACK
//!   to the arrival, over those packets; null when none arrived.
//! * `holds_started`, `holds_filled`, `holds_given_up`: access units held
//!   for missing packets, and how those holds ended.
//! * Keyframe requests sent, one per tile request. By kind, for the ones
//!   caused by loss only: `kf_loss_fir`, `kf_loss_pli`, `kf_loss_fir_pli`.
//!   By cause, all of them: `kf_gap` (an access unit came out incomplete),
//!   `kf_decode` (the decoder rejected one), `kf_damaged` (a picture decoded
//!   with missing references) are the loss causes; `kf_adoption` (a new
//!   SSRC group), `kf_reoffer` (resize or operating point change),
//!   `kf_stall` (packets but no pictures; repeated every 1.5 s),
//!   `kf_hw_fallback` (switched decoder backend) and `kf_other` (a keyframe
//!   without parameter sets, or `keyframe` on the control FIFO) are not.
//!   `kf_suppressed`: tile requests not sent because the same tile was
//!   asked within 500 ms or the stream was idle.
//! * `irap_aus`, `irap_bytes`: complete IRAP (keyframe) access units that
//!   reached the decoder, and their total size (NAL units, no start codes).
//!   Keyframes lost in transit are not counted. `irap_follow_bytes`: the
//!   size of the first access unit on each of the other tiles after an
//!   IRAP (the host's IDR covers the base tile only; the rest follow).
//! * `recovery_n`, `recovery_p50_ms`, `recovery_max_ms`: outages caused by
//!   loss and how long they lasted. An outage starts at the first loss the
//!   decoder sees (gap, decode, damaged) while none is open, whether or not
//!   its keyframe request went out (it may be rate-limited); later losses
//!   during it add nothing. A gap is seen when the hold gives up, so the
//!   hold's wait comes before the outage: it shows in `frozen_ms`, not
//!   here. It ends with the first picture, of any
//!   tile, decoded from an IRAP fed after the loss or from what followed
//!   it: a clean P picture that may still reference the damage doesn't end
//!   it. A stall reset of the decoder keeps it open. Adopting a new SSRC
//!   group drops an open outage without a sample; `recovery_dropped`
//!   counts those.
//!   Samples belong to the interval the outage ended in. Null when none
//!   ended in the interval.
//! * `frozen_ms`: time in the interval, sampled every 10 ms, during which
//!   the picture was stuck: the decoder was dropping everything while
//!   waiting for a keyframe and video arrived in the last 1.5 s, or some
//!   tile received packets in the last 100 ms but produced no picture for
//!   100 ms. A static screen, where the host sends nothing, is not frozen.
//!   It measures the decode side; a slow renderer doesn't show here.
//!   With `after_loss` conceal the decoder doesn't wait for the keyframe,
//!   so the picture keeps moving: that time is `concealed_ms` instead.
//! * `concealed_pictures`: pictures decoded after a loss and before the
//!   next keyframe (only with `after_loss` conceal); `concealed_ms`: time
//!   in the interval, sampled like `frozen_ms` and never counted in both,
//!   during which the latest picture shown was such a picture and video
//!   arrived in the last 1.5 s.
//! * `decode_aus`, `decode_avg_ms`, `decode_worst_ms`: access units fed to
//!   the decoder and how long the decode call took.
//! * `presented_fps`: frames handed to the renderer (a frame is one tile
//!   picture, or one gathered set with `--sync-tiles`). The display may
//!   still drop some.
//! * `rtt_ms`: smoothed round trip from host reports echoing our SRs; null
//!   while unknown (the host may never echo them).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use super::{KeyframeCause, Shared};
use crate::rtp::assembler::HoldStats;
use crate::rtp::rtcp::Recovery;
use crate::rtp::stats::VIDEO_CLOCK_HZ;

/// Tiles tracked for freezes; the host uses 4.
pub(super) const MAX_TILES: usize = 16;
const LINE_EVERY: Duration = Duration::from_secs(1);
const SAMPLE_EVERY: Duration = Duration::from_millis(10);
/// A tile with traffic but no picture for this long is frozen.
const FROZEN_AFTER_MS: u64 = 100;
/// Waiting for a keyframe counts as frozen only while video flows.
const FLOWING_MS: u64 = 1500;
/// Latency samples kept between lines at most.
const MAX_SAMPLES: usize = 4096;

/// Shared counters. Monotonic ones are swapped to zero by the stats writer,
/// its only reader.
#[derive(Default)]
pub(super) struct Metrics {
    pub video_packets: AtomicU64,
    pub video_bytes: AtomicU64,
    pub nacked: AtomicU64,
    pub nack_retries: AtomicU64,
    nacked_arrived: AtomicU64,
    nack_latency: Mutex<Vec<Duration>>,
    pub holds_started: AtomicU64,
    pub holds_filled: AtomicU64,
    pub holds_given_up: AtomicU64,
    pub holding: AtomicBool,
    /// Loss-caused requests by kind, and all requests by cause.
    kf_loss: [AtomicU64; 3],
    kf_cause: [AtomicU64; KeyframeCause::ALL.len()],
    pub keyframes_suppressed: AtomicU64,
    pub irap_aus: AtomicU64,
    pub irap_bytes: AtomicU64,
    pub irap_follow_bytes: AtomicU64,
    pub decoded_aus: AtomicU64,
    pub decode_ns: AtomicU64,
    pub decode_worst_ns: AtomicU64,
    pub presented: AtomicU64,
    pub awaiting_idr: AtomicBool,
    concealed_pictures: AtomicU64,
    recovery_dropped: AtomicU64,
    /// Backend name of the video decoder in use.
    decoder: Mutex<&'static str>,
    /// The latest picture published was concealed.
    concealing: AtomicBool,
    /// ltrpEnabled in the latest answer: 0 unknown, 1 off, 2 on.
    answer_ltrp: AtomicU8,
    /// Session clock (ms) of each tile's latest packet and picture.
    tile_packet_ms: [AtomicU64; MAX_TILES],
    tile_picture_ms: [AtomicU64; MAX_TILES],
    /// Per tile: new RTP timestamps, and sequence number holes.
    tile_frames: [AtomicU64; MAX_TILES],
    tile_holes: [AtomicU64; MAX_TILES],
    recovery: Mutex<Outages>,
}

fn kind_index(k: Recovery) -> usize {
    match k {
        Recovery::Fir => 0,
        Recovery::Pli => 1,
        Recovery::FirPli => 2,
    }
}

impl Metrics {
    pub fn add_holds(&self, h: HoldStats) {
        self.holds_started.fetch_add(h.started as u64, Ordering::Relaxed);
        self.holds_filled.fetch_add(h.filled as u64, Ordering::Relaxed);
        self.holds_given_up.fetch_add(h.given_up as u64, Ordering::Relaxed);
    }

    /// A keyframe request went out for one tile.
    pub fn keyframe_sent(&self, kind: Recovery, cause: KeyframeCause) {
        if cause.is_loss() {
            self.kf_loss[kind_index(kind)].fetch_add(1, Ordering::Relaxed);
        }
        self.kf_cause[cause as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// A loss was found in IRAP epoch `epoch` and a keyframe asked for.
    pub fn outage(&self, epoch: u64, at: Instant) {
        self.recovery.lock().start(epoch, at);
    }

    /// The stream was replaced; an open outage won't be repaired.
    pub fn end_outage(&self) {
        if self.recovery.lock().open.take().is_some() {
            self.recovery_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn decoder(&self) -> &'static str {
        *self.decoder.lock()
    }

    pub fn set_decoder(&self, name: &'static str) {
        *self.decoder.lock() = name;
    }

    /// A NACKed packet arrived, `after` its first NACK.
    pub fn nack_arrived(&self, after: Duration) {
        self.nacked_arrived.fetch_add(1, Ordering::Relaxed);
        let mut l = self.nack_latency.lock();
        if l.len() < MAX_SAMPLES {
            l.push(after);
        }
    }

    pub fn set_answer_ltrp(&self, ltrp: Option<bool>) {
        let v = match ltrp {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        };
        self.answer_ltrp.store(v, Ordering::Relaxed);
    }

    fn answer_ltrp(&self) -> Option<bool> {
        match self.answer_ltrp.load(Ordering::Relaxed) {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        }
    }

    pub fn decoded(&self, took: Duration) {
        let ns = took.as_nanos() as u64;
        self.decoded_aus.fetch_add(1, Ordering::Relaxed);
        self.decode_ns.fetch_add(ns, Ordering::Relaxed);
        self.decode_worst_ns.fetch_max(ns, Ordering::Relaxed);
    }

    pub fn tile_packet(&self, tile: usize, now_ms: u64) {
        if let Some(t) = self.tile_packet_ms.get(tile) {
            t.store(now_ms, Ordering::Relaxed);
        }
    }

    /// A packet with a newer RTP timestamp arrived for `tile`.
    pub fn tile_frame(&self, tile: usize) {
        if let Some(t) = self.tile_frames.get(tile) {
            t.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `n` sequence numbers were skipped on `tile`.
    pub fn tile_holes(&self, tile: usize, n: u64) {
        if let Some(t) = self.tile_holes.get(tile)
            && n > 0
        {
            t.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// New RTP timestamps and holes on `tile` since the last call.
    fn take_tile(&self, tile: usize) -> (u64, u64) {
        (self.tile_frames[tile].swap(0, Ordering::Relaxed), self.tile_holes[tile].swap(0, Ordering::Relaxed))
    }

    /// A picture for `tile` from IRAP epoch `epoch` was published.
    pub fn picture(&self, tile: usize, epoch: u64, now_ms: u64, at: Instant) {
        if let Some(t) = self.tile_picture_ms.get(tile) {
            t.store(now_ms, Ordering::Relaxed);
        }
        self.recovery.lock().picture(epoch, at);
    }

    /// A picture was published; `concealed` if decoded after a loss and
    /// before the next keyframe.
    pub fn shown(&self, concealed: bool) {
        if concealed {
            self.concealed_pictures.fetch_add(1, Ordering::Relaxed);
        }
        self.concealing.store(concealed, Ordering::Relaxed);
    }

    /// Whether the picture counts as concealed at `now_ms`; only asked
    /// when it isn't frozen.
    fn concealed(&self, now_ms: u64, last_video_ms: u64) -> bool {
        last_video_ms != 0 && now_ms.saturating_sub(last_video_ms) < FLOWING_MS && self.concealing.load(Ordering::Relaxed)
    }

    /// Whether the picture counts as frozen at `now_ms` (see the module
    /// docs), given the session clock of the latest video packet.
    fn frozen(&self, now_ms: u64, last_video_ms: u64) -> bool {
        let ago = |t: u64| now_ms.saturating_sub(t);
        if last_video_ms != 0 && ago(last_video_ms) < FLOWING_MS && self.awaiting_idr.load(Ordering::Relaxed) {
            return true;
        }
        self.tile_packet_ms.iter().zip(&self.tile_picture_ms).any(|(p, q)| {
            let (p, q) = (p.load(Ordering::Relaxed), q.load(Ordering::Relaxed));
            p != 0 && ago(p) < FROZEN_AFTER_MS && ago(q) >= FROZEN_AFTER_MS
        })
    }
}

/// Outages from a loss to the first picture after the keyframe that
/// repaired it (see the module docs).
#[derive(Default)]
struct Outages {
    /// The IRAP epoch the open outage started in, and when.
    open: Option<(u64, Instant)>,
    samples: Vec<Duration>,
}

impl Outages {
    fn start(&mut self, epoch: u64, at: Instant) {
        if self.open.is_none() {
            self.open = Some((epoch, at));
        }
    }

    fn picture(&mut self, epoch: u64, at: Instant) {
        if let Some((started, t)) = self.open
            && epoch > started
        {
            self.open = None;
            if self.samples.len() < MAX_SAMPLES {
                self.samples.push(at.saturating_duration_since(t));
            }
        }
    }
}

/// Count, median and max of `samples`, emptying it.
fn summary(samples: &mut Vec<Duration>) -> (usize, Option<(Duration, Duration)>) {
    let mut s = std::mem::take(samples);
    s.sort_unstable();
    (s.len(), s.last().map(|&max| (s[s.len() / 2], max)))
}

/// Formats a float for JSON: finite, at most `digits` decimals.
fn num(v: f64, digits: usize) -> String {
    if v.is_finite() { format!("{v:.digits$}") } else { "null".into() }
}

/// Packets expected over the interval, from the expected count now and at
/// its start.
fn expected_since(now: u64, start: Option<u64>) -> u64 {
    // Stats restart with a new SSRC group; count from zero then.
    now - start.filter(|&e| e <= now).unwrap_or(0)
}

/// Per-tile loss over the interval, from `(expected, received)` now and at
/// its start.
fn loss_pct(now: (u64, u64), start: Option<(u64, u64)>) -> f64 {
    let r0 = start.filter(|&(e, _)| e <= now.0).map_or(0, |s| s.1);
    let exp = expected_since(now.0, start.map(|s| s.0));
    let rcv = now.1.saturating_sub(r0);
    if exp == 0 { 0.0 } else { exp.saturating_sub(rcv) as f64 * 100.0 / exp as f64 }
}

/// One tile's part of a line.
#[derive(Clone, Copy, Default)]
struct TileLine {
    loss_pct: f64,
    loss_pre_pct: f64,
    jitter_ms: f64,
    fps: f64,
}

/// The values of one line that don't come from [`Metrics`] counters.
struct LineContext<'a> {
    t: f64,
    session_t: f64,
    dt: f64,
    op: &'a super::OperatingPoint,
    canvas: Option<(u32, u32)>,
    tiles: &'a [TileLine],
    frozen_ms: u64,
    concealed_ms: u64,
    rtt: Option<Duration>,
}

fn line(m: &Metrics, cx: &LineContext) -> String {
    let take = |c: &AtomicU64| c.swap(0, Ordering::Relaxed);
    let rate = |n: u64| num(n as f64 / cx.dt, 1);
    let ms = |d: Duration| num(d.as_secs_f64() * 1000.0, 1);
    let op = cx.op;
    let mut s = String::with_capacity(1024);
    let _ = write!(
        s,
        "{{\"t\":{},\"session_t\":{},\"dt\":{},\"op\":{{\"render_scale\":{},\"fps\":{},\"recovery\":\"{}\",\
         \"hold\":\"{}\",\"holding\":{},\"ltr\":{},\"after_loss\":\"{}\",\"decoder\":{}}}",
        num(cx.t, 3),
        num(cx.session_t, 3),
        num(cx.dt, 3),
        op.render_scale,
        op.fps.value(),
        op.recovery.name(),
        op.hold.name(),
        m.holding.load(Ordering::Relaxed),
        op.ltr,
        op.after_loss.name(),
        match *m.decoder.lock() {
            "" => "null".to_string(),
            name => format!("\"{name}\""),
        },
    );
    match cx.canvas {
        Some((w, h)) => {
            let _ = write!(s, ",\"canvas\":[{w},{h}]");
        }
        None => s.push_str(",\"canvas\":null"),
    }
    match m.answer_ltrp() {
        Some(v) => {
            let _ = write!(s, ",\"answer_ltrp\":{v}");
        }
        None => s.push_str(",\"answer_ltrp\":null"),
    }
    let max_fps = cx.tiles.iter().map(|t| t.fps).fold(None, |a: Option<f64>, f| Some(a.map_or(f, |a| a.max(f))));
    let _ = write!(
        s,
        ",\"max_tile_fps\":{},\"video_pps\":{},\"video_bytes_ps\":{}",
        max_fps.map_or("null".into(), |f| num(f, 1)),
        rate(take(&m.video_packets)),
        rate(take(&m.video_bytes)),
    );
    s.push_str(",\"tiles\":[");
    for (i, t) in cx.tiles.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"loss_pct\":{},\"loss_pre_pct\":{},\"jitter_ms\":{},\"fps\":{}}}",
            num(t.loss_pct, 2),
            num(t.loss_pre_pct, 2),
            num(t.jitter_ms, 2),
            num(t.fps, 1),
        );
    }
    s.push(']');
    let _ = write!(s, ",\"nacks\":{},\"nack_retries\":{},\"nacked_arrived\":{}", take(&m.nacked), take(&m.nack_retries), take(&m.nacked_arrived));
    match summary(&mut m.nack_latency.lock()) {
        (_, Some((p50, max))) => {
            let _ = write!(s, ",\"nack_arrival_p50_ms\":{},\"nack_arrival_max_ms\":{}", ms(p50), ms(max));
        }
        (_, None) => s.push_str(",\"nack_arrival_p50_ms\":null,\"nack_arrival_max_ms\":null"),
    }
    let _ = write!(
        s,
        ",\"holds_started\":{},\"holds_filled\":{},\"holds_given_up\":{}",
        take(&m.holds_started),
        take(&m.holds_filled),
        take(&m.holds_given_up),
    );
    let [fir, pli, both] = m.kf_loss.each_ref().map(take);
    let _ = write!(s, ",\"kf_loss_fir\":{fir},\"kf_loss_pli\":{pli},\"kf_loss_fir_pli\":{both}");
    for (cause, n) in KeyframeCause::ALL.iter().zip(&m.kf_cause) {
        let _ = write!(s, ",\"kf_{}\":{}", cause.name(), take(n));
    }
    let _ = write!(
        s,
        ",\"kf_suppressed\":{},\"irap_aus\":{},\"irap_bytes\":{},\"irap_follow_bytes\":{}",
        take(&m.keyframes_suppressed),
        take(&m.irap_aus),
        take(&m.irap_bytes),
        take(&m.irap_follow_bytes),
    );
    match summary(&mut m.recovery.lock().samples) {
        (n, Some((p50, max))) => {
            let _ = write!(s, ",\"recovery_n\":{n},\"recovery_p50_ms\":{},\"recovery_max_ms\":{}", ms(p50), ms(max));
        }
        (_, None) => s.push_str(",\"recovery_n\":0,\"recovery_p50_ms\":null,\"recovery_max_ms\":null"),
    }
    let _ = write!(s, ",\"recovery_dropped\":{}", take(&m.recovery_dropped));
    let aus = take(&m.decoded_aus);
    let busy = Duration::from_nanos(take(&m.decode_ns));
    let worst = Duration::from_nanos(take(&m.decode_worst_ns));
    let avg = if aus == 0 { "null".into() } else { ms(busy / aus as u32) };
    let _ = write!(
        s,
        ",\"frozen_ms\":{},\"concealed_ms\":{},\"concealed_pictures\":{},\"decode_aus\":{aus},\"decode_avg_ms\":{avg},\"decode_worst_ms\":{},\"presented_fps\":{}",
        cx.frozen_ms,
        cx.concealed_ms,
        take(&m.concealed_pictures),
        if aus == 0 { "null".into() } else { ms(worst) },
        rate(take(&m.presented)),
    );
    match cx.rtt {
        Some(r) => {
            let _ = write!(s, ",\"rtt_ms\":{}}}", ms(r));
        }
        None => s.push_str(",\"rtt_ms\":null}"),
    }
    s
}

/// Writes a stats line per second to `path` (appending) until the session
/// stops.
pub(super) fn writer(shared: Arc<Shared>, path: PathBuf) {
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path);
    let mut out = match file {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            tracing::warn!("--stats-json {}: {e}", path.display());
            return;
        }
    };
    tracing::info!("writing stats to {}", path.display());
    let m = &shared.metrics;
    // Drop what accumulated while connecting.
    let _ = line(
        m,
        &LineContext { t: 0.0, session_t: 0.0, dt: 1.0, op: &shared.op(), canvas: None, tiles: &[], frozen_ms: 0, concealed_ms: 0, rtt: None },
    );
    for i in 0..MAX_TILES {
        m.take_tile(i);
    }
    let mut start: HashMap<u32, (u64, u64)> = HashMap::new();
    let mut since = Instant::now();
    let (mut frozen_samples, mut concealed_samples) = (0u64, 0u64);
    while !shared.stopped() {
        std::thread::sleep(SAMPLE_EVERY);
        let now_ms = shared.clock.now_ms();
        let last_video = shared.last_video_packet.load(Ordering::Relaxed);
        if m.frozen(now_ms, last_video) {
            frozen_samples += 1;
        } else if m.concealed(now_ms, last_video) {
            concealed_samples += 1;
        }
        let dt = since.elapsed();
        if dt < LINE_EVERY {
            continue;
        }
        since = Instant::now();
        let secs = dt.as_secs_f64();
        let ssrcs = shared.tile_ssrcs.lock().clone();
        let now: HashMap<u32, (u64, u64, u32)> = {
            let stats = shared.rx_stats.lock();
            ssrcs
                .iter()
                .filter_map(|s| stats.get(s).map(|r| (*s, (r.expected(), r.received(), r.jitter()))))
                .collect()
        };
        let counts: Vec<(u64, u64)> = (0..MAX_TILES).map(|i| m.take_tile(i)).collect();
        let tiles: Vec<TileLine> = ssrcs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let (frames, holes) = counts.get(i).copied().unwrap_or_default();
                let fps = frames as f64 / secs;
                match now.get(s) {
                    Some(&(e, r, j)) => {
                        let s0 = start.get(s).copied();
                        let exp = expected_since(e, s0.map(|s| s.0));
                        TileLine {
                            loss_pct: loss_pct((e, r), s0),
                            loss_pre_pct: if exp == 0 { 0.0 } else { (holes as f64 * 100.0 / exp as f64).min(100.0) },
                            jitter_ms: j as f64 * 1000.0 / VIDEO_CLOCK_HZ as f64,
                            fps,
                        }
                    }
                    None => TileLine { fps, ..TileLine::default() },
                }
            })
            .collect();
        start = now.into_iter().map(|(s, (e, r, _))| (s, (e, r))).collect();
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        let cx = LineContext {
            t,
            session_t: shared.clock.0.elapsed().as_secs_f64(),
            dt: secs,
            op: &shared.op(),
            canvas: *shared.canvas.lock(),
            tiles: &tiles,
            frozen_ms: (frozen_samples * SAMPLE_EVERY.as_millis() as u64).min(dt.as_millis() as u64),
            concealed_ms: (concealed_samples * SAMPLE_EVERY.as_millis() as u64).min(dt.as_millis() as u64),
            rtt: *shared.rtt.lock(),
        };
        (frozen_samples, concealed_samples) = (0, 0);
        let l = line(m, &cx);
        if let Err(e) = writeln!(out, "{l}").and_then(|()| out.flush()) {
            tracing::warn!("--stats-json: {e}");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::assembler::HoldMode;
    use crate::session::{AfterLoss, Fps, OperatingPoint};

    #[test]
    fn one_sample_per_outage() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let m = Metrics::default();
        // A gap in epoch 3, asked again twice before the keyframe.
        m.outage(3, t0);
        m.outage(3, ms(500));
        m.outage(3, ms(900));
        // Pictures still in epoch 3 (other tiles, clean P pictures) don't
        // end it.
        m.picture(1, 3, 0, ms(600));
        m.picture(0, 3, 0, ms(650));
        assert_eq!(summary(&mut m.recovery.lock().samples), (0, None));
        // The first picture after the IRAP does, on any tile.
        m.picture(2, 4, 0, ms(1000));
        m.picture(0, 4, 0, ms(1100));
        let one = (1, Some((Duration::from_millis(1000), Duration::from_millis(1000))));
        assert_eq!(summary(&mut m.recovery.lock().samples), one);
        assert_eq!(summary(&mut m.recovery.lock().samples), (0, None));
    }

    #[test]
    fn damaged_picture_waits_for_a_later_irap() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let m = Metrics::default();
        // A damaged picture of epoch 5 that came out after IRAP 6 was fed:
        // the picture of epoch 6 closes it.
        m.picture(1, 5, 0, t0);
        m.outage(5, t0);
        m.picture(1, 5, 0, ms(30));
        m.picture(0, 6, 0, ms(40));
        let (n, rec) = summary(&mut m.recovery.lock().samples);
        assert_eq!((n, rec.map(|r| r.1)), (1, Some(Duration::from_millis(40))));
        // An outage cut short by a new stream gives no sample.
        m.outage(6, ms(50));
        m.end_outage();
        m.end_outage();
        m.picture(0, 7, 0, ms(90));
        assert_eq!(summary(&mut m.recovery.lock().samples), (0, None));
        assert_eq!(m.recovery_dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn keyframe_counters_by_cause() {
        let m = Metrics::default();
        m.keyframe_sent(Recovery::Pli, KeyframeCause::Gap);
        m.keyframe_sent(Recovery::Pli, KeyframeCause::Damaged);
        m.keyframe_sent(Recovery::Pli, KeyframeCause::Stall);
        m.keyframe_sent(Recovery::Fir, KeyframeCause::Adoption);
        let l = line(&m, &context(&op(), &[]));
        assert!(l.contains("\"kf_loss_fir\":0,\"kf_loss_pli\":2,\"kf_loss_fir_pli\":0,\"kf_gap\":1,\"kf_decode\":0,\
                            \"kf_damaged\":1,\"kf_adoption\":1,\"kf_reoffer\":0,\"kf_stall\":1,"));
    }

    #[test]
    fn frozen_rules() {
        let m = Metrics::default();
        // Nothing seen yet.
        assert!(!m.frozen(5000, 0));
        m.tile_packet(1, 4950);
        m.tile_picture_ms[1].store(4800, Ordering::Relaxed);
        assert!(m.frozen(5000, 4950));
        m.picture(1, 0, 4990, Instant::now());
        assert!(!m.frozen(5000, 4950));
        // Waiting for a keyframe while video flows.
        m.awaiting_idr.store(true, Ordering::Relaxed);
        assert!(m.frozen(5000, 4950));
        // Not on an idle stream.
        assert!(!m.frozen(9000, 4950));
    }

    #[test]
    fn concealed_rules() {
        let m = Metrics::default();
        m.shown(true);
        assert!(m.concealed(5000, 4950));
        // Not on an idle stream.
        assert!(!m.concealed(9000, 4950));
        // A clean picture after the keyframe ends it.
        m.shown(false);
        assert!(!m.concealed(5000, 4950));
        assert_eq!(m.concealed_pictures.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn interval_loss() {
        assert_eq!(loss_pct((200, 190), Some((100, 100))), 10.0);
        assert_eq!(loss_pct((100, 100), Some((100, 100))), 0.0);
        // A new SSRC group restarted the counts.
        assert_eq!(loss_pct((10, 9), Some((100, 100))), 10.0);
        assert_eq!(expected_since(10, Some(100)), 10);
        assert_eq!(expected_since(150, Some(100)), 50);
    }

    fn context<'a>(op: &'a OperatingPoint, tiles: &'a [TileLine]) -> LineContext<'a> {
        LineContext { t: 0.0, session_t: 0.0, dt: 1.0, op, canvas: None, tiles, frozen_ms: 0, concealed_ms: 0, rtt: None }
    }

    fn op() -> OperatingPoint {
        OperatingPoint {
            render_scale: 0.5,
            fps: Fps::Thirty,
            recovery: Recovery::Pli,
            hold: HoldMode::Off,
            ltr: false,
            after_loss: AfterLoss::Conceal,
        }
    }

    #[test]
    fn line_is_json_with_every_field() {
        let m = Metrics::default();
        m.video_packets.store(100, Ordering::Relaxed);
        m.keyframe_sent(Recovery::Pli, KeyframeCause::Gap);
        m.nack_arrived(Duration::from_millis(12));
        m.nack_arrived(Duration::from_millis(30));
        m.set_answer_ltrp(Some(false));
        m.decoded(Duration::from_millis(3));
        m.shown(true);
        m.shown(false);
        m.set_decoder("vaapi");
        let op = op();
        let tiles = [
            TileLine { loss_pct: 1.0, loss_pre_pct: 3.0, jitter_ms: 2.5, fps: 29.5 },
            TileLine { fps: 30.0, ..TileLine::default() },
        ];
        let l = line(
            &m,
            &LineContext {
                t: 1.5,
                session_t: 2.0,
                dt: 2.0,
                op: &op,
                canvas: Some((2560, 1440)),
                tiles: &tiles,
                frozen_ms: 30,
                concealed_ms: 40,
                rtt: None,
            },
        );
        assert_eq!(
            l,
            "{\"t\":1.500,\"session_t\":2.000,\"dt\":2.000,\"op\":{\"render_scale\":0.5,\"fps\":30,\
             \"recovery\":\"pli\",\"hold\":\"off\",\"holding\":false,\"ltr\":false,\"after_loss\":\"conceal\",\"decoder\":\"vaapi\"},\"canvas\":[2560,1440],\
             \"answer_ltrp\":false,\"max_tile_fps\":30.0,\"video_pps\":50.0,\"video_bytes_ps\":0.0,\
             \"tiles\":[{\"loss_pct\":1.00,\"loss_pre_pct\":3.00,\"jitter_ms\":2.50,\"fps\":29.5},\
             {\"loss_pct\":0.00,\"loss_pre_pct\":0.00,\"jitter_ms\":0.00,\"fps\":30.0}],\"nacks\":0,\
             \"nack_retries\":0,\"nacked_arrived\":2,\"nack_arrival_p50_ms\":30.0,\"nack_arrival_max_ms\":30.0,\
             \"holds_started\":0,\"holds_filled\":0,\"holds_given_up\":0,\"kf_loss_fir\":0,\"kf_loss_pli\":1,\
             \"kf_loss_fir_pli\":0,\"kf_gap\":1,\"kf_decode\":0,\"kf_damaged\":0,\"kf_adoption\":0,\
             \"kf_reoffer\":0,\"kf_stall\":0,\"kf_hw_fallback\":0,\"kf_other\":0,\"kf_suppressed\":0,\
             \"irap_aus\":0,\"irap_bytes\":0,\"irap_follow_bytes\":0,\"recovery_n\":0,\
             \"recovery_p50_ms\":null,\"recovery_max_ms\":null,\"recovery_dropped\":0,\"frozen_ms\":30,\"concealed_ms\":40,\"concealed_pictures\":1,\"decode_aus\":1,\
             \"decode_avg_ms\":3.0,\"decode_worst_ms\":3.0,\"presented_fps\":0.0,\"rtt_ms\":null}"
        );
        // Counters were consumed.
        let again = line(&m, &context(&op, &[]));
        assert!(again.contains("\"video_pps\":0.0"));
        assert!(again.contains("\"canvas\":null,\"answer_ltrp\":false,\"max_tile_fps\":null"));
        assert!(again.contains("\"nack_arrival_p50_ms\":null"));
    }

    #[test]
    fn tile_counters() {
        let m = Metrics::default();
        m.tile_frame(2);
        m.tile_frame(2);
        m.tile_holes(2, 3);
        m.tile_holes(2, 0);
        // Out of range is ignored.
        m.tile_frame(MAX_TILES);
        m.tile_holes(MAX_TILES, 1);
        assert_eq!(m.take_tile(2), (2, 3));
        assert_eq!(m.take_tile(2), (0, 0));
    }
}
