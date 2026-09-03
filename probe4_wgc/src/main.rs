// Phase 0 / Probe 4 (adapted): the REAL native-HW-screen-share pipeline end to end.
//
//   Windows.Graphics.Capture (monitor) -> D3D11 BGRA texture -> staging copy ->
//   BGRA->I420 (CPU, BT.601) -> livekit NativeVideoSource::capture_frame ->
//   publish H.264 with VideoEncoderBackend::Nvenc -> 1 Hz outbound-rtp stats.
//
// Task 2/3 proved NVENC engages through libwebrtc's NativeVideoSource path, so
// probe 4 tests the one piece not yet exercised: WGC capture + colour convert
// feeding that same path with real screen content. The first 10 captured frames
// are dumped as .bmp so the capture itself can be eyeballed. No ffmpeg, no
// standalone NVENC harness -- that path is not what the integration uses.
//
//   probe4_wgc [ws_url] [token]      (env LK_URL / LK_TOKEN; defaults to dev)

use anyhow::{anyhow, Result};
use livekit::options::{TrackPublishOptions, VideoCodec, VideoEncoderBackend};
use livekit::track::{LocalTrack, LocalVideoTrack, TrackSource};
use livekit::webrtc::stats::RtcStats;
use livekit::webrtc::video_frame::{I420Buffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use livekit::{Room, RoomOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

const FPS: u64 = 30;
const DUMP_FRAMES: u64 = 10;

fn mint_dev_token() -> Result<String> {
    use livekit_api::access_token::{AccessToken, VideoGrants};
    Ok(AccessToken::with_api_key("devkey", "secret")
        .with_identity("hwscreen:spike4")
        .with_name("spike4")
        .with_grants(VideoGrants {
            room_join: true,
            room: "spike".to_string(),
            can_publish: true,
            can_subscribe: false,
            ..Default::default()
        })
        .to_jwt()?)
}

/// Write a 24-bit bottom-up BMP from a top-down BGRA buffer (row_pitch bytes/row).
fn dump_bmp(path: &str, bgra: &[u8], w: u32, h: u32, row_pitch: usize) -> std::io::Result<()> {
    let row = (w as usize * 3 + 3) & !3; // BMP rows padded to 4 bytes
    let img_size = row * h as usize;
    let file_size = 54 + img_size;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"BM")?;
    f.write_all(&(file_size as u32).to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;
    f.write_all(&40u32.to_le_bytes())?;
    f.write_all(&(w as i32).to_le_bytes())?;
    f.write_all(&(h as i32).to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&24u16.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&(img_size as u32).to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    let mut out = vec![0u8; row];
    for y in (0..h as usize).rev() {
        let src = &bgra[y * row_pitch..];
        for x in 0..w as usize {
            out[x * 3] = src[x * 4]; // B
            out[x * 3 + 1] = src[x * 4 + 1]; // G
            out[x * 3 + 2] = src[x * 4 + 2]; // R
        }
        f.write_all(&out)?;
    }
    Ok(())
}

/// BGRA (top-down, row_pitch) -> I420 planes, BT.601 limited range.
fn bgra_to_i420(bgra: &[u8], row_pitch: usize, w: u32, h: u32, buf: &mut I420Buffer) {
    let w = w as usize;
    let h = h as usize;
    let (sy, su, sv) = buf.strides();
    let (sy, su, sv) = (sy as usize, su as usize, sv as usize);
    let (yp, up, vp) = buf.data_mut();
    for j in 0..h {
        let row = &bgra[j * row_pitch..];
        for i in 0..w {
            let b = row[i * 4] as i32;
            let g = row[i * 4 + 1] as i32;
            let r = row[i * 4 + 2] as i32;
            let y = (66 * r + 129 * g + 25 * b + 128 >> 8) + 16;
            yp[j * sy + i] = y.clamp(0, 255) as u8;
            if j % 2 == 0 && i % 2 == 0 {
                let u = (-38 * r - 74 * g + 112 * b + 128 >> 8) + 128;
                let v = (112 * r - 94 * g - 18 * b + 128 >> 8) + 128;
                up[(j / 2) * su + i / 2] = u.clamp(0, 255) as u8;
                vp[(j / 2) * sv + i / 2] = v.clamp(0, 255) as u8;
            }
        }
    }
}

struct Capture {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    _session: windows::Graphics::Capture::GraphicsCaptureSession,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    w: u32,
    h: u32,
}

impl Capture {
    fn new() -> Result<Self> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.ok_or_else(|| anyhow!("no D3D11 device"))?;
        let context = context.ok_or_else(|| anyhow!("no D3D11 context"))?;

        let dxgi: IDXGIDevice = device.cast()?;
        let d3d_device: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()? };

        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(hmon)? };
        let size = item.Size()?;

        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // Win11-only; ignore on older contracts.
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(true);
        session.StartCapture()?;

        Ok(Self {
            _device: device,
            context,
            pool,
            _session: session,
            staging: None,
            w: size.Width as u32,
            h: size.Height as u32,
        })
    }

    /// Poll one frame; returns (bgra, row_pitch) if a new frame was ready.
    fn try_frame(&mut self) -> Result<Option<(Vec<u8>, usize)>> {
        let frame = match self.pool.TryGetNextFrame() {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        self.w = desc.Width;
        self.h = desc.Height;

        let need_new = match &self.staging {
            Some((_, sw, sh)) => *sw != desc.Width || *sh != desc.Height,
            None => true,
        };
        if need_new {
            let sdesc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            unsafe { self._device.CreateTexture2D(&sdesc, None, Some(&mut tex))? };
            self.staging = Some((tex.unwrap(), desc.Width, desc.Height));
        }
        let (staging, _, _) = self.staging.as_ref().unwrap();

        unsafe { self.context.CopyResource(staging, &texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?
        };
        let row_pitch = mapped.RowPitch as usize;
        let mut out = vec![0u8; row_pitch * desc.Height as usize];
        unsafe {
            std::ptr::copy_nonoverlapping(
                mapped.pData as *const u8,
                out.as_mut_ptr(),
                out.len(),
            );
            self.context.Unmap(staging, 0);
        }
        let _ = frame.Close();
        Ok(Some((out, row_pitch)))
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

    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .or_else(|| std::env::var("LK_URL").ok())
        .unwrap_or_else(|| "ws://127.0.0.1:7880".to_string());
    let token = match args.next().or_else(|| std::env::var("LK_TOKEN").ok()) {
        Some(t) => t,
        None => mint_dev_token()?,
    };

    // --- start capture, learn the monitor size from the first real frame ------
    let mut cap = Capture::new()?;
    println!("WGC session started; monitor {}x{}", cap.w, cap.h);
    let (first_bgra, first_pitch) = loop {
        if let Some(f) = cap.try_frame()? {
            break f;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let (cw, ch) = (cap.w, cap.h);
    // libwebrtc/NVENC want even dimensions.
    let (ew, eh) = (cw & !1, ch & !1);
    println!("first frame: {}x{} row_pitch={} -> encoding {}x{}", cw, ch, first_pitch, ew, eh);
    let _ = std::fs::create_dir_all("wgc-dump");
    dump_bmp("wgc-dump/frame_00.bmp", &first_bgra, cw, ch, first_pitch)?;

    // --- livekit connect + publish -----------------------------------------
    println!("connecting to {url} ...");
    let (room, _rx) = Room::connect(&url, &token, RoomOptions::default()).await?;
    println!("connected as {} in {}", room.local_participant().identity(), room.name());

    let source = NativeVideoSource::new(VideoResolution { width: ew, height: eh }, true);
    let track = LocalVideoTrack::create_video_track("spike4", RtcVideoSource::Native(source.clone()));
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
    println!("published H.264 / backend=Nvenc; screen -> I420 -> capture_frame\n");

    let stop = Arc::new(AtomicBool::new(false));
    let captured = Arc::new(AtomicU64::new(0));
    let last_enc: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    // stats task
    {
        let track = track.clone();
        let last_enc = last_enc.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            while !stop.load(Ordering::Relaxed) {
                tick.tick().await;
                if let Ok(stats) = track.get_stats().await {
                    for s in &stats {
                        if let Some(line) = describe(s) {
                            println!("{line}");
                            if let RtcStats::OutboundRtp(o) = s {
                                if !o.outbound.encoder_implementation.is_empty() {
                                    *last_enc.lock().unwrap() =
                                        o.outbound.encoder_implementation.clone();
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // capture+convert+push on a blocking thread (D3D work must not sit on the async runtime)
    let cap_handle = {
        let source = source.clone();
        let captured = captured.clone();
        let stop = stop.clone();
        std::thread::spawn(move || -> Result<()> {
            let frame_dt = Duration::from_millis(1000 / FPS);
            let mut n: u64 = 0;
            // feed the first frame we already grabbed
            let mut push = |bgra: &[u8], pitch: usize, n: u64| {
                let mut buf = I420Buffer::new(ew, eh);
                bgra_to_i420(bgra, pitch, ew, eh, &mut buf);
                source.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, buf));
                if n < DUMP_FRAMES {
                    let _ = dump_bmp(
                        &format!("wgc-dump/frame_{:02}.bmp", n),
                        bgra,
                        ew,
                        eh,
                        pitch,
                    );
                }
            };
            push(&first_bgra, first_pitch, 0);
            n += 1;
            captured.store(n, Ordering::Relaxed);
            let mut last_bgra = first_bgra;
            let mut last_pitch = first_pitch;
            while !stop.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                match cap.try_frame() {
                    Ok(Some((bgra, pitch))) => {
                        last_bgra = bgra;
                        last_pitch = pitch;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("capture error: {e}");
                        break;
                    }
                }
                push(&last_bgra, last_pitch, n);
                n += 1;
                captured.store(n, Ordering::Relaxed);
                if let Some(d) = frame_dt.checked_sub(t0.elapsed()) {
                    std::thread::sleep(d);
                }
            }
            Ok(())
        })
    };

    tokio::time::sleep(Duration::from_secs(20)).await;
    stop.store(true, Ordering::Relaxed);
    let _ = cap_handle.join();
    let _ = room.close().await;

    let enc = last_enc.lock().unwrap().clone();
    let n = captured.load(Ordering::Relaxed);
    println!(
        "\nRESULT: captured+converted {n} frames; encoder_implementation = {:?}; nvenc = {}",
        enc,
        enc.contains("NVIDIA")
    );
    println!("BMP dumps in ./wgc-dump/ (frame_00..frame_{:02}.bmp)", DUMP_FRAMES - 1);
    Ok(())
}
