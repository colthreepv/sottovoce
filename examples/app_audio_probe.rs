//! Passive probe for the calling-app title.
//!
//! Prints the Core Audio sessions on the default render endpoint with their
//! peaks, then the app the sampler would name the meeting after. Reading peak
//! meters plays no audio and captures nothing.
//!
//! Usage: cargo run --example app_audio_probe -- [seconds]

use std::time::{Duration, Instant};

use sottovoce_engine::app_audio::{Sampler, snapshot};

fn main() {
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .filter(|seconds| *seconds > 0 && *seconds <= 600)
        .unwrap_or(10);

    println!("Probing the default render endpoint for {seconds}s (default device)");
    let mut sampler = Sampler::new();
    for tick in 0..seconds {
        let sessions = snapshot(None);
        if sessions.is_empty() {
            println!("[{tick:>3}s] (no sessions)");
        } else {
            let parts: Vec<String> = sessions
                .iter()
                .map(|session| {
                    let label = session.app.clone().unwrap_or_else(|| {
                        if session.is_system {
                            "System Sounds".to_owned()
                        } else {
                            format!("PID {}", session.pid)
                        }
                    });
                    format!("{label} {:>6.3}", session.peak)
                })
                .collect();
            println!("[{tick:>3}s] {}", parts.join("  |  "));
        }
        sampler.sample(None, Instant::now());
        std::thread::sleep(Duration::from_secs(1));
    }

    match sampler.top() {
        Some(app) => println!("\ntop app: {app}"),
        None => println!("\ntop app: none (nothing above the threshold)"),
    }
}
