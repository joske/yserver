//! Single-threaded core loop scaffolding.
//!
//! This module is being grown phase-by-phase per
//! `docs/superpowers/plans/2026-05-06-single-threaded-core.md`. In its
//! current form (Phase B) it only exposes the message types; the
//! sender/receiver pair and `run_core` come online in B2/B4.

pub mod barriers;
pub mod client_io;
pub mod client_reader;
pub mod damage_fanout;
pub mod fanout;
pub mod key_fanout;
pub mod message;
pub mod pointer_fanout;
pub mod poll_tokens;
pub mod process_disconnect;
pub mod process_request;
pub mod run;
pub mod sender;
pub mod setup_thread;
pub mod xi1_focus;
pub mod xi1_state_notify;

pub use message::{
    DeviceInfo, HostInputEvent, Message, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_LEFT, SYNTH_SCROLL_RIGHT,
    SYNTH_SCROLL_UP, SetupAllocateResponse,
};
pub use run::{handle_host_input, run_core};
pub use sender::{CoreReceiver, CoreSender, NOTIFY_TOKEN, channel};
