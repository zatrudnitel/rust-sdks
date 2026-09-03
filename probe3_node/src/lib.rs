// Phase 0 / Probe 3: the probe2 connect+publish+encode path, exported as a
// napi-rs async function so it can be `require()`d from a bare Node process and
// from an Electron `utilityProcess`.
//
//   const m = require('./probe3_node.node');
//   const summary = await m.runSpike(wsUrl?, token?, seconds?);
//
// `summary` is a JSON string:
//   { "identity": "...", "room": "...", "encoder": "<encoder_implementation>",
//     "frames_encoded": <n>, "qlr": "<quality_limitation_reason>",
//     "nvenc": <bool> }
// so the JS host can assert NVENC engaged inside the utilityProcess.

#[macro_use]
extern crate napi_derive;

use anyhow::Result;
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoderBackend};
use livekit::track::{LocalTrack, LocalVideoTrack, TrackSource};
use livekit::webrtc::stats::RtcStats;
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use livekit::{Room, RoomOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const W: u32 = 1280;
const H: u32 = 720;
const FPS: u64 = 30;

fn mint_dev_token() -> Result<String> {
    use livekit_api::access_token::{AccessToken, VideoGrants};
    let jwt = AccessToken::with_api_key("devkey", "secret")
        .with_identity("hwscreen:spike3")
        .with_name("spike3")
        .with_grants(VideoGrants {
            room_join: true,
            room: "spike".to_string(),
            can_publish: true,
            can_subscribe: false,
            ..Default::default()
        })
        .to_jwt()?;
    Ok(jwt)
}

fn fill_bars(buf: &mut I420Buffer, t: u32) {
    let (y, u, v) = buf.data_mut();
    let w = W as usize;
    for (i, p) in y.iter_mut().enumerate() {
        let col = (i % w + t as usize) % w;
        let bar = col * 7 / w;
        *p = [235u8, 210, 170, 145, 105, 80, 40][bar.min(6)];
    }
    for p in u.iter_mut() {
        *p = 128;
    }
    for p in v.iter_mut() {
        *p = 128;
    }
}

struct Seen {
    encoder: String,
    frames_encoded: u32,
    qlr: String,
}

fn scan(stats: &[RtcStats]) -> Option<Seen> {
    for s in stats {
        if let RtcStats::OutboundRtp(o) = s {
            if o.stream.kind == "video" {
                return Some(Seen {
                    encoder: o.outbound.encoder_implementation.clone(),
                    frames_encoded: o.outbound.frames_encoded,
                    qlr: format!("{:?}", o.outbound.quality_limitation_reason),
                });
            }
        }
    }
    None
}

async fn spike_impl(ws_url: String, token: String, seconds: u64) -> Result<String> {
    log::info!("probe3: connecting to {ws_url} ...");
    let (room, _rx) = Room::connect(&ws_url, &token, RoomOptions::default()).await?;
    let identity = room.local_participant().identity().to_string();
    let room_name = room.name();
    log::info!("probe3: connected as {identity} in {room_name}");

    let source = NativeVideoSource::new(VideoResolution { width: W, height: H }, true);
    let track = LocalVideoTrack::create_video_track("spike3", RtcVideoSource::Native(source.clone()));

    room.local_participant()
        .publish_track(
            LocalTrack::Video(track.clone()),
            TrackPublishOptions {
                source: TrackSource::Screenshare,
                video_codec: VideoCodec::H264,
                video_encoder: VideoEncoderBackend::Nvenc,
                ..Default::default()
            },
        )
        .await?;
    log::info!("probe3: published H.264 / backend=Nvenc; pushing {W}x{H}@{FPS} for {seconds}s");

    let done = Arc::new(AtomicBool::new(false));
    let last: Arc<Mutex<Option<Seen>>> = Arc::new(Mutex::new(None));

    let stats_track = track.clone();
    let stats_last = last.clone();
    let stats_done = done.clone();
    let stats_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        while !stats_done.load(Ordering::Relaxed) {
            tick.tick().await;
            if let Ok(stats) = stats_track.get_stats().await {
                if let Some(seen) = scan(&stats) {
                    log::info!(
                        "probe3: outbound-rtp encoder={:?} frames_encoded={} qlr={}",
                        seen.encoder,
                        seen.frames_encoded,
                        seen.qlr
                    );
                    *stats_last.lock().unwrap() = Some(seen);
                }
            }
        }
    });

    let frame_dt = Duration::from_millis(1000 / FPS);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut t: u32 = 0;
    while tokio::time::Instant::now() < deadline {
        let mut buf = I420Buffer::new(W, H);
        fill_bars(&mut buf, t);
        source.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, buf));
        t = t.wrapping_add(4);
        tokio::time::sleep(frame_dt).await;
    }

    done.store(true, Ordering::Relaxed);
    let _ = stats_task.await;
    let _ = room.close().await;

    let (encoder, frames_encoded, qlr) = match last.lock().unwrap().take() {
        Some(s) => (s.encoder, s.frames_encoded, s.qlr),
        None => (String::new(), 0, "NoStats".to_string()),
    };
    let nvenc = encoder.contains("NVIDIA");
    Ok(format!(
        "{{\"identity\":{:?},\"room\":{:?},\"encoder\":{:?},\"frames_encoded\":{},\"qlr\":{:?},\"nvenc\":{}}}",
        identity, room_name, encoder, frames_encoded, qlr, nvenc
    ))
}

/// Run the spike. All args optional: `ws_url` defaults to `ws://127.0.0.1:7880`,
/// `token` to a self-minted `devkey`/`secret` dev token, `seconds` to 20.
#[napi]
pub async fn run_spike(
    ws_url: Option<String>,
    token: Option<String>,
    seconds: Option<u32>,
) -> napi::Result<String> {
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,livekit=info"),
    )
    .is_test(false)
    .try_init();

    let ws_url = ws_url
        .or_else(|| std::env::var("LK_URL").ok())
        .unwrap_or_else(|| "ws://127.0.0.1:7880".to_string());
    let token = match token.or_else(|| std::env::var("LK_TOKEN").ok()) {
        Some(t) => t,
        None => mint_dev_token().map_err(|e| napi::Error::from_reason(e.to_string()))?,
    };
    let seconds = seconds.unwrap_or(20) as u64;

    spike_impl(ws_url, token, seconds)
        .await
        .map_err(|e| napi::Error::from_reason(e.to_string()))
}
