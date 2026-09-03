//! `local_run` -- drive `engine::run` end to end against a local
//! `livekit-server --dev`, bypassing the napi layer (which needs a JS env for
//! its `ThreadsafeFunction`).
//!
//! ```text
//! docker run --rm -d -p 7880:7880 -p 7881:7881 -p 7882:7882/udp \
//!   --name lk-dev-t8 livekit/livekit-server:v1.13.1 --dev --bind 0.0.0.0
//! CARGO_TARGET_DIR=C:/lkrt cargo run -p poshumim_hwscreen \
//!   --target x86_64-pc-windows-msvc --example local_run
//! ```
//!
//! Mints its own `devkey`/`secret` dev token (override with `LK_URL` / `LK_TOKEN`),
//! lists monitors, prints `probe_nvenc()`, starts on the primary monitor at
//! `Quality::Low`, prints every `EngineEvent`, sends `Reconfigure(High)` at ~8 s
//! and `Stop` at ~20 s.

#[cfg(not(windows))]
fn main() {
    eprintln!("local_run is Windows-only (native WGC capture + NVENC engine)");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::thread;
    use std::time::Duration;

    use poshumim_hwscreen::engine::capture::enumerate_monitors;
    use poshumim_hwscreen::engine::room::nvenc_supported;
    use poshumim_hwscreen::engine::run::{run, StartParams};
    use poshumim_hwscreen::logic::control::ControlMsg;
    use poshumim_hwscreen::logic::quality::Quality;

    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,livekit=info"),
    )
    .is_test(false)
    .try_init();

    let ws_url = std::env::var("LK_URL").unwrap_or_else(|_| "ws://127.0.0.1:7880".to_string());
    let token = match std::env::var("LK_TOKEN") {
        Ok(t) => t,
        Err(_) => mint_dev_token()?,
    };

    let mons = enumerate_monitors();
    println!("monitors ({}):", mons.len());
    for m in &mons {
        println!(
            "  {:>5}x{:<5} @ ({},{}){}",
            m.width,
            m.height,
            m.x,
            m.y,
            if m.primary { "  [primary]" } else { "" }
        );
    }
    println!("probe_nvenc() = {}", nvenc_supported());

    let primary = mons
        .iter()
        .find(|m| m.primary)
        .or_else(|| mons.first())
        .ok_or_else(|| anyhow::anyhow!("no monitors detected"))?;
    println!(
        "starting on primary @ ({},{}) at Quality::Low  ws_url={ws_url}",
        primary.x, primary.y
    );

    let (tx, rx) = tokio::sync::mpsc::channel::<ControlMsg>(8);

    let timer = thread::spawn(move || {
        thread::sleep(Duration::from_secs(8));
        println!("[timer] -> Reconfigure(High)");
        let _ = tx.blocking_send(ControlMsg::Reconfigure(Quality::High));
        thread::sleep(Duration::from_secs(12));
        println!("[timer] -> Stop");
        let _ = tx.blocking_send(ControlMsg::Stop);
    });

    let params = StartParams {
        ws_url,
        token,
        monitor_x: primary.x,
        monitor_y: primary.y,
        quality: Quality::Low,
    };

    run(params, rx, |e| println!("[event] {e:?}"));

    let _ = timer.join();
    println!("local_run: done");
    Ok(())
}

/// probe2's `mint_dev_token`, verbatim shape: `livekit-server --dev` keys
/// (`devkey` / `secret`), room `spike`.
#[cfg(windows)]
fn mint_dev_token() -> anyhow::Result<String> {
    use livekit_api::access_token::{AccessToken, VideoGrants};
    let jwt = AccessToken::with_api_key("devkey", "secret")
        .with_identity("hwscreen:local_run")
        .with_name("hwscreen:local_run")
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
