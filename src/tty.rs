//! Pseudo-terminal bridge between child processes and the TUI.
//!
//! Tools such as the AWS CLI or `sudo` prompt on `/dev/tty` rather than on
//! stdin or stderr. A child inherits the TUI's controlling terminal, so such a
//! prompt paints straight over the interface — outside whatever popup is
//! showing the command's piped output — and competes with the TUI for keys.
//!
//! When the TUI attaches its message sender here, interactive children get a
//! pty as their controlling terminal instead: whatever they write to the tty
//! is sent to the app as [`AppMessage::ChildTtyOutput`], and the app writes
//! typed or pasted input back through the sender in
//! [`AppMessage::ChildTtyOpened`].
//!
//! Headless commands never attach, so their children keep the real terminal.

use anyhow::Result;
use std::sync::Mutex;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::app::AppMessage;

static TERMINAL: Mutex<Option<mpsc::Sender<AppMessage>>> = Mutex::new(None);

/// Route child tty prompts to the app owning this sender.
pub fn attach(tx: mpsc::Sender<AppMessage>) {
    if let Ok(mut slot) = TERMINAL.lock() {
        *slot = Some(tx);
    }
}

/// Serializes tests, since the routed terminal is process-wide.
#[cfg(test)]
static TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Hold the process-wide terminal for the duration of one test.
#[cfg(test)]
pub async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_GUARD.lock().await
}

/// Stop routing child tty prompts; children fall back to the real terminal.
#[cfg(test)]
pub fn detach() {
    if let Ok(mut slot) = TERMINAL.lock() {
        *slot = None;
    }
}

fn attached() -> Option<mpsc::Sender<AppMessage>> {
    TERMINAL.lock().ok().and_then(|slot| slot.clone())
}

/// Give `cmd` a pty when the TUI is attached, so that a `/dev/tty` prompt
/// reaches the popup instead of the real terminal.
///
/// Call before spawning. The returned handle starts forwarding once the child
/// exists, and must be finished after it exits.
#[cfg(unix)]
pub fn attach_pty(cmd: &mut Command) -> Result<Option<ChildPty>> {
    let Some(tx) = attached() else {
        return Ok(None);
    };
    let mut pty = ChildPty::open(tx)?;
    pty.attach_to(cmd);
    Ok(Some(pty))
}

/// Windows children keep the real console, so no terminal is ever routed.
#[cfg(not(unix))]
pub fn attach_pty(_cmd: &mut Command) -> Result<Option<ChildPty>> {
    Ok(None)
}

#[cfg(unix)]
pub use unix::ChildPty;

