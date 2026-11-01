//! Learning from niri where our windows actually are.
//!
//! Wayland tells a client nothing about where its windows end up, so satellite can only park
//! every toplevel at the origin of its output, and in X they all claim the same rectangle. An X
//! client that works out what the pointer is over by comparing window positions -- dragging a
//! tab out of one window and onto another, say -- cannot get a sensible answer out of that.
//!
//! niri reports window positions over its IPC socket, including which Wayland surface each
//! window belongs to, so when it is the compositor we are running under we can hand X the real
//! positions instead. Everything here is best-effort: without the socket, or with a compositor
//! that doesn't report surface ids, satellite behaves as it did before.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use serde_json::Value;

/// How long to wait before trying the compositor again after the stream drops.
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Where niri says a window is, in the logical coordinates of its output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Position {
    pub x: f64,
    pub y: f64,
}

/// What we know about one of niri's windows.
#[derive(Debug, Default)]
struct Window {
    /// Protocol id of the window's surface in our own connection, if the window is ours.
    surface_id: Option<u32>,
    /// Output the window is on, and where it is on it. Absent for a window that niri doesn't
    /// render anywhere, which is the case for one scrolled out of view.
    placement: Option<(String, Position)>,
    /// Whether the window is in the floating layout, which is the only one with positions that
    /// can be asked to change.
    is_floating: bool,
}

/// A connection to niri's event stream, tracking where our windows are.
pub(crate) struct Niri {
    /// Events from niri, read by a thread of their own.
    ///
    /// niri drops a client that falls behind its event buffer, and satellite's own loop waits on
    /// the X server often enough to do so. Reading on a thread keeps the compositor's view of us
    /// independent of what the X server is doing.
    events: Option<Receiver<Vec<u8>>>,
    /// Becomes readable when the reader has events waiting, so that the poll loop wakes for them.
    wake: UnixStream,
    /// The writing end, handed to each reader.
    waker: UnixStream,
    /// When to try connecting again, once the stream has dropped.
    retry_at: Option<Instant>,
    /// Our own process id, which is the one niri reports for every window we create.
    pid: i32,
    windows: HashMap<u64, Window>,
}

impl Niri {
    /// Subscribes to niri's event stream, if we are running under niri.
    pub(crate) fn connect() -> Option<Self> {
        // Not running under niri at all is the one case worth giving up on: everything else is
        // worth another try, since the compositor outlives any one connection to it.
        std::env::var_os("NIRI_SOCKET")?;

        let (wake, waker) = match UnixStream::pair() {
            Ok(pair) => pair,
            Err(e) => {
                warn!(
                    "Could not make a wakeup pipe ({e}); X windows will not be told where they are"
                );
                return None;
            }
        };
        if let Err(e) = wake.set_nonblocking(true) {
            warn!("Could not make the wakeup pipe nonblocking ({e})");
            return None;
        }

        let mut niri = Self {
            events: None,
            wake,
            waker,
            retry_at: Some(Instant::now()),
            pid: std::process::id() as i32,
            windows: HashMap::new(),
        };
        niri.reconnect();
        Some(niri)
    }

    /// A connection that reports one window of ours where the test says it is.
    #[cfg(test)]
    pub(crate) fn with_placement(surface_id: u32, output: &str, position: Position) -> Self {
        let (wake, waker) = UnixStream::pair().unwrap();
        let mut niri = Self {
            events: None,
            wake,
            waker,
            retry_at: None,
            pid: std::process::id() as i32,
            windows: HashMap::new(),
        };
        niri.windows.insert(
            1,
            Window {
                surface_id: Some(surface_id),
                placement: Some((output.to_owned(), position)),
                is_floating: false,
            },
        );
        niri
    }

