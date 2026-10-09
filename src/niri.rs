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
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;

use log::{debug, info, warn};
use serde_json::Value;

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
    stream: UnixStream,
    /// Bytes of an event that hasn't arrived in full yet.
    pending: Vec<u8>,
    /// Our own process id, which is the one niri reports for every window we create.
    pid: i32,
    windows: HashMap<u64, Window>,
    /// Set once niri stops talking to us, after which we leave X positions alone.
    done: bool,
}

impl Niri {
    /// Subscribes to niri's event stream, if we are running under niri.
    pub(crate) fn connect() -> Option<Self> {
        let path = std::env::var_os("NIRI_SOCKET")?;

        let mut stream = match UnixStream::connect(&path) {
            Ok(stream) => stream,
            Err(e) => {
                warn!(
                    "Could not connect to niri at {path:?} ({e}); X windows will not be told where they are"
                );
                return None;
            }
        };

        // The reply to this is a line of its own, which parsing skips like any other event we
        // don't handle.
        if let Err(e) = stream.write_all(b"\"EventStream\"\n") {
            warn!("Could not subscribe to niri's event stream ({e})");
            return None;
        }

        if let Err(e) = stream.set_nonblocking(true) {
            warn!("Could not make the niri socket nonblocking ({e})");
            return None;
        }

        info!("Following window positions from niri at {path:?}");
        Some(Self {
            stream,
            pending: Vec::new(),
            pid: std::process::id() as i32,
            windows: HashMap::new(),
            done: false,
        })
    }

    pub(crate) fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }

    /// Reads whatever niri has sent us, returning true if any window moved.
    pub(crate) fn read_events(&mut self) -> bool {
        if self.done {
            return false;
        }

        let mut buf = [0u8; 8192];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    warn!("niri closed its event stream; X windows will keep their positions");
                    self.done = true;
                    break;
                }
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    warn!("Reading from niri failed ({e}); X windows will keep their positions");
                    self.done = true;
                    break;
                }
            }
        }

        let mut moved = false;
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            moved |= self.handle_event(&line[..end]);
        }
        moved
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

    /// The window one of our surfaces belongs to, if it is a floating one that niri is placing,
    /// together with where it is now.
    pub(crate) fn floating_window(&self, surface_id: u32) -> Option<(u64, Position)> {
        self.windows.iter().find_map(|(id, window)| {
            if window.surface_id != Some(surface_id) || !window.is_floating {
                return None;
            }
            let (_, position) = window.placement.as_ref()?;
            Some((*id, *position))
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
