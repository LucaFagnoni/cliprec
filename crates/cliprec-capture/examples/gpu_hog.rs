//! Carico GPU per le prove dei frame persi: un processo a parte, come un gioco, che tiene il
//! motore 3D della scheda del monitor principale al 100% con un pixel shader pesante a 1440p,
//! senza aprire finestre. Uso: `cargo run --release -p cliprec-capture --example gpu_hog -- <secondi>`.

#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC};
use windows::core::s;

const SHADERS: &str = r"
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
}
float4 ps(float4 pos : SV_Position) : SV_Target {
    float3 c = float3(pos.xy * 0.001, 0.5);
    [loop] for (uint i = 0; i < 400; i++) { c = sin(c * 1.37 + cos(c.yzx * 2.11)) + 0.1; }
    return float4(c, 1);
}
";

fn compile(entry: windows::core::PCSTR, target: windows::core::PCSTR) -> Vec<u8> {
    let mut code = None;
    let mut errors = None;
    // SAFETY: sorgente e nomi validi per tutta la chiamata; puntatori d'uscita locali.
    let r = unsafe { D3DCompile(SHADERS.as_ptr().cast(), SHADERS.len(), s!("hog"), None, None, entry, target, 0, 0, &mut code, Some(&mut errors)) };
    if let Err(e) = r {
        // SAFETY: il blob degli errori contiene testo ASCII della lunghezza dichiarata.
        let msg = errors.map(|b| unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())).into_owned() });
        panic!("shader: {e} {msg:?}");
    }
    let code = code.unwrap();
    // SAFETY: blob valido appena restituito.
    unsafe { std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize()).to_vec() }
}

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(10);
    let gpu = cliprec_capture::device_for_monitor(0).unwrap();
    let (vs, ps) = (compile(s!("vs"), s!("vs_5_0")), compile(s!("ps"), s!("ps_5_0")));
    // SAFETY: chiamate D3D11 su un device valido con descrittori e puntatori d'uscita locali.
    unsafe {
        let dev = &gpu.device;
        let ctx = &gpu.context;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 2560,
            Height: 1440,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let mut tex = None;
        dev.CreateTexture2D(&desc, None, Some(&mut tex)).unwrap();
        let mut rtv = None;
        dev.CreateRenderTargetView(tex.as_ref().unwrap(), None, Some(&mut rtv)).unwrap();
        let (mut v, mut p) = (None, None);
        dev.CreateVertexShader(&vs, None, Some(&mut v)).unwrap();
        dev.CreatePixelShader(&ps, None, Some(&mut p)).unwrap();
        ctx.OMSetRenderTargets(Some(&[rtv.clone()]), None);
        ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: 2560.0, Height: 1440.0, MaxDepth: 1.0, ..Default::default() }]));
        ctx.VSSetShader(v.as_ref(), None);
        ctx.PSSetShader(p.as_ref(), None);
        ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        let end = Instant::now() + Duration::from_secs(secs);
        let mut draws = 0u64;
        while Instant::now() < end {
            ctx.Draw(3, 0);
            ctx.Flush();
            draws += 1;
        }
        println!("gpu_hog: {draws} disegni in {secs} s");
    }
}
