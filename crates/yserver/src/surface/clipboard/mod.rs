//! The clipboard bridge, XQuartz's `xpbproxy` model: the server's own X
//! client (an internal connection, `Message::InternalClient`) keeps
//! CLIPBOARD and the host pasteboard in step, as text, while the sync
//! setting is on. PRIMARY is not bridged.

mod bridge;
pub(crate) mod conn;
#[cfg(all(test, kms))]
mod tests;
mod text;

#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use yserver_core::core_loop::{CoreSender, Message};

#[cfg(windows)]
pub(crate) use text::{from_windows_units, to_windows_units};

/// The bridge's end of its internal connection.
#[cfg(unix)]
pub(crate) type Stream = UnixStream;
/// The bridge's end of its internal connection: Windows has no
/// `socketpair`, so a loopback TCP pair.
#[cfg(windows)]
pub(crate) type Stream = std::net::TcpStream;

/// A connected pair: the core's end, as the core takes it, and ours.
#[cfg(unix)]
fn pair() -> io::Result<(yserver_core::transport::Transport, Stream)> {
    let (core_side, ours) = UnixStream::pair()?;
    Ok((core_side.into(), ours))
}

/// A connected loopback pair from a one-shot listener; the accepted end
/// must be the peer we connected, not another local process that raced in.
#[cfg(windows)]
fn pair() -> io::Result<(yserver_core::transport::Transport, Stream)> {
    use std::net::{Ipv4Addr, TcpListener, TcpStream};

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let ours = TcpStream::connect(listener.local_addr()?)?;
    let (core_side, peer) = listener.accept()?;
    if peer != ours.local_addr()? {
        return Err(io::Error::other(
            "clipboard: a stranger took the loopback pair",
        ));
    }
    ours.set_nodelay(true)?;
    core_side.set_nodelay(true)?;
    Ok((yserver_core::transport::Transport::Tcp(core_side), ours))
}

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

/// The host pasteboard and the sync setting, which the host flips at any
/// time; the bridge follows within one poll.
pub(crate) struct HostClipboard {
    pub(crate) pasteboard: Box<dyn Pasteboard>,
    pub(crate) sync: Arc<AtomicBool>,
}

/// The bridge's thread; reconnects until stopped.
pub(crate) struct ClipboardBridge {
    stop: Arc<AtomicBool>,
    /// The live connection's socket, shut down to stop.
    socket: Arc<Mutex<Option<Stream>>>,
    thread: Option<JoinHandle<()>>,
}

impl ClipboardBridge {
    /// Start the bridge as an internal client of the core behind `sender`.
    ///
    /// # Errors
    ///
    /// The thread could not be spawned.
    pub(crate) fn start(sender: CoreSender, host: HostClipboard) -> io::Result<Self> {
        let HostClipboard {
            mut pasteboard,
            sync,
        } = host;
        let stop = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, socket) = (Arc::clone(&stop), Arc::clone(&socket));
            thread::Builder::new()
                .name("yserver-clipboard".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        if let Err(e) = serve(&sender, &mut *pasteboard, &sync, &stop, &socket) {
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
    sync: &AtomicBool,
    stop: &AtomicBool,
    socket: &Mutex<Option<Stream>>,
) -> io::Result<()> {
    let (core_side, ours) = pair()?;
    {
        let mut slot = socket.lock().unwrap_or_else(|e| e.into_inner());
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        *slot = Some(ours.try_clone()?);
    }
    sender.send(Message::InternalClient(core_side))?;
    let conn = conn::Conn::setup(ours)?;
    bridge::Bridge::new(conn, pasteboard, sync)?.run()
}
