//! What the node says: printed to stdout, or, while a control-socket
//! command runs ([`capture`]), kept for that command's client instead.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Running as a service: stdout goes to a system log, so what people
/// write isn't printed there ([`private`]).
static SERVICE: AtomicBool = AtomicBool::new(false);

pub fn set_service(on: bool) {
    SERVICE.store(on, Ordering::Relaxed);
}

/// Whether message contents must stay out of stdout (running as a
/// service, its log); `/history` still shows them to `threnody ctl`.
pub fn private() -> bool {
    SERVICE.load(Ordering::Relaxed)
}

/// Text said while a control command runs. Commands run one at a time on
/// the UI task, so one buffer is enough; whatever another task says
/// meanwhile goes to that client too.
static CAPTURE: Mutex<Option<String>> = Mutex::new(None);

/// Prints `s` (with no newline added), or keeps it for the control
/// command running now: its answer can hold history, which a service's
/// log mustn't.
pub fn say(s: &str) {
    if let Some(buf) = CAPTURE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        buf.push_str(s);
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// Runs `f`, returning its result and everything said meanwhile.
pub async fn capture<T>(f: impl Future<Output = T>) -> (T, String) {
    *CAPTURE.lock().unwrap_or_else(|e| e.into_inner()) = Some(String::new());
    let r = f.await;
    let said = CAPTURE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default();
    (r, said)
}

/// `println!`, through [`say`].
macro_rules! say {
    () => {
        $crate::out::say("\n")
    };
    ($($t:tt)*) => {
        $crate::out::say(&format!("{}\n", format_args!($($t)*)))
    };
}

/// `print!`, through [`say`].
macro_rules! say_part {
    ($($t:tt)*) => {
        $crate::out::say(&format!($($t)*))
    };
}
