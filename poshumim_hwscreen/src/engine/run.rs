//! `engine::run` -- the frame loop that ties `capture` -> `convert` -> `room`
//! together, driven by a `ControlMsg` channel and emitting `EngineEvent`s.
//!
//! This is *pure plumbing*. The lifecycle state machine (`logic::control::Session`
//! -- retry-once, guard, fallback) runs JS-side (Task 12); `run` here only:
//!   * connects + publishes, emits `Started`,
//!   * paces captured frames through the BGRA->I420 shader into the encoder,
//!   * emits a `Stats` sample ~1x/s,
//!   * honours `Reconfigure` (new rung: re-publish + resize the shader + re-pace)
//!     and `Stop` (close the room, emit `Ended`),
//!   * turns any `?` failure into a single `Error(msg)` event.
//!
//! ## Threading (RULING R9)
//!
//! Task 7 proved `RoomEngine::connect`'s LiveKit signalling handshake stalls
//! forever on a **current-thread** tokio runtime. So `run` builds an explicit
//! `new_multi_thread` runtime and `block_on`s the loop. `block_on` polls the
//! future on *this* (the engine) thread -- the `!Send` D3D11 COM objects inside
//! `Capture` / `Converter` never cross a thread boundary -- while LiveKit's own
//! internally-spawned networking tasks get real worker threads. The loop future
//! is `!Send`, which is fine: only `Runtime::spawn` requires `Send`, not
//! `block_on`. Capture + convert + `push_i420` are synchronous and run inline
//! between the `.await` points (connect / publish / reconfigure / stats / close).

use std::time::{Duration, Instant};

use anyhow::Result;
use livekit::webrtc::video_frame::I420Buffer;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::Receiver;

use crate::engine::capture::Capture;
use crate::engine::convert::Converter;
use crate::engine::room::RoomEngine;
use crate::logic::control::{ControlMsg, EngineEvent};
use crate::logic::pacing::FramePacer;
use crate::logic::quality::{encode_config, Quality};

/// Everything `run` needs to bring a session up. `quality` is the starting rung;
/// later rungs arrive as `ControlMsg::Reconfigure`.
pub struct StartParams {
    pub ws_url: String,
    pub token: String,
    pub monitor_x: i32,
    pub monitor_y: i32,
    pub quality: Quality,
}

/// Run one screen-share session to completion. Blocks the calling thread until
/// `Stop` (or the control channel closing) or a fatal error. See the module docs
/// for the threading rationale (R9).
pub fn run(
    p: StartParams,
    mut ctl_rx: Receiver<ControlMsg>,
    emit: impl Fn(EngineEvent) + Send + 'static,
) {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            emit(EngineEvent::Error(format!("tokio runtime build failed: {e}")));
            return;
        }
    };

    rt.block_on(async move {
        if let Err(e) = engine_loop(p, &mut ctl_rx, &emit).await {
            emit(EngineEvent::Error(e.to_string()));
        }
    });
}

/// The `!Send` loop body. Kept as its own `async fn` so `?` funnels into the one
/// `Error` event in `run`.
async fn engine_loop(
    p: StartParams,
    ctl_rx: &mut Receiver<ControlMsg>,
    emit: &impl Fn(EngineEvent),
) -> Result<()> {
    let mut cfg = encode_config(p.quality);

    // Capture is fixed at the monitor's native size; the shader downscales to the
    // rung size (src != dst -- first time that scaled path runs, see T18 note).
    let mut cap = Capture::for_bounds(p.monitor_x, p.monitor_y)?;
    let (src_w, src_h) = cap.size();
    let mut conv = Converter::new(cap.device(), src_w, src_h, cfg.width, cfg.height)?;

    let mut room = RoomEngine::connect(&p.ws_url, &p.token).await?;
    room.publish(cfg).await?;
    emit(EngineEvent::Started);

    let mut pacer = FramePacer::new(cfg.fps);
    let mut buf = I420Buffer::new(cfg.width, cfg.height);
    // Last captured BGRA frame (bytes + row pitch). `next_bgra` borrows `&mut cap`,
    // so copy the bytes out before the pacer branch touches `conv`.
    let mut latest: Option<(Vec<u8>, usize)> = None;
    let mut last_stats = Instant::now();

    loop {
        // ---- drain control messages ----
        loop {
            match ctl_rx.try_recv() {
                Ok(ControlMsg::Stop) => {
                    room.close().await;
                    emit(EngineEvent::Ended);
                    return Ok(());
                }
                Ok(ControlMsg::Reconfigure(q)) => {
                    cfg = encode_config(q);
                    room.reconfigure(cfg).await?;
                    conv.resize(cfg.width, cfg.height)?;
                    pacer.set_fps(cfg.fps);
                    buf = I420Buffer::new(cfg.width, cfg.height);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Host dropped the handle without a clean Stop -- treat as end.
                    room.close().await;
                    emit(EngineEvent::Ended);
                    return Ok(());
                }
            }
        }

        // ---- pull the freshest frame (non-blocking) ----
        if let Some(frame) = cap.next_bgra()? {
            latest = Some((frame.bgra.to_vec(), frame.row_pitch));
        }

        // ---- emit one paced frame ----
        if pacer.tick(Instant::now()) {
            if let Some((bgra, row_pitch)) = latest.as_ref() {
                conv.to_i420(bgra, *row_pitch, &mut buf)?;
                room.push_i420(&buf);
            }
        }

        // ---- ~1 Hz stats ----
        if last_stats.elapsed() >= Duration::from_secs(1) {
            if let Some(s) = room.stats().await {
                emit(EngineEvent::Stats(s));
            }
            last_stats = Instant::now();
        }

        // ---- sleep to the next frame deadline, capped so ctl_rx stays responsive ----
        let wait = pacer
            .next_deadline()
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(10));
        tokio::time::sleep(wait).await;
    }
}
