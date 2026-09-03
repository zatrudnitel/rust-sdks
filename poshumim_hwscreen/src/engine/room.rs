//! `engine::room` -- connect to a LiveKit room, publish one NVENC H.264 video
//! track, reconfigure it in place, and read send-side stats.
//!
//! This is the productised port of the Phase-0 probes `probe2/src/main.rs`
//! (native binary) and `probe3_node/src/lib.rs` (napi-rs) -- the connect +
//! publish + capture_frame + get_stats path they proved works against a local
//! `livekit-server --dev`. The only additions here are:
//!   * the publication handle is kept so the track can be torn down again, and
//!   * `reconfigure` (unpublish + re-publish on the same `Room`) -- see R1 below.
//!
//! Windows-native: the whole `engine` module is `#[cfg(all(windows, feature =
//! "engine"))]`, so this file only compiles there.
//!
//! Consumers land in later tasks (T8 `engine::run` / `napi_api` drive this),
//! so the module is `#![allow(dead_code)]` until then.
#![allow(dead_code)]

use anyhow::Result;
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::publication::LocalTrackPublication;
use livekit::track::{LocalTrack, LocalVideoTrack, TrackSource};
use livekit::webrtc::stats::RtcStats;
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use livekit::{Room, RoomOptions};
use std::sync::Mutex;

use crate::logic::control::StatsSnapshot;
use crate::logic::quality::EncodeConfig;

/// The three objects that make up one published track. `reconfigure` throws the
/// whole trio away and builds a fresh one at the new size/bitrate; the `Room`
/// they live on is untouched.
struct Published {
    /// Frame sink. `push_i420` feeds this.
    source: NativeVideoSource,
    /// The `LocalVideoTrack` -- `get_stats()` is called on it.
    track: LocalVideoTrack,
    /// The publication handle. We keep it *only* for `reconfigure`: its
    /// `.sid()` is the `TrackSid` that `unpublish_track` needs.
    publication: LocalTrackPublication,
}

/// A live LiveKit room with (optionally) one NVENC H.264 screenshare track.
pub struct RoomEngine {
    room: Room,
    /// Drains the `RoomEvent` stream. `Room::connect` hands back an
    /// `UnboundedReceiver`; if it is dropped the SDK logs a send error on every
    /// subsequent event, so we keep it alive in a task and abort it on `close`.
    events: tokio::task::JoinHandle<()>,
    /// `Some` once `publish` has run. `reconfigure` replaces it wholesale.
    published: Option<Published>,
    /// `(bytes_sent, timestamp_us)` from the previous `stats()` call, for the
    /// bitrate delta. `Mutex` because `stats(&self)` takes `&self`.
    last_bytes: Mutex<Option<(u64, i64)>>,
}

impl RoomEngine {
    /// `Room::connect(ws_url, token, RoomOptions::default())`, exactly as the
    /// probes do it, plus a task that drains the event receiver.
    pub async fn connect(ws_url: &str, token: &str) -> Result<RoomEngine> {
        let (room, mut rx) = Room::connect(ws_url, token, RoomOptions::default()).await?;
        log::info!(
            "engine::room: connected as {} in {}",
            room.local_participant().identity(),
            room.name()
        );
        let events = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        Ok(RoomEngine { room, events, published: None, last_bytes: Mutex::new(None) })
    }

    /// Create a `NativeVideoSource` + `LocalVideoTrack` at `cfg`'s size and
    /// publish it as an H.264 screenshare with the NVENC backend requested.
    /// Mirrors probe2/probe3's publish block; adds `video_encoding` (bitrate +
    /// framerate cap from `cfg`) and `simulcast: false`.
    pub async fn publish(&mut self, cfg: EncodeConfig) -> Result<()> {
        let source =
            NativeVideoSource::new(VideoResolution { width: cfg.width, height: cfg.height }, true);
        let track =
            LocalVideoTrack::create_video_track("hwscreen", RtcVideoSource::Native(source.clone()));

        let publication = self
            .room
            .local_participant()
            .publish_track(
                LocalTrack::Video(track.clone()),
                TrackPublishOptions {
                    source: TrackSource::Screenshare,
                    video_codec: VideoCodec::H264,
                    video_encoder: VideoEncoderBackend::Nvenc,
                    video_encoding: Some(VideoEncoding {
                        max_bitrate: cfg.max_bitrate_kbps as u64 * 1000,
                        max_framerate: cfg.fps as f64,
                    }),
                    simulcast: false,
                    ..Default::default()
                },
            )
            .await?;

        log::info!(
            "engine::room: published H.264/Nvenc {}x{}@{} <= {}kbps  sid={}",
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.max_bitrate_kbps,
            publication.sid()
        );
        *self.last_bytes.lock().unwrap() = None;
        self.published = Some(Published { source, track, publication });
        Ok(())
    }

