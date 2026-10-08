//! Voice and video call media (Appendix Q).
//!
//! WebRTC does the media work (capture, codecs, echo cancellation, jitter
//! buffers, congestion control). It reaches the peer only through a TURN
//! server inside this process ([`turn`]), whose relayed traffic travels as
//! the call's sealed media over the Threnody session.

pub mod call;
pub mod driver;
pub mod rtc;
pub mod turn;

pub use call::CallMedia;
pub use driver::{Driver, MediaState};
