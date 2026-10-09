//! A node that answers calls, for testing calls from an app without a
//! second person: it accepts whoever connects, answers every call with
//! this machine's microphone and speaker, logs how the call goes, and
//! hangs up after a while. With `BOT_CALL=1` it calls whoever connects
//! instead, to test ringing on the other side. With `BOT_VIDEO=1` it sends
//! moving colour bars as its video and logs the frames it receives (their
//! size and brightness, nothing more). It sends silence and plays
//! nothing unless `BOT_MIC=1`: a bot's speaker echoes into its microphone.
//! It sends back every reaction it gets, so both float on the app's screen.
//!
//! ```sh
//! cargo run -p threnody-ffi --features calls --example call_bot -- <home> [port] [seconds]
//! ```

use std::time::{Duration, Instant};

use threnody_ffi::{NodeEvent, ThrenodyNode};

fn main() {
    let mut args = std::env::args().skip(1);
    let home = args
        .next()
        .expect("usage: call_bot <home> [port] [seconds]");
    let port: u16 = args.next().map_or(7460, |p| p.parse().expect("port"));
    let talk = Duration::from_secs(args.next().map_or(20, |s| s.parse().expect("seconds")));

    let node = ThrenodyNode::open(home, None, None).expect("open");
    node.set_cover_traffic(None);
    node.set_call_devices(std::env::var_os("BOT_MIC").is_some());
    let addr = node.listen(format!("0.0.0.0:{port}")).expect("listen");
    let lan = std::env::var("BOT_ADDR").unwrap_or(addr);
    println!("invite: {}", node.invite_link(lan));
    println!("calls supported: {}", node.calls_supported());

    let calls_out = std::env::var_os("BOT_CALL").is_some();
    let video = std::env::var_os("BOT_VIDEO").is_some();
    if video {
        bars(node.clone());
        watch(node.clone());
    }
    let mut answered: Option<(u64, Instant)> = None;
    loop {
        if let Some((id, at)) = answered
            && at.elapsed() > talk
        {
            println!("hanging up after {talk:?}");
            node.hangup_call(id);
            answered = None;
        }
        let Some(e) = node.next_event(200) else {
            continue;
        };
        match e {
            NodeEvent::Connected { peer, .. } => {
                println!("connected: {peer}");
                let _ = node.accept_contact(peer.clone());
                if calls_out {
                    // Give the peer's Hello (and its call support) a moment.
                    std::thread::sleep(Duration::from_secs(2));
                    match node.start_call(peer, video) {
                        Ok(id) => println!("calling: {id:x}"),
                        Err(e) => println!("call failed: {e}"),
                    }
                }
            }
            NodeEvent::CallIncoming { peer, call, .. } => {
                println!("call {call:x} from {peer}: answering");
                match node.answer_call(call, video) {
                    Ok(()) => answered = Some((call, Instant::now())),
                    Err(e) => println!("answer failed: {e}"),
                }
            }
            NodeEvent::CallRinging { call, .. } => println!("call {call:x} ringing"),
            NodeEvent::CallStarted { call, .. } => {
                println!("call {call:x} answered");
                answered = Some((call, Instant::now()));
            }
            NodeEvent::CallMedia { call, state } => println!("call {call:x} audio: {state}"),
            NodeEvent::CallVideo { call, video, .. } => {
                println!("call {call:x} peer video: {video}")
            }
            NodeEvent::CallReaction { call, emoji, .. } => {
                println!("call {call:x} reaction {emoji}");
                let n = node.clone();
                // After a moment, so the two don't float on top of each other.
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    let _ = n.send_call_reaction(emoji);
                });
            }
            NodeEvent::CallEnded {
                call,
                reason,
                by_us,
                ..
            } => {
                println!("call {call:x} ended: {reason} (by us: {by_us})");
                answered = None;
            }
            NodeEvent::Disconnected { peer, reason } => println!("disconnected: {peer} ({reason})"),
            _ => {}
        }
    }
}

/// Sends moving colour bars (I420, 640×480, ~30 a second) whenever a call
/// runs.
fn bars(node: std::sync::Arc<ThrenodyNode>) {
    std::thread::spawn(move || {
        let (w, h) = (640usize, 480usize);
        let mut t = 0usize;
        loop {
            // Eight bars of rising brightness, scrolling sideways.
            let mut f = vec![0u8; w * h + 2 * (w / 2) * (h / 2)];
            for y in 0..h {
                for x in 0..w {
                    f[y * w + x] = (((x + t * 8) / 80 % 8) * 32 + 16) as u8;
                }
            }
            let (u, v) = f[w * h..].split_at_mut((w / 2) * (h / 2));
            for (i, (u, v)) in u.iter_mut().zip(v.iter_mut()).enumerate() {
                *u = ((i % (w / 2)) * 255 / (w / 2)) as u8;
                *v = 255 - *u;
            }
            node.send_video_frame(w as u32, h as u32, 0, f);
            t += 1;
            std::thread::sleep(Duration::from_millis(33));
        }
    });
}

/// Logs, every two seconds, how many frames of the peer's video arrived,
/// their size and their average brightness.
fn watch(node: std::sync::Arc<ThrenodyNode>) {
    std::thread::spawn(move || {
        let (mut n, mut last, mut at) = (0u32, None, Instant::now());
        loop {
            if let Some(f) = node.next_video_frame(250) {
                n += 1;
                let luma: u64 = f
                    .rgba
                    .chunks(4)
                    .map(|p| (u64::from(p[0]) + u64::from(p[1]) + u64::from(p[2])) / 3)
                    .sum();
                last = Some((
                    f.width,
                    f.height,
                    f.rotation,
                    luma / u64::from((f.width * f.height).max(1)),
                ));
            }
            if at.elapsed() >= Duration::from_secs(2) {
                if let Some((w, h, r, l)) = last {
                    println!("video in: {n} frames in 2s, {w}x{h} rot {r}, brightness {l}");
                }
                if let Some(s) = node.call_stats() {
                    println!("media: {s:?}");
                }
                (n, at) = (0, Instant::now());
            }
        }
    });
}