    /// Feed one I420 frame into the encoder. probe2/probe3's `capture_frame`
    /// call, but taking the buffer by reference: `I420Buffer` is not `Clone`,
    /// and `VideoFrame::new` / `capture_frame` are generic over
    /// `T: AsRef<dyn VideoBuffer>`, which `&I420Buffer` satisfies -- no copy of
    /// the plane data.
    pub fn push_i420(&self, buf: &I420Buffer) {
        if let Some(p) = self.published.as_ref() {
            p.source.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, buf));
        }
    }

    /// Change quality rung in place.
    ///
    /// **Ruling R1 (Phase 0):** the `livekit` crate exposes **no** in-place
    /// `max_bitrate` / resolution setter on a live sender. `TrackPublishOptions`
    /// (which carries `video_encoding` and the NVENC backend request) is
    /// consumed once at `publish_track` time, and `RtpSender::set_parameters` in
    /// this fork will not restripe the encoding list. So the only mechanism is:
    /// `unpublish_track(sid)` the current track, then `publish(cfg)` a brand-new
    /// `NativeVideoSource` + `LocalVideoTrack` at the new width/height/bitrate --
    /// on the **same** `Room`. The signalling websocket, ICE and DTLS are all
    /// untouched; only an AddTrack/RemoveTrack plus a renegotiation of that one
    /// transceiver occurs, which is sub-200ms against a local SFU. (Phase 0 also
    /// established that `unpublish_track` in this fork takes just `&TrackSid` --
    /// the `stop_on_unpublish` bool is commented out upstream.)
    pub async fn reconfigure(&mut self, cfg: EncodeConfig) -> Result<()> {
        if let Some(p) = self.published.take() {
            let sid = p.publication.sid();
            self.room.local_participant().unpublish_track(&sid).await?;
            log::info!("engine::room: unpublished {sid} for reconfigure");
        }
        self.publish(cfg).await
    }

    /// Send-side stats for the UI overlay. `None` until libwebrtc has an
    /// outbound-rtp video row. The bitrate is a rough `Δbytes*8/Δt`; if there is
    /// no previous sample yet it falls back to the encoder's `target_bitrate`.
    /// This feeds an overlay, never a gate, so the arithmetic is deliberately
    /// approximate.
    pub async fn stats(&self) -> Option<StatsSnapshot> {
        let p = self.published.as_ref()?;
        let stats = p.track.get_stats().await.ok()?;

        // RTT comes from the remote-inbound-rtp video row (seconds -> ms); 0 if
        // the SFU hasn't reported a receiver report yet.
        let mut rtt_ms = 0u32;
        for s in &stats {
            if let RtcStats::RemoteInboundRtp(r) = s {
                if r.stream.kind == "video" && r.remote_inbound.round_trip_time > 0.0 {
                    rtt_ms = (r.remote_inbound.round_trip_time * 1000.0).round() as u32;
                }
            }
        }

        for s in &stats {
            let RtcStats::OutboundRtp(o) = s else { continue };
            if o.stream.kind != "video" {
                continue;
            }
            let bytes = o.sent.bytes_sent;
            let ts_us = o.rtc.timestamp;
            let bitrate_kbps = {
                let mut last = self.last_bytes.lock().unwrap();
                let kbps = match *last {
                    Some((pb, pts)) if ts_us > pts && bytes >= pb => {
                        let dt_s = (ts_us - pts) as f64 / 1_000_000.0;
                        ((bytes - pb) as f64 * 8.0 / dt_s / 1000.0).round() as u32
                    }
                    // first sample (or a counter reset): use what the encoder
                    // is aiming for instead of a bogus huge delta.
                    _ => (o.outbound.target_bitrate / 1000.0).round() as u32,
                };
                *last = Some((bytes, ts_us));
                kbps
            };
            return Some(StatsSnapshot {
                fps: o.outbound.frames_per_second,
                bitrate_kbps,
                rtt_ms,
            });
        }
        None
    }

    /// The libwebrtc `encoder_implementation` string for the outbound video row,
    /// once available. "NVIDIA ..." iff NVENC actually engaged; "SimulcastEncoderAdapter
    /// (libvpx, ...)" / "OpenH264" for the software fallback. Diagnostics only.
    pub async fn encoder_impl(&self) -> Option<String> {
        let p = self.published.as_ref()?;
        let stats = p.track.get_stats().await.ok()?;
        for s in &stats {
            if let RtcStats::OutboundRtp(o) = s {
                if o.stream.kind == "video" && !o.outbound.encoder_implementation.is_empty() {
                    return Some(o.outbound.encoder_implementation.clone());
                }
            }
        }
        None
    }

    /// Abort the event-drain task and close the room.
    pub async fn close(self) {
        self.events.abort();
        let _ = self.room.close().await;
    }
}

