//! The clipboard bridge, XQuartz's `xpbproxy` model: the server's own X
//! client (an internal connection, `Message::InternalClient`) keeps
//! CLIPBOARD and the host pasteboard in step, as text. PRIMARY is not
//! bridged.

mod bridge;
pub(crate) mod conn;
#[cfg(all(test, kms))]
mod tests;

use std::{
    io,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use yserver_core::core_loop::{CoreSender, Message};

/// After the connection is lost (KillClient, a server reset), reconnect
/// after this.
const RECONNECT: Duration = Duration::from_secs(1);

/// The host's clipboard, as text.
pub(crate) trait Pasteboard: Send {
    /// A counter that changes whenever the pasteboard's contents change.
    fn change_count(&mut self) -> i64;
    /// The pasteboard's text, if it holds text.
    fn read_text(&mut self) -> Option<String>;
    /// Replace the pasteboard's contents with `text`; returns the change
    /// count after the write.
    fn write_text(&mut self, text: &str) -> i64;
}

/// The bridge's thread; reconnects until stopped.
pub(crate) struct ClipboardBridge {
    stop: Arc<AtomicBool>,
    /// The live connection's socket, shut down to stop.
    socket: Arc<Mutex<Option<UnixStream>>>,
    thread: Option<JoinHandle<()>>,
}

impl ClipboardBridge {
    /// Start the bridge as an internal client of the core behind `sender`.
    ///
    /// # Errors
    ///
    /// The thread could not be spawned.
    pub(crate) fn start(
        sender: CoreSender,
        mut pasteboard: Box<dyn Pasteboard>,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, socket) = (Arc::clone(&stop), Arc::clone(&socket));
            thread::Builder::new()
                .name("yserver-clipboard".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        if let Err(e) = serve(&sender, &mut *pasteboard, &stop, &socket) {
                            log::debug!("clipboard: connection ended: {e}");
                        }
                        let mut waited = Duration::ZERO;
                        while waited < RECONNECT && !stop.load(Ordering::Acquire) {
                            thread::sleep(Duration::from_millis(50));
                            waited += Duration::from_millis(50);
                        }
                    }
                })?
        };
        Ok(Self {
            stop,
            socket,
            thread: Some(thread),
        })
    }

    /// Disconnect and join the thread.
    pub(crate) fn stop(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(socket) = self.socket.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ClipboardBridge {
    fn drop(&mut self) {
        self.shut_down();
    }
}

/// One connection: hand the core its end, set up, bridge until it ends.
fn serve(
    sender: &CoreSender,
    pasteboard: &mut dyn Pasteboard,
    stop: &AtomicBool,
    socket: &Mutex<Option<UnixStream>>,
) -> io::Result<()> {
    let (core_side, ours) = UnixStream::pair()?;
    {
        let mut slot = socket.lock().unwrap_or_else(|e| e.into_inner());
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        *slot = Some(ours.try_clone()?);
    }
    sender.send(Message::InternalClient(core_side))?;
    let conn = conn::Conn::setup(ours)?;
    bridge::Bridge::new(conn, pasteboard)?.run()
}
