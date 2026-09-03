// Phase 0 / Probe 2: publish a synthetic H.264 track into a LiveKit room and
// report whether libwebrtc's send-side encoder is NVENC or software.
//
//   probe2-room <ws_url> <token>
// or with env: LK_URL / LK_TOKEN
//   probe2-room                       # mints its own token for ws://127.0.0.1:7880
//                                     # using the `livekit-server --dev` keys
//
// Prints, once a second:
//   outbound-rtp  codec=<n>  encoder=<encoder_implementation>  fps=<n>  qlr=<reason>

use anyhow::Result;
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoderBackend};
use livekit::track::{LocalTrack, LocalVideoTrack, TrackSource};
use livekit::webrtc::stats::RtcStats;
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use livekit::{Room, RoomOptions};
use std::time::Duration;

const W: u32 = 1280;
const H: u32 = 720;
const FPS: u64 = 30;

fn mint_dev_token() -> Result<String> {
    use livekit_api::access_token::{AccessToken, VideoGrants};
    let jwt = AccessToken::with_api_key("devkey", "secret")
        .with_identity("hwscreen:spike")
        .with_name("spike")
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
    // 7 vertical colour bars in the luma plane; chroma left neutral. Shift by t so
    // there is real inter-frame change (a static screen would let ddagrab-style
    // sources stall, and gives the rate controller nothing to do).
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

fn describe(s: &RtcStats) -> Option<String> {
    match s {
        RtcStats::OutboundRtp(o) if o.stream.kind == "video" => {
            let enc = if o.outbound.encoder_implementation.is_empty() {
                "<none yet>".to_string()
            } else {
                o.outbound.encoder_implementation.clone()
            };
            Some(format!(
                "outbound-rtp  encoder={:?}  fps={:.1}  frames_encoded={}  key={}  qlr={:?}",
                enc,
                o.outbound.frames_per_second,
                o.outbound.frames_encoded,
                o.outbound.key_frames_encoded,
                o.outbound.quality_limitation_reason
            ))
        }
        _ => None,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,livekit=info"))
        .init();

    let mut a = std::env::args().skip(1);
    let url = a
        .next()
        .or_else(|| std::env::var("LK_URL").ok())
        .unwrap_or_else(|| "ws://127.0.0.1:7880".to_string());
    let token = match a.next().or_else(|| std::env::var("LK_TOKEN").ok()) {
        Some(t) => t,
        None => mint_dev_token()?,
    };

    println!("connecting to {url} ...");
    let (room, _rx) = Room::connect(&url, &token, RoomOptions::default()).await?;
    println!("connected as {} in {}", room.local_participant().identity(), room.name());

    let source = NativeVideoSource::new(VideoResolution { width: W, height: H }, true);
    let track = LocalVideoTrack::create_video_track("spike", RtcVideoSource::Native(source.clone()));

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
    println!("published H.264 / requested backend = Nvenc; pushing {W}x{H}@{FPS} test pattern\n");

    let stats_track = track.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            match stats_track.get_stats().await {
                Ok(stats) => {
                    for s in &stats {
                        if let Some(line) = describe(s) {
                            println!("{line}");
                        }
                    }
                }
                Err(e) => println!("get_stats error: {e}"),
            }
        }
    });

    let frame_dt = Duration::from_millis(1000 / FPS);
    let mut t: u32 = 0;
    loop {
        let mut buf = I420Buffer::new(W, H);
        fill_bars(&mut buf, t);
        source.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, buf));
        t = t.wrapping_add(4);
        tokio::time::sleep(frame_dt).await;
    }
}