    /// The fd that goes readable when there are events to take in.
    pub(crate) fn as_fd(&self) -> BorrowedFd<'_> {
        self.wake.as_fd()
    }

    /// Starts a reader for a fresh connection, if one is due.
    fn reconnect(&mut self) {
        if self.events.is_some() || self.retry_at.is_some_and(|at| at > Instant::now()) {
            return;
        }
        self.retry_at = None;

        let Some(path) = std::env::var_os("NIRI_SOCKET") else {
            return;
        };

        let mut stream = match UnixStream::connect(&path) {
            Ok(stream) => stream,
            Err(e) => {
                debug!("Could not connect to niri at {path:?} ({e}); trying again shortly");
                self.retry_at = Some(Instant::now() + RETRY_DELAY);
                return;
            }
        };

        // The reply to this is a line of its own, which parsing skips like any other event we
        // don't handle. Everything niri knows follows it, so the windows we knew about before
        // are described again and the map can start empty.
        if let Err(e) = stream.write_all(b"\"EventStream\"\n") {
            warn!("Could not subscribe to niri's event stream ({e}); trying again shortly");
            self.retry_at = Some(Instant::now() + RETRY_DELAY);
            return;
        }

        let Ok(waker) = self.waker.try_clone() else {
            warn!("Could not clone the wakeup pipe; X windows will not be told where they are");
            return;
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("niri events".to_owned())
            .spawn(move || read_events_forever(stream, tx, waker));
        if let Err(e) = spawned {
            warn!("Could not start a reader for niri's events ({e}); trying again shortly");
            self.retry_at = Some(Instant::now() + RETRY_DELAY);
            return;
        }

        info!("Following window positions from niri at {path:?}");
        self.windows.clear();
        self.events = Some(rx);
    }

    /// How long until the connection is worth trying again, if it is waiting to be.
    pub(crate) fn retry_in(&self) -> Option<Duration> {
        let at = self.retry_at?;
        Some(at.saturating_duration_since(Instant::now()))
    }

    /// Takes in whatever niri has sent us, returning true if any window moved.
    pub(crate) fn read_events(&mut self) -> bool {
        self.reconnect();

        // Whatever woke us has been noted; the events themselves come from the reader.
        let mut scratch = [0u8; 256];
        while let Ok(n) = (&self.wake).read(&mut scratch) {
            if n == 0 {
                break;
            }
        }

        let mut moved = false;
        loop {
            let Some(events) = &self.events else {
                return moved;
            };

            match events.try_recv() {
                Ok(line) => moved |= self.handle_event(&line),
                Err(TryRecvError::Empty) => return moved,
                Err(TryRecvError::Disconnected) => {
                    warn!("niri stopped sending events; reconnecting shortly");
                    self.events = None;
                    self.retry_at = Some(Instant::now() + RETRY_DELAY);
                    return moved;
                }
            }
        }
    }

    /// Takes in one event, returning true if it moved any window.
    fn handle_event(&mut self, line: &[u8]) -> bool {
        let event: Value = match serde_json::from_slice(line) {
            Ok(event) => event,
            Err(e) => {
                debug!("Ignoring unparseable message from niri ({e})");
                return false;
            }
        };

        let Some((name, data)) = event.as_object().and_then(|o| o.iter().next()) else {
            return false;
        };

        match name.as_str() {
            "WindowsChanged" => {
                // A full list: anything missing from it is gone.
                let windows = data.get("windows").and_then(Value::as_array);
                let Some(windows) = windows else {
                    return false;
                };

                let mut previous = std::mem::take(&mut self.windows);
                for window in windows {
                    let Some(id) = window.get("id").and_then(Value::as_u64) else {
                        continue;
                    };
                    let mut entry = previous.remove(&id).unwrap_or_default();
                    entry.surface_id = self.our_surface_id(window);
                    entry.is_floating = is_floating(window);
                    self.windows.insert(id, entry);
                }

                // Only the windows that went away could have moved anything.
                previous.values().any(|window| window.is_ours())
            }
            "WindowOpenedOrChanged" => {
                let Some(window) = data.get("window") else {
                    return false;
                };
                let Some(id) = window.get("id").and_then(Value::as_u64) else {
                    return false;
                };

                let surface_id = self.our_surface_id(window);
                let floating = is_floating(window);
                let entry = self.windows.entry(id).or_default();
                let was_ours = entry.surface_id.is_some();
                entry.surface_id = surface_id;
                entry.is_floating = floating;

                // A window of ours that just appeared has no placement yet; the geometry event
                // that follows is what moves it.
                was_ours != entry.surface_id.is_some()
            }
            "WindowClosed" => {
                let Some(id) = data.get("id").and_then(Value::as_u64) else {
                    return false;
                };
                self.windows
                    .remove(&id)
                    .is_some_and(|window| window.is_ours())
            }
            "WindowGeometriesChanged" => {
                let changes = data.get("changes").and_then(Value::as_array);
                let Some(changes) = changes else {
                    return false;
                };

                let mut moved = false;
                let mut seen = Vec::with_capacity(changes.len());
                for change in changes {
                    let Some(id) = change.get("id").and_then(Value::as_u64) else {
                        continue;
                    };
                    seen.push(id);

                    let output = change.get("output").and_then(Value::as_str);
                    let x = change.get("x").and_then(Value::as_f64);
                    let y = change.get("y").and_then(Value::as_f64);
                    let Some(((output, x), y)) = output.zip(x).zip(y) else {
                        continue;
                    };

                    let placement = Some((output.to_owned(), Position { x, y }));
                    let entry = self.windows.entry(id).or_default();
                    if entry.placement != placement {
                        entry.placement = placement;
                        moved |= entry.is_ours();
                    }
                }

                // niri sends a geometry for every window it renders, so a window of ours that is
                // missing from a full update is one that has scrolled out of view. Its last known
                // position is as good as any, so it keeps it.
                moved
            }
            _ => false,
        }
    }

    /// The protocol id of the window's surface, if the window is one that we created.
    fn our_surface_id(&self, window: &Value) -> Option<u32> {
        if window.get("pid").and_then(Value::as_i64) != Some(i64::from(self.pid)) {
            return None;
        }

        // Compositors that don't report this leave us no way to tell our windows apart.
        let surface_id = window.get("wl_surface_id").and_then(Value::as_u64);
        if surface_id.is_none() {
            debug!("niri does not report surface ids; X windows will not be told where they are");
        }
        surface_id.and_then(|id| u32::try_from(id).ok())
    }

    /// Where the window belonging to one of our surfaces is, as an output name and a position on
    /// that output.
    pub(crate) fn placement_of(&self, surface_id: u32) -> Option<(&str, Position)> {
        self.windows
            .values()
            .find(|window| window.surface_id == Some(surface_id))
            .and_then(|window| window.placement.as_ref())
            .map(|(output, position)| (output.as_str(), *position))
    }

    /// The window one of our surfaces belongs to, if it is a floating one whose position the
    /// compositor will take requests about.
    pub(crate) fn floating_window(&self, surface_id: u32) -> Option<u64> {
        self.windows.iter().find_map(|(id, window)| {
            (window.surface_id == Some(surface_id) && window.is_floating).then_some(*id)
        })
    }

    /// Asks niri to move a floating window by the given logical distance.
    ///
    /// Wayland gives a client no way to place its own windows, so a client that wants to move one
    /// -- an X client dragging its own window around, say -- can only be served by asking the
    /// compositor to do it. The move is relative, which saves having to know where niri's working
    /// area starts.
    pub(crate) fn move_floating_window(&self, id: u64, dx: f64, dy: f64) {
        let action = format!(
            "{{\"Action\":{{\"MoveFloatingWindow\":\
             {{\"id\":{id},\"x\":{{\"AdjustFixed\":{dx}}},\"y\":{{\"AdjustFixed\":{dy}}}}}}}}}"
        );

        // A niri request takes a connection of its own, and the reply is of no use to us: there is
        // nothing to do about a move the compositor declines.
        if let Err(e) = self.send_request(&action) {
            debug!("Could not ask niri to move window {id} ({e})");
        }
    }

    fn send_request(&self, request: &str) -> std::io::Result<()> {
        let path = std::env::var_os("NIRI_SOCKET")
            .ok_or_else(|| std::io::Error::other("NIRI_SOCKET is unset"))?;
        let mut stream = UnixStream::connect(path)?;
        stream.write_all(request.as_bytes())?;
        stream.write_all(b"\n")?;
        Ok(())
    }
}

/// Reads events until the connection ends, handing each line to the main thread.
fn read_events_forever(
    stream: UnixStream,
    tx: std::sync::mpsc::Sender<Vec<u8>>,
    mut waker: UnixStream,
) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
                    line.pop();
                }
                if tx.send(std::mem::take(&mut line)).is_err() {
                    break;
                }
                // Any byte will do: the main loop only needs to know to come and look.
                if let Err(e) = waker.write_all(b"n") {
                    if e.kind() != ErrorKind::WouldBlock {
                        break;
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Whether niri reports this window as floating.
fn is_floating(window: &Value) -> bool {
    window
        .get("is_floating")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

impl Window {
    fn is_ours(&self) -> bool {
        self.surface_id.is_some()
    }
}
