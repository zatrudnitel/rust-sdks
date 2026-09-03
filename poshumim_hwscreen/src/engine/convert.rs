//! `engine::convert` -- D3D11 compute-shader BGRA->I420 (BT.601 limited range).
//!
//! A captured BGRA frame is uploaded into a `DYNAMIC` source texture, a single
//! `[numthreads(8,8,1)]` compute pass (`convert.hlsl`) writes the three I420
//! planes into `R8_UNORM` UAV textures, and each plane is `CopyResource`d into a
//! `STAGING` texture and mapped back into a [`livekit`] `I420Buffer`.
//!
//! [`cpu_to_i420`] is `probe4_wgc::bgra_to_i420` kept verbatim as the oracle the
//! GPU path is checked against by `shader_matches_cpu_within_tolerance`.
//!
//! The shader is `D3DCompile`d from `include_str!("convert.hlsl")` in
//! [`Converter::new`] and the bytecode cached on the struct; a build-time `.cso`
//! (fxc in `build.rs`) would trim that one-time cost.
//!
//! `R8_UNORM` typed-UAV store is not in D3D11's guaranteed set but is supported
//! on every desktop GPU this ships to (the parity test box is an RTX 4070 Ti
//! SUPER). Fallback if that ever bites: `R32_FLOAT` plane textures + a float->u8
//! round during readback.
#![allow(dead_code)]

use anyhow::{anyhow, Result};
use livekit::webrtc::video_frame::I420Buffer;

use windows::core::PCSTR;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, ID3DInclude};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS,
    D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_READ,
    D3D11_MAP_WRITE_DISCARD, D3D11_MAPPED_SUBRESOURCE, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};

/// One `R8_UNORM` output plane: the UAV target the shader writes and the
/// `STAGING` twin it is copied into for CPU readback.
struct PlaneTex {
    uav_tex: ID3D11Texture2D,
    uav: ID3D11UnorderedAccessView,
    readback: ID3D11Texture2D,
    w: u32,
    h: u32,
}

impl PlaneTex {
    fn new(device: &ID3D11Device, w: u32, h: u32) -> Result<PlaneTex> {
        let mut base = tex2d_desc(w, h, DXGI_FORMAT_R8_UNORM);
        base.Usage = D3D11_USAGE_DEFAULT;
        base.BindFlags = D3D11_BIND_UNORDERED_ACCESS.0 as u32;
        let mut uav_tex = None;
        unsafe { device.CreateTexture2D(&base, None, Some(&mut uav_tex))? };
        let uav_tex = uav_tex.ok_or_else(|| anyhow!("plane UAV texture"))?;

        let mut uav = None;
        unsafe { device.CreateUnorderedAccessView(&uav_tex, None, Some(&mut uav))? };
        let uav = uav.ok_or_else(|| anyhow!("plane UAV"))?;

        let mut rb = tex2d_desc(w, h, DXGI_FORMAT_R8_UNORM);
        rb.Usage = D3D11_USAGE_STAGING;
        rb.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut readback = None;
        unsafe { device.CreateTexture2D(&rb, None, Some(&mut readback))? };
        let readback = readback.ok_or_else(|| anyhow!("plane staging texture"))?;

        Ok(PlaneTex { uav_tex, uav, readback, w, h })
    }
}

/// The Y / U / V plane trio at a given destination size.
struct Planes {
    y: PlaneTex,
    u: PlaneTex,
    v: PlaneTex,
}

impl Planes {
    fn new(device: &ID3D11Device, dst_w: u32, dst_h: u32) -> Result<Planes> {
        let (cw, ch) = (dst_w.div_ceil(2), dst_h.div_ceil(2));
        Ok(Planes {
            y: PlaneTex::new(device, dst_w, dst_h)?,
            u: PlaneTex::new(device, cw, ch)?,
            v: PlaneTex::new(device, cw, ch)?,
        })
    }
}

/// A D3D11 compute pipeline that turns BGRA frames into I420 buffers.
///
/// `src_w`/`src_h` are fixed for the life of the `Converter`; the destination
/// size can change via [`Converter::resize`].
pub struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    cs: ID3D11ComputeShader,
    /// Cached compiled shader bytecode (kept so a rebuild is a no-op).
    _cs_bytecode: Vec<u8>,
    src_tex: ID3D11Texture2D,
    src_srv: ID3D11ShaderResourceView,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    dims_cb: ID3D11Buffer,
    planes: Planes,
}

