//! napi-rs surface for the hardware screen-share engine -- the functions the
//! Electron `utilityProcess` calls.
//!
//! ```text
//! probeNvenc(): boolean
//! listMonitors(): { x, y, width, height, primary }[]
//! start(params: { wsUrl, token, monitorX, monitorY, quality }, onEvent): HwScreenHandle
//! HwScreenHandle.reconfigure(quality: string): void
//! HwScreenHandle.stop(): void
//! ```
//!
//! `start` spawns a dedicated OS thread running [`engine::run::run`] (which owns
//! its own multi-thread tokio runtime, R9) and returns a handle carrying the
//! control-channel sender + the join handle. Events come back through a
//! `ThreadsafeFunction` (`ErrorStrategy::Fatal` -- JS gets the event object
//! directly, no `(err, value)` tuple), called `NonBlocking` so the engine loop
//! never stalls on a slow JS listener.
//!
//! Every item here is reachable only from JS (via the `#[napi]` registration
//! shims), never from Rust, so the module is `#![allow(dead_code)]` like its
//! `engine::*` siblings.
#![allow(dead_code)]

use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

use crate::engine::{self, capture, room};
use crate::logic::control::{ControlMsg, EngineEvent};
use crate::logic::quality::Quality;

/// Whether this build can plausibly bring up an NVENC H.264 encoder. See
/// [`room::nvenc_supported`] for why this is a `cfg!(windows)` heuristic, not a
/// live probe.
#[napi]
pub fn probe_nvenc() -> bool {
    room::nvenc_supported()
}

/// One monitor's virtual-desktop rect plus the primary flag. `x`/`y` feed
/// `start`'s `monitorX`/`monitorY`.
#[napi(object)]
pub struct MonitorJs {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Enumerate every monitor (`EnumDisplayMonitors`).
#[napi]
pub fn list_monitors() -> Vec<MonitorJs> {
    capture::enumerate_monitors()
        .into_iter()
        .map(|m| MonitorJs {
            x: m.x,
            y: m.y,
            width: m.width,
            height: m.height,
            primary: m.primary,
        })
        .collect()
}

/// `start` parameters as they arrive from JS (camelCased: `wsUrl`, `monitorX`,
/// ...). `quality` is a string rung (`"low"` / `"medium"` / `"high"`); an
/// unrecognised value falls back to `Medium`.
#[napi(object)]
pub struct StartParamsJs {
    pub ws_url: String,
    pub token: String,
    pub monitor_x: i32,
    pub monitor_y: i32,
    pub quality: String,
}

impl From<StartParamsJs> for engine::run::StartParams {
    fn from(j: StartParamsJs) -> Self {
        engine::run::StartParams {
            ws_url: j.ws_url,
            token: j.token,
            monitor_x: j.monitor_x,
            monitor_y: j.monitor_y,
            quality: j.quality.parse().unwrap_or(Quality::Medium),
        }
    }
}

/// An engine event delivered to the JS `onEvent` callback. `type` is one of
/// `"started"` / `"stats"` / `"error"` / `"ended"`; the other fields are set only
/// for the variant that carries them (`stats` -> `fps` / `bitrateKbps` /
/// `rttMs`; `error` -> `message`).
#[napi(object)]
pub struct EngineEventJs {
    pub r#type: String,
    pub fps: Option<f64>,
    pub bitrate_kbps: Option<u32>,
    pub rtt_ms: Option<u32>,
    pub message: Option<String>,
}

impl From<EngineEvent> for EngineEventJs {
    fn from(e: EngineEvent) -> Self {
        let mut out = EngineEventJs {
            r#type: String::new(),
            fps: None,
            bitrate_kbps: None,
            rtt_ms: None,
            message: None,
        };
        match e {
            EngineEvent::Started => out.r#type = "started".to_string(),
            EngineEvent::Stats(s) => {
                out.r#type = "stats".to_string();
                out.fps = Some(s.fps);
                out.bitrate_kbps = Some(s.bitrate_kbps);
                out.rtt_ms = Some(s.rtt_ms);
            }
            EngineEvent::Error(m) => {
                out.r#type = "error".to_string();
                out.message = Some(m);
            }
            EngineEvent::Ended => out.r#type = "ended".to_string(),
        }
        out
    }
}

/// Handle to a running session. Held JS-side as a class instance; `reconfigure`
/// and `stop` push `ControlMsg`s to the engine thread.
#[napi]
pub struct HwScreenHandle {
    ctl_tx: tokio::sync::mpsc::Sender<ControlMsg>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Spawn the engine thread and return its handle. Non-fatal: a spawn failure is
/// the only error path (`start` itself does no I/O); every runtime failure comes
/// back as an `EngineEventJs { type: "error" }`.
#[napi]
pub fn start(
    params: StartParamsJs,
    on_event: ThreadsafeFunction<EngineEventJs, ErrorStrategy::Fatal>,
) -> napi::Result<HwScreenHandle> {
    let params: engine::run::StartParams = params.into();
    let (tx, rx) = tokio::sync::mpsc::channel(8);

    let join = std::thread::Builder::new()
        .name("hwscreen-engine".to_string())
        .spawn(move || {
            engine::run::run(params, rx, move |ev| {
                on_event.call(ev.into(), ThreadsafeFunctionCallMode::NonBlocking);
            });
        })
        .map_err(|e| napi::Error::from_reason(format!("spawn engine thread: {e}")))?;

    Ok(HwScreenHandle {
        ctl_tx: tx,
        join: Some(join),
    })
}

#[napi]
impl HwScreenHandle {
    /// Switch quality rung. Fire-and-forget: if the engine already stopped the
    /// send just fails silently.
    #[napi]
    pub fn reconfigure(&self, quality: String) {
        let q = quality.parse().unwrap_or(Quality::Medium);
        let _ = self.ctl_tx.blocking_send(ControlMsg::Reconfigure(q));
    }

    /// Stop the session and block until the engine thread has fully unwound
    /// (room closed, `Ended` emitted).
    #[napi]
    pub fn stop(&mut self) {
        let _ = self.ctl_tx.blocking_send(ControlMsg::Stop);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
