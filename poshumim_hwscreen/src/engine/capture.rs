//! `engine::capture` — Windows.Graphics.Capture of a single monitor.
//!
//! Ported from `probe4_wgc/src/main.rs` (Phase 0, probe 4). The probe hard-coded
//! `MonitorFromPoint(POINT{0,0}, MONITOR_DEFAULTTOPRIMARY)`; here `for_bounds(x, y)`
//! enumerates the monitors (`EnumDisplayMonitors` + `GetMonitorInfoW`) and picks
//! the one whose `rcMonitor.left/top == (x, y)`, falling back to the primary.
//!
//! Consumers of these entry points land in later tasks (T6 `engine::convert`
//! takes `device()`, T8 `engine::run` / `napi_api` drive the whole struct), so
//! the module is `#![allow(dead_code)]` until then.
#![allow(dead_code)]

use anyhow::{anyhow, Result};

use windows::core::Interface;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

/// `MONITORINFOF_PRIMARY` from winuser.h. Declared locally so the crate does not
/// have to pull the whole `Win32_UI_WindowsAndMessaging` feature for one bit.
const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;

/// A monitor's virtual-desktop rectangle plus whether it is the primary display.
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

struct RawMon {
    hmon: HMONITOR,
    rect: MonitorRect,
}

unsafe extern "system" fn enum_proc(
    hmon: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let out = &mut *(data.0 as *mut Vec<RawMon>);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        let r = mi.rcMonitor;
        out.push(RawMon {
            hmon,
            rect: MonitorRect {
                x: r.left,
                y: r.top,
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
                primary: (mi.dwFlags & MONITORINFOF_PRIMARY) != 0,
            },
        });
    }
    BOOL(1) // non-zero => keep enumerating
}

fn enum_monitors_raw() -> Vec<RawMon> {
    let mut out: Vec<RawMon> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC(std::ptr::null_mut()),
            None,
            Some(enum_proc),
            LPARAM(&mut out as *mut Vec<RawMon> as isize),
        );
    }
    out
}

/// Enumerate every monitor's virtual-desktop rect (top-left, size, primary flag).
pub fn enumerate_monitors() -> Vec<MonitorRect> {
    enum_monitors_raw().into_iter().map(|m| m.rect).collect()
}

/// One captured, CPU-readable BGRA frame borrowed from the [`Capture`]'s buffer.
pub struct Frame<'a> {
    pub bgra: &'a [u8],
    pub row_pitch: usize,
    pub width: u32,
    pub height: u32,
}

/// A live Windows.Graphics.Capture session over a single monitor, plus the D3D11
/// device/context and a cached staging texture for CPU readback.
pub struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    _session: GraphicsCaptureSession,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    buf: Vec<u8>,
    row_pitch: usize,
    w: u32,
    h: u32,
}

impl Capture {
    /// Start capturing the monitor whose `rcMonitor.left/top == (x, y)`; if no
    /// monitor matches, fall back to the primary (then to the first enumerated).
    pub fn for_bounds(x: i32, y: i32) -> Result<Capture> {
        let mons = enum_monitors_raw();
        let chosen = mons
            .iter()
            .find(|m| m.rect.x == x && m.rect.y == y)
            .or_else(|| mons.iter().find(|m| m.rect.primary))
            .or_else(|| mons.first())
            .ok_or_else(|| anyhow!("no monitors detected"))?;
        let hmon = chosen.hmon;

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

        Ok(Capture {
            device,
            context,
            pool,
            _session: session,
            staging: None,
            buf: Vec::new(),
            row_pitch: 0,
            w: size.Width as u32,
            h: size.Height as u32,
        })
    }

    /// The monitor size learned from the capture item (updated per frame).
    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    /// The D3D11 device backing this capture — shared with Task 6's `Converter`.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Poll one frame. `Ok(None)` = no new frame was ready this poll. On success
    /// the returned [`Frame`] borrows this `Capture`'s internal buffer until the
    /// next call. Handles `RowPitch != width * 4` by exposing the real pitch.
    pub fn next_bgra(&mut self) -> Result<Option<Frame<'_>>> {
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
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            unsafe { self.device.CreateTexture2D(&sdesc, None, Some(&mut tex))? };
            self.staging = Some((
                tex.ok_or_else(|| anyhow!("no staging texture"))?,
                desc.Width,
                desc.Height,
            ));
        }
        let (staging, _, _) = self.staging.as_ref().unwrap();

        unsafe { self.context.CopyResource(staging, &texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?
        };
        let row_pitch = mapped.RowPitch as usize;
        let need = row_pitch * desc.Height as usize;
        if self.buf.len() != need {
            self.buf.resize(need, 0);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(mapped.pData as *const u8, self.buf.as_mut_ptr(), need);
            self.context.Unmap(staging, 0);
        }
        let _ = frame.Close();
        self.row_pitch = row_pitch;

        Ok(Some(Frame {
            bgra: &self.buf,
            row_pitch: self.row_pitch,
            width: self.w,
            height: self.h,
        }))
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use std::io::Write;

    /// Write a 24-bit bottom-up BMP from a top-down BGRA buffer (`row_pitch`
    /// bytes/row). Copied verbatim from `probe4_wgc::dump_bmp`.
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

    #[test]
    #[ignore] // run explicitly: cargo test -p poshumim_hwscreen capture_dumps_a_bmp -- --ignored
    fn capture_dumps_a_bmp() {
        let mons = enumerate_monitors();
        assert!(!mons.is_empty());
        let p = mons.iter().find(|m| m.primary).unwrap();
        let mut cap = Capture::for_bounds(p.x, p.y).unwrap();
        let mut got = None;
        for _ in 0..200 {
            if let Some(f) = cap.next_bgra().unwrap() {
                got = Some((f.bgra.to_vec(), f.row_pitch, f.width, f.height));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (bgra, pitch, w, h) = got.expect("a frame within 1s");
        // reuse probe4's dump_bmp (copied into this test module)
        dump_bmp("hwscreen-capture-smoke.bmp", &bgra, w, h, pitch).unwrap();
        eprintln!("wrote hwscreen-capture-smoke.bmp ({w}x{h})");
    }
}