fn tex2d_desc(w: u32, h: u32, format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

/// A 16-byte constant buffer holding `[srcW, srcH, dstW, dstH]` for the shader.
fn make_dims_cb(device: &ID3D11Device, sw: u32, sh: u32, dw: u32, dh: u32) -> Result<ID3D11Buffer> {
    let data: [u32; 4] = [sw, sh, dw, dh];
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 16,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr() as *const core::ffi::c_void,
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut cb = None;
    unsafe { device.CreateBuffer(&desc, Some(&init), Some(&mut cb))? };
    cb.ok_or_else(|| anyhow!("dims constant buffer"))
}

impl Converter {
    pub fn new(
        device: &ID3D11Device,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<Converter> {
        let context = unsafe { device.GetImmediateContext()? };

        // ---- compile convert.hlsl (cs_5_0) ----
        let hlsl = include_str!("convert.hlsl");
        let mut blob: Option<ID3DBlob> = None;
        let mut errs: Option<ID3DBlob> = None;
        let compiled = unsafe {
            D3DCompile(
                hlsl.as_ptr() as *const core::ffi::c_void,
                hlsl.len(),
                PCSTR(c"convert.hlsl".as_ptr() as *const u8),
                None,
                None::<&ID3DInclude>,
                PCSTR(c"main".as_ptr() as *const u8),
                PCSTR(c"cs_5_0".as_ptr() as *const u8),
                0,
                0,
                &mut blob,
                Some(&mut errs),
            )
        };
        if let Err(e) = compiled {
            let msg = errs
                .map(|b| unsafe {
                    let p = b.GetBufferPointer() as *const u8;
                    String::from_utf8_lossy(std::slice::from_raw_parts(p, b.GetBufferSize()))
                        .into_owned()
                })
                .unwrap_or_default();
            return Err(anyhow!("D3DCompile(convert.hlsl): {e}: {msg}"));
        }
        let blob = blob.ok_or_else(|| anyhow!("no shader blob"))?;
        let bytecode = unsafe {
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
                .to_vec()
        };
        let mut cs = None;
        unsafe { device.CreateComputeShader(&bytecode, None, Some(&mut cs))? };
        let cs = cs.ok_or_else(|| anyhow!("no compute shader"))?;

        // ---- source texture (BGRA bytes in an R8G8B8A8_UNORM SRV) ----
        let mut src_desc = tex2d_desc(src_w, src_h, DXGI_FORMAT_R8G8B8A8_UNORM);
        src_desc.Usage = D3D11_USAGE_DYNAMIC;
        src_desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
        src_desc.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE.0 as u32;
        let mut src_tex = None;
        unsafe { device.CreateTexture2D(&src_desc, None, Some(&mut src_tex))? };
        let src_tex = src_tex.ok_or_else(|| anyhow!("source texture"))?;
        let mut src_srv = None;
        unsafe { device.CreateShaderResourceView(&src_tex, None, Some(&mut src_srv))? };
        let src_srv = src_srv.ok_or_else(|| anyhow!("source SRV"))?;

        let dims_cb = make_dims_cb(device, src_w, src_h, dst_w, dst_h)?;
        let planes = Planes::new(device, dst_w, dst_h)?;

        Ok(Converter {
            device: device.clone(),
            context,
            cs,
            _cs_bytecode: bytecode,
            src_tex,
            src_srv,
            src_w,
            src_h,
            dst_w,
            dst_h,
            dims_cb,
            planes,
        })
    }

    /// Drop and recreate the plane + readback textures (and the dims cbuffer) at
    /// a new destination size. No-op if the size is unchanged.
    pub fn resize(&mut self, dst_w: u32, dst_h: u32) -> Result<()> {
        if dst_w == self.dst_w && dst_h == self.dst_h {
            return Ok(());
        }
        self.planes = Planes::new(&self.device, dst_w, dst_h)?;
        self.dims_cb = make_dims_cb(&self.device, self.src_w, self.src_h, dst_w, dst_h)?;
        self.dst_w = dst_w;
        self.dst_h = dst_h;
        Ok(())
    }

    #[allow(clippy::wrong_self_convention)] // `to_i420` is the interface name in the task brief
    pub fn to_i420(
        &mut self,
        bgra: &[u8],
        row_pitch: usize,
        out: &mut I420Buffer,
    ) -> Result<()> {
        let ctx = self.context.clone();

        // ---- upload BGRA into the DYNAMIC source texture ----
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            ctx.Map(&self.src_tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
        }
        let dst_pitch = mapped.RowPitch as usize;
        let copy = row_pitch.min(dst_pitch).min(self.src_w as usize * 4);
        unsafe {
            for y in 0..self.src_h as usize {
                let src_off = y * row_pitch;
                if src_off + copy > bgra.len() {
                    break;
                }
                std::ptr::copy_nonoverlapping(
                    bgra.as_ptr().add(src_off),
                    (mapped.pData as *mut u8).add(y * dst_pitch),
                    copy,
                );
            }
            ctx.Unmap(&self.src_tex, 0);
        }

        // ---- one compute dispatch: BGRA -> Y / U / V ----
        unsafe {
            ctx.CSSetShader(&self.cs, None);
            ctx.CSSetShaderResources(0, Some(&[Some(self.src_srv.clone())]));
            ctx.CSSetConstantBuffers(0, Some(&[Some(self.dims_cb.clone())]));
            let uavs = [
                Some(self.planes.y.uav.clone()),
                Some(self.planes.u.uav.clone()),
                Some(self.planes.v.uav.clone()),
            ];
            ctx.CSSetUnorderedAccessViews(0, 3, Some(uavs.as_ptr()), None);
            ctx.Dispatch(self.dst_w.div_ceil(8), self.dst_h.div_ceil(8), 1);
            // release the UAVs so the plane textures can be CopyResource sources
            let null_uavs: [Option<ID3D11UnorderedAccessView>; 3] = [None, None, None];
            ctx.CSSetUnorderedAccessViews(0, 3, Some(null_uavs.as_ptr()), None);
        }

        // ---- readback each plane into the I420Buffer ----
        let (ys, us, vs) = out.strides();
        let (yb, ub, vb) = out.data_mut();
        read_plane(&ctx, &self.planes.y, yb, ys as usize)?;
        read_plane(&ctx, &self.planes.u, ub, us as usize)?;
        read_plane(&ctx, &self.planes.v, vb, vs as usize)?;
        Ok(())
    }
}

/// `CopyResource` a plane into its staging twin, then map+copy it row by row
/// into `out`, honouring both `out`'s stride and the mapped `RowPitch`.
fn read_plane(
    ctx: &ID3D11DeviceContext,
    plane: &PlaneTex,
    out: &mut [u8],
    stride: usize,
) -> Result<()> {
    unsafe {
        ctx.CopyResource(&plane.readback, &plane.uav_tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&plane.readback, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let src_pitch = m.RowPitch as usize;
        let w = plane.w as usize;
        for y in 0..plane.h as usize {
            let dst_off = y * stride;
            if dst_off + w > out.len() {
                break;
            }
            std::ptr::copy_nonoverlapping(
                (m.pData as *const u8).add(y * src_pitch),
                out.as_mut_ptr().add(dst_off),
                w,
            );
        }
        ctx.Unmap(&plane.readback, 0);
    }
    Ok(())
}

/// BGRA (top-down, `row_pitch` bytes/row) -> I420 planes, BT.601 limited range.
///
/// Verbatim `probe4_wgc::bgra_to_i420`; kept as the reference oracle the GPU
/// path is validated against.
pub fn cpu_to_i420(bgra: &[u8], row_pitch: usize, w: u32, h: u32, out: &mut I420Buffer) {
    let w = w as usize;
    let h = h as usize;
    let (sy, su, sv) = out.strides();
    let (sy, su, sv) = (sy as usize, su as usize, sv as usize);
    let (yp, up, vp) = out.data_mut();
    for j in 0..h {
        let row = &bgra[j * row_pitch..];
        for i in 0..w {
            let b = row[i * 4] as i32;
            let g = row[i * 4 + 1] as i32;
            let r = row[i * 4 + 2] as i32;
            let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            yp[j * sy + i] = y.clamp(0, 255) as u8;
            if j % 2 == 0 && i % 2 == 0 {
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                up[(j / 2) * su + i / 2] = u.clamp(0, 255) as u8;
                vp[(j / 2) * sv + i / 2] = v.clamp(0, 255) as u8;
            }
        }
    }
}

#[cfg(test)]
pub(super) fn test_device() -> ID3D11Device {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    };
    let mut device: Option<ID3D11Device> = None;
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
            None,
        )
        .expect("D3D11CreateDevice(HARDWARE)");
    }
    device.expect("no D3D11 device")
}