#[cfg(unix)]
mod unix {
    use super::*;
    use anyhow::Context;
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::pty::{openpty, Winsize};
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};
    use std::process::Stdio;
    use std::sync::Arc;
    use tokio::io::unix::AsyncFd;
    use tokio::task::JoinHandle;
    use tokio::time::{timeout, Duration};

    /// A pty whose slave side becomes a child's stdin and controlling terminal.
    pub struct ChildPty {
        master: Arc<AsyncFd<OwnedFd>>,
        slave: Option<OwnedFd>,
        tx: mpsc::Sender<AppMessage>,
    }

    /// Reader and writer tasks for an open pty; finished once the child exits.
    pub struct ChildPtyIo {
        reader: JoinHandle<()>,
        writer: JoinHandle<()>,
        tx: mpsc::Sender<AppMessage>,
    }

    impl ChildPty {
        pub(super) fn open(tx: mpsc::Sender<AppMessage>) -> Result<Self> {
            // The width a prompt is wrapped at; the popup wraps again to fit.
            let size = Winsize {
                ws_row: 24,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let pty = openpty(Some(&size), None).context("Failed to open pty")?;
            fcntl(pty.master.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
                .context("Failed to make pty master non-blocking")?;
            let master = AsyncFd::new(pty.master).context("Failed to register pty master")?;
            Ok(Self {
                master: Arc::new(master),
                slave: Some(pty.slave),
                tx,
            })
        }

        /// Give the child the slave as stdin and as its controlling terminal,
        /// so `/dev/tty` inside the child resolves to this pty.
        pub(super) fn attach_to(&mut self, cmd: &mut Command) {
            if let Some(slave) = self.slave.take() {
                cmd.stdin(Stdio::from(std::fs::File::from(slave)));
            }
            // SAFETY: only async-signal-safe libc calls between fork and exec.
            // A failure here would leave the child on the TUI's own terminal,
            // so it is reported rather than ignored.
            unsafe {
                cmd.pre_exec(|| {
                    if nix::libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // As session leader, the child's exit hangs up the pty and
                    // SIGHUPs its process group — killing whatever it left in
                    // the background (`xdg-open … &`), even `nohup`/`setsid`
                    // launched ones that haven't exec'd yet. Ignored here, it
                    // is inherited by everything the child starts.
                    if nix::libc::signal(nix::libc::SIGHUP, nix::libc::SIG_IGN)
                        == nix::libc::SIG_ERR
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        /// Start forwarding tty output to the app and app input to the tty.
        pub async fn start_io(self) -> ChildPtyIo {
            let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>(32);
            let _ = self
                .tx
                .send(AppMessage::ChildTtyOpened { input: input_tx })
                .await;
            let reader = tokio::spawn(read_loop(Arc::clone(&self.master), self.tx.clone()));
            let writer = tokio::spawn(write_loop(self.master, input_rx));
            ChildPtyIo {
                reader,
                writer,
                tx: self.tx,
            }
        }
    }

    impl ChildPtyIo {
        /// Drain what the child wrote last, then release the terminal.
        ///
        /// The reader is given a moment rather than awaited to the end: a
        /// command may leave a background process holding the slave open,
        /// which would otherwise keep the terminal attached forever.
        pub async fn finish(self) {
            let _ = timeout(Duration::from_millis(200), self.reader).await;
            self.writer.abort();
            let _ = self.tx.send(AppMessage::ChildTtyClosed).await;
        }
    }

    async fn read_loop(master: Arc<AsyncFd<OwnedFd>>, tx: mpsc::Sender<AppMessage>) {
        let mut buf = [0u8; 4096];
        loop {
            let Ok(mut guard) = master.readable().await else {
                return;
            };
            match guard.try_io(|fd| {
                nix::unistd::read(fd.get_ref().as_raw_fd(), &mut buf).map_err(std::io::Error::from)
            }) {
                // EOF, or EIO once the slave side is closed
                Ok(Ok(0)) | Ok(Err(_)) => return,
                Ok(Ok(n)) => {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if tx.send(AppMessage::ChildTtyOutput(text)).await.is_err() {
                        return;
                    }
                }
                Err(_would_block) => continue,
            }
        }
    }

    async fn write_loop(master: Arc<AsyncFd<OwnedFd>>, mut input: mpsc::Receiver<Vec<u8>>) {
        while let Some(bytes) = input.recv().await {
            let mut offset = 0;
            while offset < bytes.len() {
                let Ok(mut guard) = master.writable().await else {
                    return;
                };
                match guard.try_io(|fd| {
                    nix::unistd::write(fd.get_ref().as_fd(), &bytes[offset..])
                        .map_err(std::io::Error::from)
                }) {
                    Ok(Ok(n)) => offset += n,
                    Ok(Err(_)) => return,
                    Err(_would_block) => continue,
                }
            }
        }
    }
}

#[cfg(not(unix))]
pub use fallback::ChildPty;

/// Stand-ins so callers compile unchanged where there is no pty. Nothing
/// constructs them, because [`attach_pty`] never attaches one there.
#[cfg(not(unix))]
mod fallback {
    pub struct ChildPty;

    impl ChildPty {
        pub async fn start_io(self) -> ChildPtyIo {
            ChildPtyIo
        }
    }

    pub struct ChildPtyIo;

    impl ChildPtyIo {
        pub async fn finish(self) {}
    }
}
