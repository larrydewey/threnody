//! A node that answers calls, for testing calls from an app without a
//! second person: it accepts whoever connects, answers every call with
//! this machine's microphone and speaker, logs how the call goes, and
//! hangs up after a while. With `BOT_CALL=1` it calls whoever connects
//! instead, to test ringing on the other side.
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
    let addr = node.listen(format!("0.0.0.0:{port}")).expect("listen");
    let lan = std::env::var("BOT_ADDR").unwrap_or(addr);
    println!("invite: {}", node.invite_link(lan));
    println!("calls supported: {}", node.calls_supported());

    let calls_out = std::env::var_os("BOT_CALL").is_some();
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
                    match node.start_call(peer, false) {
                        Ok(id) => println!("calling: {id:x}"),
                        Err(e) => println!("call failed: {e}"),
                    }
                }
            }
            NodeEvent::CallIncoming { peer, call, .. } => {
                println!("call {call:x} from {peer}: answering");
                match node.answer_call(call, false) {
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