#[cfg(test)]
mod tests {
    use super::*;
    use livekit::webrtc::video_frame::I420Buffer;

    fn synthetic_bgra(w: u32, h: u32) -> (Vec<u8>, usize) {
        let pitch = (w * 4) as usize;
        let mut v = vec![0u8; pitch * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let p = y * pitch + x * 4;
                v[p] = (x % 256) as u8; // B
                v[p + 1] = (y % 256) as u8; // G
                v[p + 2] = ((x + y) % 256) as u8; // R
                v[p + 3] = 255;
            }
        }
        (v, pitch)
    }

    #[test]
    #[ignore] // needs a GPU: cargo test ... shader_matches_cpu -- --ignored
    fn shader_matches_cpu_within_tolerance() {
        let (w, h) = (256u32, 128u32);
        let (bgra, pitch) = synthetic_bgra(w, h);

        let mut cpu = I420Buffer::new(w, h);
        cpu_to_i420(&bgra, pitch, w, h, &mut cpu);

        let dev = super::test_device();
        let mut conv = Converter::new(&dev, w, h, w, h).unwrap();
        let mut gpu = I420Buffer::new(w, h);
        conv.to_i420(&bgra, pitch, &mut gpu).unwrap();

        let (cy, cu, cv) = cpu.data();
        let (gy, gu, gv) = gpu.data();
        let max_abs = |a: &[u8], b: &[u8]| {
            a.iter()
                .zip(b)
                .map(|(x, y)| (*x as i32 - *y as i32).abs())
                .max()
                .unwrap_or(0)
        };
        let (dy, du, dv) = (max_abs(cy, gy), max_abs(cu, gu), max_abs(cv, gv));
        eprintln!("max abs diff  Y={dy}  U={du}  V={dv}");
        assert!(dy <= 3, "Y diff {dy}");
        assert!(du <= 3, "U diff {du}");
        assert!(dv <= 3, "V diff {dv}");
    }
}
