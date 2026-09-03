// convert.hlsl -- BGRA -> I420 (BT.601 limited range), one D3D11 compute pass.
//
// `src` is a DXGI_FORMAT_R8G8B8A8_UNORM texture fed raw BGRA bytes, so in the
// shader channel .r is Blue, .g is Green, .b is Red (.a unused) -- we un-swizzle
// on load. One thread per destination luma pixel writes Y; the thread at the
// top-left of each 2x2 luma block (dstXY % 2 == 0) also writes U and V, sampling
// the same top-left source pixel the CPU oracle (probe4_wgc::bgra_to_i420) uses
// -- nearest, not a box average -- so the two paths line up within +/-3.
//
// Float BT.601 limited-range coefficients; values chosen to match the integer
// 66/129/25, -38/-74/112, 112/-94/-18 (>>8, +16 / +128) form to <1 LSB. Output
// is written normalised 0..1 into R8_UNORM UAVs; the D3D UNORM store rounds it
// back to the byte the readback returns.  (A build-time .cso via fxc/build.rs is
// a possible optimisation; we D3DCompile this at Converter::new for now.)

Texture2D<float4>  src    : register(t0);
RWTexture2D<float> yPlane : register(u0);
RWTexture2D<float> uPlane : register(u1);
RWTexture2D<float> vPlane : register(u2);

cbuffer Dims : register(b0)
{
    uint2 srcSize; // source width, height
    uint2 dstSize; // destination luma width, height
};

// dst luma pixel -> nearest source pixel (integer scale, clamped in-bounds).
float3 loadRGB(uint2 d)
{
    uint2 s = uint2(d.x * srcSize.x / dstSize.x, d.y * srcSize.y / dstSize.y);
    s = min(s, srcSize - uint2(1, 1));
    float4 c = src.Load(int3(int(s.x), int(s.y), 0));
    return float3(c.b, c.g, c.r); // BGRA-in-RGBA -> (R, G, B)
}

[numthreads(8, 8, 1)]
void main(uint3 tid : SV_DispatchThreadID)
{
    uint2 d = tid.xy;
    if (d.x >= dstSize.x || d.y >= dstSize.y)
        return;

    float3 p = loadRGB(d);
    yPlane[d] = saturate(0.256788 * p.r + 0.504129 * p.g + 0.097906 * p.b + 0.0627451);

    if ((d.x & 1u) == 0u && (d.y & 1u) == 0u)
    {
        uint2 c = uint2(d.x >> 1, d.y >> 1);
        uPlane[c] = saturate(-0.148223 * p.r - 0.290993 * p.g + 0.439216 * p.b + 0.501961);
        vPlane[c] = saturate( 0.439216 * p.r - 0.367788 * p.g - 0.071427 * p.b + 0.501961);
    }
}