/// Whether this build can plausibly bring up an NVENC H.264 encoder.
///
/// This is the pragmatic `cfg!(windows)` fallback the Task 7 brief sanctions,
/// **not** a live probe. Rationale: libwebrtc's real gate,
/// `webrtc::NvidiaVideoEncoderFactory::IsSupported()` (in
/// `webrtc-sys/src/nvidia/nvidia_encoder_factory.cpp` -- it `dlopen`s
/// `nvEncodeAPI`, `cuInit`s, and opens a throwaway encode session), is a C++
/// `static` that is **not** surfaced through the `webrtc-sys` cxx bridge. Wiring
/// a fresh FFI export means rebuilding the vendored libwebrtc, which is
/// disproportionate for this task. The truth still surfaces downstream:
/// `publish()` logs libwebrtc's fallback warning if NVENC can't start, and
/// `stats()` / `encoder_impl()` report the real `encoder_implementation`
/// ("NVIDIA ..." only when NVENC actually engaged). Task 12's JS `capable()`
/// additionally checks the GPU vendor before this path is ever selected.
pub fn nvenc_supported() -> bool {
    cfg!(windows)
}

#[cfg(test)]
mod it {
    use super::*;
    use crate::engine::convert::cpu_to_i420;
    use crate::logic::quality::{encode_config, Quality};

    /// probe2's `mint_dev_token`, verbatim shape: the `livekit-server --dev`
    /// keys (`devkey` / `secret`), identity `hwscreen:it`, room `spike`.
    fn dev_token() -> String {
        use livekit_api::access_token::{AccessToken, VideoGrants};
        AccessToken::with_api_key("devkey", "secret")
            .with_identity("hwscreen:it")
            .with_name("hwscreen:it")
            .with_grants(VideoGrants {
                room_join: true,
                room: "spike".to_string(),
                can_publish: true,
                can_subscribe: false,
                ..Default::default()
            })
            .to_jwt()
            .expect("mint dev token")
    }

    /// Moving grey colour bars in BGRA (`w*4` stride, top-down), probe2's
    /// `fill_bars` palette. B=G=R so it survives the BT.601 round-trip as grey;
    /// the horizontal shift by `t` gives the rate controller real motion.
    fn bars(w: usize, h: usize, t: u32) -> Vec<u8> {
        const PAL: [u8; 7] = [235, 210, 170, 145, 105, 80, 40];
        let mut v = vec![0u8; w * 4 * h];
        for y in 0..h {
            for x in 0..w {
                let col = (x + t as usize) % w;
                let g = PAL[(col * 7 / w).min(6)];
                let p = (y * w + x) * 4;
                v[p] = g;
                v[p + 1] = g;
                v[p + 2] = g;
                v[p + 3] = 255;
            }
        }
        v
    }

    // Multi-thread flavour on purpose: probe2 ran under `#[tokio::main]` (a
    // multi-thread runtime) and the LiveKit signalling websocket handshake
    // stalls ("Handshake not finished") on a single-threaded current-thread
    // runtime. `rt-multi-thread` is already in the built `tokio` via the
    // `livekit` dependency's feature set (cargo feature unification).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore] // needs: docker run --rm -d -p 7880:7880 -p 7881:7881 -p 7882:7882/udp livekit/livekit-server:v1.13.1 --dev
    async fn publishes_nvenc_h264_and_reconfigures() {
        assert!(nvenc_supported(), "nvenc_supported() must be true on this Windows box");

        let mut e = RoomEngine::connect("ws://127.0.0.1:7880", &dev_token())
            .await
            .expect("connect to local livekit --dev");

        // --- rung 1: Low (1280x720@30) ---
        e.publish(encode_config(Quality::Low)).await.expect("publish Low");
        for t in 0..90u32 {
            let mut b = I420Buffer::new(1280, 720);
            cpu_to_i420(&bars(1280, 720, t.wrapping_mul(6)), 1280 * 4, 1280, 720, &mut b);
            e.push_i420(&b);
            tokio::time::sleep(std::time::Duration::from_millis(33)).await;
        }

        let s = e.stats().await.expect("stats() Some after 3s of frames");
        eprintln!("stats (Low): {s:?}");
        assert!(s.fps > 0.0, "expected non-zero fps, got {s:?}");
        let enc = e.encoder_impl().await;
        eprintln!("encoder_implementation (Low): {enc:?}");
        assert!(
            enc.as_deref().is_some_and(|s| s.contains("NVIDIA")),
            "expected NVENC encoder, got {enc:?}"
        );

        // --- reconfigure -> rung 3: High (1920x1080@60) ---
        e.reconfigure(encode_config(Quality::High)).await.expect("reconfigure High");
        for t in 0..90u32 {
            let mut b = I420Buffer::new(1920, 1080);
            cpu_to_i420(&bars(1920, 1080, t.wrapping_mul(6)), 1920 * 4, 1920, 1080, &mut b);
            e.push_i420(&b);
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
        }

        let s2 = e.stats().await.expect("stats() Some after reconfigure");
        eprintln!("stats (High): {s2:?}");
        assert!(s2.fps > 0.0, "expected non-zero fps after reconfigure, got {s2:?}");
        let enc = e.encoder_impl().await;
        eprintln!("encoder_implementation (High): {enc:?}");
        assert!(
            enc.as_deref().is_some_and(|s| s.contains("NVIDIA")),
            "expected NVENC encoder, got {enc:?}"
        );

        e.close().await;
    }
}
