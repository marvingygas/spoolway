//! The terminal primitives every full-screen view shares.
//!
//! `spoolway queue` was the first code in the project that reads a keystroke,
//! and `spoolway eval`'s bare screen is the second. Both read one byte at a
//! time off stdin rather than reaching for a terminal crate, because the same
//! loop has to run identically whether stdin is a real tty in raw mode or a
//! pipe an end-to-end suite is scripting — see [`read_key`]. Sharing that
//! reader, and the small drawing helpers built on top of it, is what keeps a
//! second screen from re-deciding any of that for itself.
//!
//! Each screen still owns its own frame layout, its own modes and its own key
//! handling — only what is generic across any of them lives here. [`shell`]
//! is what bare `spoolway` opens: the same screens, one per tab, under one
//! strip, one terminal guard and one quit — and [`dispatcher`] is the
//! `spoolway dispatch` child its dispatch tab starts and stops.

pub(crate) mod dispatcher;
pub(crate) mod frame_writer;
pub(crate) mod overrides;
pub(crate) mod pane;
pub(crate) mod shell;

/// One key a screen reads, decoded from however many bytes it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Enter,
    Tab,
    Esc,
    /// `0x7f` (`DEL`), the byte a real terminal sends for the backspace key.
    /// Decoded as its own key rather than `Char('\u{7f}')` so a screen with a
    /// text box — `spoolway queue`'s own filter is the first — can delete a
    /// character without a mode that reads literal characters mistaking it
    /// for one typed on purpose.
    Backspace,
    Char(char),
}

/// How long [`read_escape`] waits for a second byte before deciding a lone
/// `Esc` was the whole thing. Long enough that a real `\x1b[X` sequence,
/// whose bytes a terminal sends back to back, is never mistaken for two
/// separate keys; short enough that nobody perceives the wait after an
/// actual Escape press.
const ESCAPE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

/// A reader that can say whether its next byte is already there, so
/// [`read_escape`] can tell "wait, more of this sequence is coming" apart
/// from "that was the whole key". Every in-memory reader has all its bytes
/// queued up front — a pipe's write already landed before an end-to-end
/// suite's `spoolway` process ever starts reading — so the default answer is
/// an unconditional `true`, which reproduces `read_key`'s old always-block
/// behaviour exactly for anything but a live terminal. Only [`RawStdin`]
/// can make a caller wait on a byte that may never come — a person pressing
/// Escape alone sends just the one 0x1b, with nothing after it — so it is the
/// only impl that actually asks the kernel.
pub(crate) trait PollableRead: std::io::Read {
    fn byte_pending(&self, timeout: std::time::Duration) -> bool;

    /// [`PollableRead::byte_pending`], cut short when `wake` is signalled.
    /// `true` only when a byte is pending; a signal alone returns `false`
    /// early, with the signal drained, so the caller can look at whatever it
    /// was signalled about and wait again. An in-memory reader has no
    /// descriptor to poll beside its own and ignores `wake`.
    fn wait_pending(&self, timeout: std::time::Duration, wake: Option<&Wake>) -> bool {
        let _ = wake;
        self.byte_pending(timeout)
    }

    /// Throw away whatever was typed and not yet read — keys pressed while
    /// the screen was busy and could not answer them, which a person meant
    /// for a screen that was not yet drawn. An in-memory reader holds a
    /// script, not keys typed early, and keeps every byte of it.
    fn discard_typed(&mut self) {}
}

impl PollableRead for std::io::Cursor<Vec<u8>> {
    fn byte_pending(&self, _timeout: std::time::Duration) -> bool {
        true
    }
}

/// Stdin, read one byte at a time straight off the descriptor with no
/// buffering layer of its own.
///
/// [`std::io::Stdin`] keeps an internal `BufReader` (8KiB by default) that a
/// single-byte `read` can still fill completely: a real terminal delivers an
/// arrow key's `\x1b[X` as one burst, so the very first `read` of the
/// sequence can drain all three bytes off the kernel into that buffer at
/// once, and every read after just slices it out of userspace — the kernel's
/// fd goes empty immediately. `PollableRead::byte_pending` polls that same
/// kernel fd, so against a buffering `Stdin` the two disagree: bytes it
/// considers "pending" can already be sitting unseen in Stdin's own buffer,
/// and `poll` reports nothing left. That is exactly what broke arrow keys —
/// `read_escape`'s `byte_pending` check saw an empty kernel fd for a burst
/// `Stdin` had already swallowed, and gave up as a bare `Esc`. Going straight
/// to `libc::read` keeps this reader's view of "what's pending" identical to
/// the kernel's, at the one-byte-at-a-time granularity every caller here
/// already reads at, so there is nothing left for `poll` to be blind to.
pub(crate) struct RawStdin;

impl std::io::Read for RawStdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // SAFETY: `buf` is a valid, appropriately sized buffer for the
        // duration of this call, and stdin's descriptor is open for the
        // life of the process.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n as usize)
    }
}

impl PollableRead for RawStdin {
    fn byte_pending(&self, timeout: std::time::Duration) -> bool {
        fd_has_byte_within(libc::STDIN_FILENO, timeout)
    }

    #[cfg(unix)]
    fn wait_pending(&self, timeout: std::time::Duration, wake: Option<&Wake>) -> bool {
        let Some(wake) = wake else {
            return self.byte_pending(timeout);
        };
        wait_input_or_wake(libc::STDIN_FILENO, wake, timeout)
    }

    fn discard_typed(&mut self) {
        // SAFETY: stdin's descriptor is open for the life of the process;
        // on anything but a terminal this fails harmlessly and discards
        // nothing, which is the in-memory readers' answer too.
        #[cfg(unix)]
        unsafe {
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
        }
    }
}

/// Whether `input` has a byte within `timeout`, giving up early, with
/// `false`, when `wake` is signalled first. The signal is drained either
/// way, so the next wait starts clean. A free function over the descriptor,
/// like [`fd_has_byte_within`], so a test can aim it at a pipe.
#[cfg(unix)]
fn wait_input_or_wake(
    input: std::os::unix::io::RawFd,
    wake: &Wake,
    timeout: std::time::Duration,
) -> bool {
    let ready = poll_ready(&[input, wake.read_fd()], timeout);
    wake.drain();
    ready.first().copied().unwrap_or(false)
}

/// A signal one thread sends another that is waiting in [`poll_ready`]: a
/// pipe whose read end is polled beside stdin, written to by
/// [`Wake::notify`].
///
/// The dispatch tab waits on this, rather than polling for a landed
/// reading on a short timer. A signal is a hint, never the only way to learn: the tab
/// still wakes once a [`crate::status::POLL`], so a missed signal costs at
/// most that second.
#[cfg(unix)]
pub(crate) struct Wake {
    read: std::os::unix::io::RawFd,
    write: std::os::unix::io::RawFd,
}

#[cfg(unix)]
impl Wake {
    /// `None` when the pipe cannot be made, which leaves the caller on its
    /// plain timeout.
    pub(crate) fn new() -> Option<Self> {
        // Non-blocking so neither a full pipe stalls `notify` nor an empty
        // one stalls `drain`; close-on-exec so a lane's child process does
        // not inherit them. Both are set as the pipe is made where the
        // system allows, since a process another thread spawns between
        // `pipe` and a later `fcntl` would inherit both ends.
        let mut fds = [0i32; 2];
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: `fds` is a well-formed two-element buffer `pipe2` fills in.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return None;
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            // SAFETY: `fds` is a well-formed two-element buffer `pipe` fills in.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return None;
            }
            for fd in fds {
                // SAFETY: `fd` was just opened above and is still owned here.
                unsafe {
                    libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
            }
        }
        Some(Self {
            read: fds[0],
            write: fds[1],
        })
    }

    /// The descriptor [`poll_ready`] watches.
    pub(crate) fn read_fd(&self) -> std::os::unix::io::RawFd {
        self.read
    }

    /// Signal the waiter. A full pipe already holds a signal, so a failed
    /// write loses nothing.
    pub(crate) fn notify(&self) {
        // SAFETY: `write` is this value's own open descriptor and the buffer
        // is one valid byte.
        unsafe {
            libc::write(self.write, [1u8].as_ptr().cast(), 1);
        }
    }

    /// Forget every signal sent so far.
    pub(crate) fn drain(&self) {
        let mut buf = [0u8; 64];
        // SAFETY: `read` is this value's own open descriptor and `buf` is
        // valid for its length.
        while unsafe { libc::read(self.read, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
    }
}

#[cfg(unix)]
impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: both descriptors belong to this value and are closed once.
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

/// The non-Unix half of [`Wake`]: nothing waits on a descriptor there, so
/// [`Wake::new`] never returns one and the tab keeps its plain timeout.
#[cfg(not(unix))]
pub(crate) struct Wake;

#[cfg(not(unix))]
impl Wake {
    pub(crate) fn new() -> Option<Self> {
        None
    }

    pub(crate) fn notify(&self) {}
}

/// Whether `fd` has something for the next read to see within `timeout`,
/// without consuming it — either a real byte (`POLLIN`) or the far end going
/// away (`POLLHUP`/`POLLERR`), since a closed pipe makes the next `read`
/// return `0` just as promptly as a byte would satisfy it, and either one
/// means the wait is over. A free function, rather than inlined into the
/// `Stdin` impl above, so a test can point it at an ordinary pipe it
/// controls the timing of — exercising the exact kernel call
/// [`PollableRead::byte_pending`] makes on stdin without needing a real
/// terminal or hijacking the test process's own stdin.
#[cfg(unix)]
fn fd_has_byte_within(fd: std::os::unix::io::RawFd, timeout: std::time::Duration) -> bool {
    poll_ready(&[fd], timeout).first().copied().unwrap_or(false)
}

/// Polls every descriptor in `fds` together and says which of them had
/// something waiting within `timeout` — a byte on stdin, a [`DirWatch`]
/// event — in one kernel call rather than one poll per descriptor checked in
/// turn. This is the join `commands::dispatch`'s wait loop makes: a
/// keystroke and a file landing in the queue or commands directory answer
/// the exact same `poll`, at the same moment, so neither has to wait behind
/// the other's own timeout.
///
/// Ready means a real event (`POLLIN`) or the far end going away
/// (`POLLHUP`/`POLLERR`) — see [`fd_has_byte_within`]'s own doc on why a
/// closed descriptor counts as ready too.
#[cfg(unix)]
pub(crate) fn poll_ready(
    fds: &[std::os::unix::io::RawFd],
    timeout: std::time::Duration,
) -> Vec<bool> {
    let mut pollfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `pollfds` holds one well-formed `pollfd` per descriptor, each
    // kept alive by its caller for the duration of this call.
    let n = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, ms) };
    // POSIX leaves every `revents` unspecified when `poll` itself returns
    // -1 (an interrupting signal, most often here — see `stop::
    // catch_interrupt`) — trusting them without this guard would read
    // whatever `revents` happened to be initialised to as a real answer.
    // `n == 0` (the timeout ran out with nothing ready) already reads every
    // `revents` back as zero on its own, so this only ever changes the
    // `n < 0` case.
    if n < 0 {
        return vec![false; fds.len()];
    }
    pollfds
        .iter()
        .map(|pfd| pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
        .collect()
}

/// Watches a directory for anything that used to be learned only by rereading
/// it — see [`crate::dispatch::PROBE_INTERVAL`]'s own doc. The dispatcher
/// opens one on the commands directory, and one on the directory a request to
/// reload lands in — see [`crate::reload`].
/// Built once, outside `commands::dispatch`'s own loop, and polled alongside
/// stdin every time that loop waits — see [`poll_ready`].
///
/// Backed by [`inotify(7)`](https://man7.org/linux/man-pages/man7/inotify.7.html),
/// a Linux-only kernel facility with no portable equivalent — see
/// [`open_dir_watch`]'s own split for what every other target falls back to.
#[cfg(target_os = "linux")]
pub(crate) struct DirWatch {
    fd: std::os::unix::io::RawFd,
}

#[cfg(target_os = "linux")]
impl DirWatch {
    /// `None` on any failure to open the instance or watch the directory —
    /// the caller falls back to the plain interval wait it already has,
    /// rather than fail a whole run over a watch it can live without.
    fn new(commands_dir: &std::path::Path) -> Option<Self> {
        use std::os::unix::ffi::OsStrExt;

        // A new file (`IN_CREATE`, the wrapper's log file created before it
        // is spawned, or a `.pid`/`.exit`/`.pane` file's first write), one
        // written and closed (`IN_CLOSE_WRITE` — those same `.pid`, `.exit`
        // and `.pane` files, each a plain open/write/close), one replaced in
        // place (`IN_MOVED_TO`, how `command_step::Runs::roll_log_aside`
        // renames the last run's log out of the way) or removed
        // (`IN_DELETE`, a `.pane` file forgotten once its pane closes, or a
        // task's run files cleaned up on archive) — everything either side
        // of this watch actually does to the commands directory. A request to
        // reload is one more new file (`IN_CREATE`), and clearing it one more
        // `IN_DELETE`, in the other directory this is opened on.
        const MASK: u32 =
            libc::IN_CREATE | libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_DELETE;

        // SAFETY: no arguments borrow anything. `IN_NONBLOCK` keeps a read
        // issued only after `poll` says ready from ever blocking on one that
        // lost the race with a concurrent drain.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK) };
        if fd < 0 {
            return None;
        }
        let Ok(path) = std::ffi::CString::new(commands_dir.as_os_str().as_bytes()) else {
            // SAFETY: `fd` was opened by this same call, above, and nothing
            // else holds it yet.
            unsafe { libc::close(fd) };
            return None;
        };
        // SAFETY: `fd` is the instance opened above; `path` is a valid
        // NUL-terminated buffer for the duration of this call.
        let wd = unsafe { libc::inotify_add_watch(fd, path.as_ptr(), MASK) };
        if wd < 0 {
            // SAFETY: as above.
            unsafe { libc::close(fd) };
            return None;
        }
        Some(Self { fd })
    }

    pub(crate) fn fd(&self) -> std::os::unix::io::RawFd {
        self.fd
    }

    /// Drain every event `poll_ready` just found waiting on [`Self::fd`],
    /// and say whether any landed — a single run can write its `.pid`,
    /// `.exit` and `.pane` files and roll its log aside in one go, and the
    /// caller only needs to know it should act, not how many times.
    pub(crate) fn drain(&self) -> bool {
        // Only whether `read(2)` returned anything is checked below, so no
        // `inotify_event` is ever parsed out of `buf` — but it still has to
        // be at least one event long, and `[u64; _]` over `[u8; _]` costs
        // nothing to keep, so it stays.
        let mut buf = [0u64; 512];
        let cap = std::mem::size_of_val(&buf);
        let mut any = false;
        loop {
            // SAFETY: `buf` is a valid, appropriately sized and aligned
            // buffer for the duration of this call, and `self.fd` is open
            // for the life of `self`.
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), cap) };
            if n <= 0 {
                break;
            }
            any = true;
        }
        any
    }
}

#[cfg(target_os = "linux")]
impl Drop for DirWatch {
    fn drop(&mut self) {
        // SAFETY: `self.fd` was opened by `Self::new` and nothing else holds
        // it.
        unsafe { libc::close(self.fd) };
    }
}

/// The non-Linux half of [`DirWatch`]: [`Self::new`] never returns anything
/// to watch, since `inotify` is Linux-only — a target that lands here falls
/// back to `commands::dispatch`'s plain interval wait alone, exactly as it
/// behaved before this task.
#[cfg(not(target_os = "linux"))]
pub(crate) struct DirWatch;

#[cfg(not(target_os = "linux"))]
impl DirWatch {
    fn new(_commands_dir: &std::path::Path) -> Option<Self> {
        None
    }

    pub(crate) fn fd(&self) -> std::os::unix::io::RawFd {
        unreachable!("DirWatch::new never returns Some off this target")
    }

    pub(crate) fn drain(&self) -> bool {
        false
    }
}

/// Open a [`DirWatch`] on `commands_dir`, or `None` where there is nothing to
/// watch with — see [`DirWatch::new`] on each target.
pub(crate) fn open_dir_watch(commands_dir: &std::path::Path) -> Option<DirWatch> {
    DirWatch::new(commands_dir)
}

/// Read one key off `input`, blocking until it can. `None` at end of input —
/// the tty going away, or a scripted pipe running out of bytes — which is
/// what lets a screen degrade rather than hang or crash where there is no
/// terminal to drive.
pub(crate) fn read_key(input: &mut impl PollableRead) -> Option<Key> {
    let mut byte = [0u8; 1];
    match input.read(&mut byte) {
        Ok(0) | Err(_) => return None,
        Ok(_) => {}
    }
    Some(match byte[0] {
        0x1b => read_escape(input).unwrap_or(Key::Esc),
        b'\r' | b'\n' => Key::Enter,
        b'\t' => Key::Tab,
        0x7f => Key::Backspace,
        c => Key::Char(c as char),
    })
}

/// The rest of a `\x1b[X` arrow sequence, or a `\x1b[N~` page sequence, once
/// the leading escape has already been read. `None` for anything else escape
/// can start — including the escape simply running out at end of input —
/// which `read_key` reads back as a bare `Esc`.
///
/// The read that used to sit here first, unconditionally, is exactly what
/// froze the screen on a lone Escape press: on a real tty that read blocks
/// until a byte turns up, and no second byte is ever coming behind a bare
/// `Esc`. Worse, the next key the person actually meant — typed once the
/// screen looked frozen — landed in that same blocked read and was thrown
/// away here as "not `[`", never reaching the caller as its own keystroke.
/// Waiting on [`PollableRead::byte_pending`] first turns that indefinite
/// block into a bounded one: nothing arrives inside `ESCAPE_TIMEOUT`, this
/// gives up and reads back as a bare `Esc`, and the very next keystroke is
/// still sitting unread for the next call to pick up.
pub(crate) fn read_escape(input: &mut impl PollableRead) -> Option<Key> {
    if !input.byte_pending(ESCAPE_TIMEOUT) {
        return None;
    }
    let mut byte = [0u8; 1];
    if input.read(&mut byte).unwrap_or(0) == 0 || byte[0] != b'[' {
        return None;
    }
    if input.read(&mut byte).unwrap_or(0) == 0 {
        return None;
    }
    match byte[0] {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(Key::Right),
        b'D' => Some(Key::Left),
        // Page up (`\x1b[5~`) and page down (`\x1b[6~`) carry one more byte
        // than an arrow does — the tilde that closes a numbered CSI
        // sequence — so it has to be read and checked before the digit
        // ahead of it can be trusted to mean either key.
        digit @ (b'5' | b'6') => {
            if input.read(&mut byte).unwrap_or(0) == 0 || byte[0] != b'~' {
                return None;
            }
            Some(if digit == b'5' {
                Key::PageUp
            } else {
                Key::PageDown
            })
        }
        _ => None,
    }
}

/// The fallback pane size every redrawing screen already falls back to for
/// its own width alone (`shell::FALLBACK_WIDTH`) or leaves as `None` for its
/// own height, now given a height too because [`frame_writer::FrameWriter`]
/// takes both as one argument it cannot leave unset. Only reached with no
/// terminal to measure at all — a piped run, or `cargo test`'s own stdout —
/// so nobody ever sees the width or height chosen here.
const FALLBACK_PANE_SIZE: (usize, usize) = (100, 30);

/// The columns bare `spoolway` keeps blank on each side of every tab.
/// [`drawing_area`] takes them off the width, and
/// [`frame_writer::FrameWriter`] writes the left one in front of every row,
/// so a row as wide as the area still ends one column short of the
/// terminal's last.
pub(crate) const MARGIN: usize = 1;

/// The rows [`drawing_area`] takes off the terminal's height. Each tab
/// already holds back the area's last row for the cursor its last newline
/// leaves there, so with this one too two blank rows sit under the key line.
const MARGIN_ROWS: usize = 1;

/// The terminal's own size in columns and rows, or `None` with no terminal
/// to measure. The one place spoolway reads it: [`pane_size`] and
/// [`drawing_area`] are both built on this, so a screen switch and the frame
/// it draws next always measure the same terminal rather than two callers
/// racing a resize between them.
///
/// A test build never reads the real terminal: `cargo test` inherits
/// whatever pane runs it, and a 14-row pane once failed board tests that
/// pass with no terminal at all. It answers `None`, as if piped, unless a
/// test has named a size with [`test_terminal`].
fn terminal() -> Option<(usize, usize)> {
    #[cfg(test)]
    {
        TEST_TERMINAL.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        terminal_size::terminal_size().map(|(w, h)| (w.0 as usize, h.0 as usize))
    }
}

#[cfg(test)]
thread_local! {
    /// The terminal a test on this thread draws to — see [`test_terminal`].
    /// Thread-local for the same reason as `shell`'s own `HOSTED`: tests run
    /// side by side, and one's size must never reach another's frame.
    static TEST_TERMINAL: std::cell::Cell<Option<(usize, usize)>> =
        const { std::cell::Cell::new(None) };
}

/// Makes [`terminal`] answer `size` on this thread for as long as the guard
/// lives, so a test can draw a tab at a terminal size it names.
#[cfg(test)]
pub(crate) fn test_terminal(size: (usize, usize)) -> TestTerminal {
    TEST_TERMINAL.with(|cell| cell.set(Some(size)));
    TestTerminal
}

/// Clears [`test_terminal`]'s size on drop, so a failed assertion cannot
/// leave it behind for the next test this thread runs.
#[cfg(test)]
pub(crate) struct TestTerminal;

#[cfg(test)]
impl Drop for TestTerminal {
    fn drop(&mut self) {
        TEST_TERMINAL.with(|cell| cell.set(None));
    }
}

/// The terminal's own size in columns and rows, for every redrawing screen
/// to pass to [`frame_writer::FrameWriter::write_frame`] as the pane it
/// drew for — the whole terminal, margin and all, since that is what the
/// writer paints onto. A tab lays itself out against [`drawing_area`]
/// instead.
pub(crate) fn pane_size() -> (usize, usize) {
    terminal().unwrap_or(FALLBACK_PANE_SIZE)
}

/// The columns and rows every tab of bare `spoolway` lays itself out
/// against: the terminal less [`MARGIN`] on each side and [`MARGIN_ROWS`]
/// at the foot. The tab strip, every tab's frame, its key line and every
/// popup centred on that frame are all measured here, so the margin is
/// decided in this one place rather than subtracted again by each tab.
///
/// With no shell hosting a screen — one a unit test draws on its own, say
/// — this is the whole terminal, so nothing drawn outside bare `spoolway`
/// gains a margin. `None` with no
/// terminal to measure; see [`terminal`].
pub(crate) fn drawing_area() -> Option<(usize, usize)> {
    let (width, height) = terminal()?;
    Some(match shell::hosted() {
        Some(_) => (
            width.saturating_sub(2 * MARGIN),
            height.saturating_sub(MARGIN_ROWS),
        ),
        None => (width, height),
    })
}

/// Pad `s` to exactly `width` visible characters, or cut it to fit — a screen
/// row has to end at the same column every time for the border on its right
/// to line up, whatever the content of any one line.
pub(crate) fn pad_to(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.chars().take(width).collect()
    } else {
        let mut out = s.to_string();
        out.push_str(&" ".repeat(width - len));
        out
    }
}

/// The key line every full-screen view draws at its own foot: an ordered
/// list of key/label pairs, dimmed the way the board dims its own.
///
/// The board at `src/status/mod.rs` spelled this first; the queue screen and
/// the jobs screen each grew a second spelling of it as a string literal.
/// This is the one place left to build it, reading [`crate::status::DIM`]
/// and [`crate::status::GUTTER`] rather than writing either out again, so a
/// screen that switches over stops deciding its own spelling.
///
/// Called by the queue screen's own `footer`, the job popups'
/// `NewJobWalk::footer`, `spoolway eval`'s own `footer` in `src/eval.rs`,
/// and [`shell`]'s own `message_frame`. The board's own key line in
/// `src/status/mod.rs` goes through [`key_hint_lit`], which draws this same
/// line with one pair taken out of the dim.
pub(crate) fn key_hint(pairs: &[(&str, &str)]) -> String {
    hint(&keys(pairs))
}

/// [`key_hint`], with the pair whose key is `lit` drawn in normal text
/// while every other pair stays dim — for a key that shows only when there
/// is something to do, and would be easy to miss dimmed among the rest.
///
/// The pair closes the line's dim before itself and opens it again after,
/// so the codes travel inside the pair: [`key_rows_to`] breaks only on the
/// gutter between pairs, and a pair moved to a row of its own keeps them.
/// No pair named `lit` draws the same line [`key_hint`] does.
pub(crate) fn key_hint_lit(pairs: &[(&str, &str)], lit: &str) -> String {
    let (dim, reset) = (crate::status::DIM, crate::status::RESET);
    let row = pairs
        .iter()
        .map(|&pair| match pair.0 == lit {
            true => format!("{reset}{}{dim}", keys(&[pair])),
            false => keys(&[pair]),
        })
        .collect::<Vec<_>>()
        .join(crate::status::GUTTER);
    hint(&row)
}

/// A key row already built — [`keys`], with words of its own between pairs
/// where a row needs them — dimmed and indented the way [`key_hint`] draws
/// its own, so the two read alike under a frame.
pub(crate) fn hint(row: &str) -> String {
    format!(
        "{dim} {row}{reset}",
        dim = crate::status::DIM,
        reset = crate::status::RESET
    )
}

/// The same `[key] label` pairs [`key_hint`] draws, with no colour and no
/// leading space — the key row a [`panel`] carries inside its own box.
///
/// Plain because [`overlay`] lays a panel over a frame one character at a
/// time: a colour code there would be counted as columns it does not take,
/// and the panel's right border would land short of where the box says.
/// Shared with `key_hint` so a key named in a popup and the same key named
/// on the row under the frame read exactly alike.
pub(crate) fn keys(pairs: &[(&str, &str)]) -> String {
    let mut line = String::new();
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            line.push_str(crate::status::GUTTER);
        }
        line.push_str(&format!("[{key}] {label}"));
    }
    line
}

/// A key line — [`key_hint`]'s or [`hint`]'s — broken between its key
/// pairs into rows no wider than `width`, for a tab to draw under its frame.
///
/// Left to the terminal, a line wider than the pane wraps mid-word, and the
/// rest starts in column 0, inside the margin [`MARGIN`] keeps. Each row
/// here starts with the line's own lead — its colour and the spaces before
/// its first key — and ends with its reset, so every row reads like the
/// first. A pair wider than `width` on its own still gets a row of its own.
pub(crate) fn key_rows_to(line: &str, width: usize) -> Vec<String> {
    let reset = crate::status::RESET;
    let (body, closed) = match line.strip_suffix(reset) {
        Some(body) => (body, reset),
        None => (line, ""),
    };
    // The lead is everything up to the first visible character that is not
    // a space: colour codes take no column, so they are skipped whole. A
    // reset ends it too: it belongs to a pair drawn out of the line's own
    // colour — see [`key_hint_lit`] — and a lead that carried it would
    // start every row undimmed.
    let mut lead_len = body.len();
    let mut chars = body.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '\x1b' if body[at..].starts_with(reset) => {
                lead_len = at;
                break;
            }
            '\x1b' => {
                chars.by_ref().find(|(_, c)| c.is_ascii_alphabetic());
            }
            ' ' => {}
            _ => {
                lead_len = at;
                break;
            }
        }
    }
    let (lead, pairs) = body.split_at(lead_len);
    let lead_width = crate::status::strip_ansi(lead).chars().count();
    let gutter = crate::status::GUTTER;

    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = lead_width;
    for pair in pairs.split(gutter) {
        let pair_width = crate::status::strip_ansi(pair).chars().count();
        if !current.is_empty() && current_width + gutter.len() + pair_width > width {
            rows.push(format!("{lead}{}{closed}", std::mem::take(&mut current)));
            current_width = lead_width;
        }
        if !current.is_empty() {
            current.push_str(gutter);
            current_width += gutter.len();
        }
        current.push_str(pair);
        current_width += pair_width;
    }
    rows.push(format!("{lead}{current}{closed}"));
    rows
}

/// [`key_rows_to`] at [`drawing_area`]'s width — the rows a tab draws its
/// key line in. With no terminal to measure the line stays one row: there
/// is no edge for it to wrap at.
pub(crate) fn key_rows(line: &str) -> Vec<String> {
    match drawing_area() {
        Some((width, _)) => key_rows_to(line, width),
        None => vec![line.to_string()],
    }
}

/// The four corners of every box spoolway draws, rounded: popups, printed
/// panels and every tab's frame alike.
///
/// Held here, once, because the code that draws a frame and the code that
/// finds one by its corners — `stretch` in the queue screen blanks a bottom
/// border by matching them — have to agree on the same characters. A corner
/// typed by hand in one of them is how a frame stops being recognised.
///
/// The `┬` and `┴` joins between two panes stay square: box drawing has no
/// rounded join.
pub(crate) mod corner {
    pub(crate) const TOP_LEFT: char = '╭';
    pub(crate) const TOP_RIGHT: char = '╮';
    pub(crate) const BOTTOM_LEFT: char = '╰';
    pub(crate) const BOTTOM_RIGHT: char = '╯';
}

/// A framed box — a title and whatever lines fill it — to be drawn over a
/// screen's own frame.
///
/// Sits on top of the layout rather than pushed into it: a list inserted
/// between two rows moves everything below it down, and a person reading a
/// row loses the one they had the cursor on.
///
/// Shared by [`panel`], which appends its own key line as one more row after
/// a blank spacer, by [`ask`] and [`pick`], which append a popup's answers
/// and their key row, and by callers whose body needs no key row after it at
/// all.
pub(crate) fn boxed(title: &str, body: &[String]) -> Vec<String> {
    use corner::{BOTTOM_LEFT, BOTTOM_RIGHT, TOP_LEFT, TOP_RIGHT};
    let head = format!("─ {title} ");
    let widest = body
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let inner = (widest + 4).max(head.chars().count() + 2);

    let mut out = vec![format!(
        "{TOP_LEFT}{head}{}{TOP_RIGHT}",
        "─".repeat(inner - head.chars().count())
    )];
    for line in body {
        out.push(format!("│{}│", pad_to(&format!("  {line}"), inner)));
    }
    out.push(format!("{BOTTOM_LEFT}{}{BOTTOM_RIGHT}", "─".repeat(inner)));
    out
}

/// The key row every popup that only says something is answered with:
/// `[enter] confirm`, closing it and doing nothing else.
///
/// Defined once so a new popup takes this row rather than typing its own,
/// and every notice on every tab reads alike — five popups each spelling
/// the row out by hand is how they came to drift before.
pub(crate) fn confirm() -> String {
    keys(&[("enter", "confirm")])
}

/// A framed box with a line of keys under its body, one blank row apart.
pub(crate) fn panel(title: &str, body: &[String], keys: &str) -> Vec<String> {
    let mut full = body.to_vec();
    full.push(String::new());
    full.push(keys.to_string());
    boxed(title, &full)
}

/// The key row of every popup that asks a person something: `enter` runs
/// the answer under the cursor, `esc` closes the popup with nothing changed.
///
/// One row for every such popup, so no popup answers on letters of its
/// own, and no letter means one thing in one popup and another one tab
/// over. The arrows that move the cursor are not named: the `>` on the
/// cursor's row already says there is one.
pub(crate) fn answer_keys() -> String {
    keys(&[("enter", "confirm"), ("esc", "close")])
}

/// A popup's answers as rows, for the bottom of its body: a blank row, one
/// row per answer with `>` on the cursor's own and two spaces on the rest,
/// a blank row, and [`answer_keys`].
///
/// An answer of several lines — a pipeline and its description under it —
/// draws its later lines under its first, two columns in, so they read as
/// part of the answer rather than as answers of their own. A `cursor` past
/// the last answer marks no row.
///
/// Apart from [`pick`] for a caller that prints its answers under a notice
/// of its own rather than in a box — `spoolway dispatch`'s gates in a
/// terminal — so the printed answers and the boxed ones are the same rows.
/// A popup with one answer drawn through [`ask`] draws none of these rows,
/// though its printed twin still lists that answer.
pub(crate) fn answer_rows(answers: &[String], cursor: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    for (i, answer) in answers.iter().enumerate() {
        let mut lines = answer.split('\n');
        let first = lines.next().unwrap_or_default();
        let mark = if i == cursor { ">" } else { " " };
        rows.push(format!("{mark} {first}"));
        rows.extend(lines.map(|line| format!("  {line}")));
    }
    rows.push(String::new());
    rows.push(answer_keys());
    rows
}

/// A framed popup that asks something: its `body`, then its `answers` as
/// [`answer_rows`] draws them, the cursor on the `cursor`th, when there are
/// two or more.
///
/// Every popup that asks a question is drawn through this or [`pick`], so
/// each one reads and answers alike: `enter` runs the answer, and `esc`
/// closes. With two or more answers the arrows move the cursor and `enter`
/// runs the answer under it. A popup that only says something is a
/// [`notice`] instead, closed by [`confirm`].
///
/// A popup with exactly one answer is a confirmation, and draws no answer
/// row: it ends on its body, a blank row and [`answer_keys`]. A lone
/// `> delete` under a body that already says what deleting does repeats
/// the title, and reads like the first of a list of choices. `enter` still
/// runs that answer and `esc` still closes, so nothing is lost but the row.
/// A list built from data, which may come down to one entry and still
/// needs that entry named, is drawn by [`pick`] instead.
pub(crate) fn ask(title: &str, body: &[String], answers: &[String], cursor: usize) -> Vec<String> {
    if answers.len() == 1 {
        return panel(title, body, &answer_keys());
    }
    pick(title, body, answers, cursor)
}

/// A framed popup that picks one of `answers`: its `body`, then every
/// answer as [`answer_rows`] draws them, the cursor on the `cursor`th —
/// a single answer included.
///
/// For a popup whose answers are what it lists — a routine, a pipeline, a
/// step — rather than a fixed confirmation. Through [`ask`], such a list
/// narrowed to one entry would lose the one row that says what `enter`
/// picks.
pub(crate) fn pick(title: &str, body: &[String], answers: &[String], cursor: usize) -> Vec<String> {
    let mut full = body.to_vec();
    full.extend(answer_rows(answers, cursor));
    boxed(title, &full)
}

/// The line an [`options`] menu with no items shows instead of a list.
pub(crate) const NOTHING_TO_DO: &str = "nothing to do here";

/// A row's options menu: a popup titled `title` that lists `items`, each
/// named by its verb alone, the cursor on the `cursor`th, closed on
/// `[enter] confirm   [esc] close` like every popup that asks.
///
/// The items open straight under the title with no blank row: the menu
/// has no body above its list for one to set apart. A menu with no items
/// still opens, so `o` never seems to do nothing; it says
/// [`NOTHING_TO_DO`] over `[esc] close`, since there is nothing for
/// `enter` to confirm. That line is drawn plain, not dim: [`overlay`]
/// counts a colour code as columns, which would push the box's right
/// border out of line.
pub(crate) fn options(title: &str, items: &[&str], cursor: usize) -> Vec<String> {
    if items.is_empty() {
        return panel(
            title,
            &[NOTHING_TO_DO.to_string()],
            &keys(&[("esc", "close")]),
        );
    }
    let items: Vec<String> = items.iter().map(|item| item.to_string()).collect();
    boxed(title, &answer_rows(&items, cursor)[1..])
}

/// `cursor` moved by `key` over a list of `answers` answers: up or down
/// one row, held at either end, and unchanged by any other key.
pub(crate) fn move_cursor(cursor: usize, answers: usize, key: Key) -> usize {
    match key {
        Key::Up => cursor.saturating_sub(1),
        Key::Down => (cursor + 1).min(answers.saturating_sub(1)),
        _ => cursor,
    }
}

/// How wide [`notice`] wraps its text: a popup then stays inside the 100
/// columns every tab is drawn to, with the tab's own panes showing either
/// side of it.
pub(crate) const NOTICE_WRAP: usize = 66;

/// A popup that says something and names the keys that answer it: `text`
/// word-wrapped to `width` between a blank row above it and the key row
/// [`panel`] puts under it — the shape every notice over a tab is drawn in,
/// a refusal, a result or a gate alike.
///
/// Each of `text`'s own lines wraps on its own, keeping its own indent, so a
/// message that lays out rows of its own — the failed open hook's `opened`
/// and `log` — keeps them.
pub(crate) fn notice(title: &str, text: &str, keys: &str, width: usize) -> Vec<String> {
    let mut body = vec![String::new()];
    for line in text.lines() {
        body.extend(wrap(line, width));
    }
    panel(title, &body, keys)
}

/// What a screen has to tell a person — a refusal, a result — held on the
/// tab it belongs to as a popup until `enter` closes it.
#[derive(Debug, Clone)]
pub(crate) struct Notice {
    pub(crate) title: String,
    pub(crate) text: String,
}

impl Notice {
    pub(crate) fn new(title: &str, text: impl Into<String>) -> Notice {
        Notice {
            title: title.to_string(),
            text: text.into(),
        }
    }

    /// The popup, its text wrapped to `width` — see [`notice`] — over the
    /// [`confirm`] row, the one key that closes it.
    pub(crate) fn panel(&self, width: usize) -> Vec<String> {
        notice(&self.title, &self.text, &confirm(), width)
    }
}

/// `line` word-wrapped to `width`, each continuation keeping the line's own
/// leading indent so a command stays lined up under the one above it.
///
/// A single word too long for a row of its own — a path, most often — is
/// broken across as many rows as it takes. Left whole, it would widen the
/// popup past the frame [`overlay`] lays it on, and `overlay` drops whatever
/// runs past the frame's edge: the popup's right border, and the end of the
/// very path it was showing.
pub(crate) fn wrap(line: &str, width: usize) -> Vec<String> {
    let indent: String = line.chars().take_while(|c| *c == ' ').collect();
    let room = width.saturating_sub(indent.chars().count()).max(1);
    let mut lines = Vec::new();
    let mut current = indent.clone();
    for word in line.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        while word.len() > room {
            if current.len() > indent.len() {
                lines.push(std::mem::replace(&mut current, indent.clone()));
            }
            let rest = word.split_off(room);
            current.extend(word);
            lines.push(std::mem::replace(&mut current, indent.clone()));
            word = rest;
        }
        let word: String = word.into_iter().collect();
        if current.len() > indent.len()
            && current.chars().count() + 1 + word.chars().count() > width
        {
            lines.push(std::mem::replace(&mut current, indent.clone()));
        }
        if current.len() > indent.len() {
            current.push(' ');
        }
        current.push_str(&word);
    }
    lines.push(current);
    lines
}

/// Draw `panel` over `frame`, centred, one row down from centre so the
/// frame's own top border and its titles stay readable behind it.
pub(crate) fn overlay(frame: &mut [String], panel: &[String]) {
    let Some(width) = frame.first().map(|line| line.chars().count()) else {
        return;
    };
    let panel_width = panel.first().map_or(0, |line| line.chars().count());
    let x = width.saturating_sub(panel_width) / 2;
    let y = (frame.len().saturating_sub(panel.len()) / 2).max(1);

    for (i, line) in panel.iter().enumerate() {
        let Some(row) = frame.get_mut(y + i) else {
            break;
        };
        let mut chars: Vec<char> = row.chars().collect();
        for (j, ch) in line.chars().enumerate() {
            match chars.get_mut(x + j) {
                Some(slot) => *slot = ch,
                None => break,
            }
        }
        *row = chars.into_iter().collect();
    }
}

/// `n` with its noun, singular where that is what `n` is.
pub(crate) fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    }
}

/// What a test that reads a screen's text calls the screen `lines` draw: the
/// tab the strip marks open, or `fallback` when there is no strip, and the
/// title of a popup drawn over it, if there is one. A popup's top border
/// sits inside the tab's own `│` border, which a pane's title never does.
#[cfg(test)]
pub(crate) fn shown_screen(lines: &[String], fallback: &str) -> String {
    let tab = ["DISPATCH", "QUEUE", "ROUTINES", "OVERRIDES", "EVAL"]
        .into_iter()
        .find(|tab| {
            lines
                .iter()
                .take(3)
                .any(|line| line.contains(&format!("[{tab}]")))
        })
        .map_or(fallback.to_string(), str::to_lowercase);
    let popup = lines.iter().find_map(|line| {
        let (_, after) = line.split_once('│')?;
        let (_, title) = after.split_once(corner::TOP_LEFT)?;
        let title = title.trim_start_matches('─').split(" ─").next()?.trim();
        (!title.is_empty()).then(|| title.to_string())
    });
    match popup {
        Some(title) => format!("{tab} tab, popup \"{title}\""),
        None => format!("{tab} tab"),
    }
}

/// The frames one scripted walk drew, as the screens a test reads, each
/// named by what it shows — see [`shown_screen`]. Named by content because a
/// walk's earlier frames are not the screen its keys were written to reach,
/// and a name written beside the keys would be wrong for them.
#[cfg(test)]
pub(crate) fn walk_screens(fallback: &str, frames: &[String]) -> Vec<(String, Vec<String>)> {
    frames
        .iter()
        .map(|frame| {
            let lines: Vec<String> = frame.lines().map(str::to_string).collect();
            (shown_screen(&lines, fallback), lines)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::corner::{BOTTOM_LEFT, BOTTOM_RIGHT, TOP_LEFT, TOP_RIGHT};

    /// Words that keep a capital in TUI text, each under the one category
    /// the rule allows. All-caps words need no entry: [`capital_violation`]
    /// lets them through whole.
    const ALLOWED_CAPITALS: &[(&str, &str)] = &[
        // Month names and their short forms.
        ("January", "month"),
        ("February", "month"),
        ("March", "month"),
        ("April", "month"),
        ("May", "month"),
        ("June", "month"),
        ("July", "month"),
        ("August", "month"),
        ("September", "month"),
        ("October", "month"),
        ("November", "month"),
        ("December", "month"),
        ("Jan", "month"),
        ("Feb", "month"),
        ("Mar", "month"),
        ("Apr", "month"),
        ("Jun", "month"),
        ("Jul", "month"),
        ("Aug", "month"),
        ("Sep", "month"),
        ("Oct", "month"),
        ("Nov", "month"),
        ("Dec", "month"),
        // Weekday names and their short forms.
        ("Monday", "weekday"),
        ("Tuesday", "weekday"),
        ("Wednesday", "weekday"),
        ("Thursday", "weekday"),
        ("Friday", "weekday"),
        ("Saturday", "weekday"),
        ("Sunday", "weekday"),
        ("Mon", "weekday"),
        ("Tue", "weekday"),
        ("Wed", "weekday"),
        ("Thu", "weekday"),
        ("Fri", "weekday"),
        ("Sat", "weekday"),
        ("Sun", "weekday"),
        ("Mo", "weekday"),
        ("Tu", "weekday"),
        // "We" is left out of these two-letter forms: it opens English
        // sentences, so allowing it would let "We could not ..." through.
        ("Th", "weekday"),
        ("Fr", "weekday"),
        ("Sa", "weekday"),
        ("Su", "weekday"),
        // Proper nouns.
        ("Windows", "proper noun"),
        ("Unix", "proper noun"),
        ("Jira", "proper noun"),
    ];

    /// The greeting, whole: its first word is no capital to allow anywhere
    /// else, so it is taken out of a line before the line is read.
    const ALLOWED_PHRASES: &[(&str, &str)] = &[
        ("Good morning", "greeting"),
        ("Good afternoon", "greeting"),
        ("Good evening", "greeting"),
        ("Working late", "greeting"),
    ];

    /// The first word of `line` that starts with a capital outside the
    /// allowed cases: its start column and the word, or `None`.
    ///
    /// A word is a run of letters and digits, so `Press` in `(Press o)` is
    /// found, and a word with no lowercase letter, such as `ULID`, is read
    /// whole and let through.
    fn capital_violation(line: &str) -> Option<(usize, String)> {
        // Blanked rather than cut, so a column the report marks is still the
        // column in the line.
        let mut line = line.to_string();
        // The fixtures live under the machine's temporary directory, a value
        // the screens draw but do not write: macOS runners keep it under
        // `/Users/runner/work/_temp`. A column may cut it short at either
        // end, so any piece of it with a `/` in it counts, as long as it runs
        // to the end of the directory or to where the line stops spelling a
        // path.
        let temp = crate::scratch::temp_root().to_string_lossy().into_owned();
        let mut at = 0;
        while at < line.len() {
            let mut matched = 0;
            for (i, c) in line[at..].char_indices() {
                if !temp.contains(&line[at..at + i + c.len_utf8()]) {
                    break;
                }
                matched = i + c.len_utf8();
            }
            let piece = &line[at..at + matched];
            let whole_end = temp.ends_with(piece);
            let cut_short = line[at + matched..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '/');
            if piece.contains('/') && piece.len() > 2 && (whole_end || cut_short) {
                let blank = " ".repeat(piece.chars().count());
                line.replace_range(at..at + matched, &blank);
                at += matched;
            } else {
                at += line[at..].chars().next().map_or(1, char::len_utf8);
            }
        }
        for (phrase, _) in ALLOWED_PHRASES {
            while let Some(at) = line.find(phrase) {
                let mut end = at + phrase.len();
                // `Good morning, Marvin.`: the name after the greeting is git's
                // `user.name`, a value read from the machine.
                if line[end..].starts_with(", ") {
                    let name = &line[end + 2..];
                    end += 2 + name.find(['.', ' ', '│']).unwrap_or(name.len());
                }
                line.replace_range(at..end, &" ".repeat(line[at..end].chars().count()));
            }
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        let mut after_shouting = false;
        while i < chars.len() {
            if !chars[i].is_alphanumeric() {
                i += 1;
                continue;
            }
            let start = i;
            while i < chars.len() && chars[i].is_alphanumeric() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let first = word.chars().next().unwrap();
            // Every run of letters is a word wherever it sits, so `Cargo` in
            // `src/Cargo.toml` counts as much as one after a space.
            if !first.is_uppercase() {
                after_shouting = false;
                continue;
            }
            let letters = word.chars().filter(|c| c.is_alphabetic()).count();
            let shouting = !word.chars().any(char::is_lowercase)
                // A lone capital is a word of an all-caps heading only when
                // the word before it is one too, as the `R` in `CACHE R`.
                // Anywhere else it starts a sentence, such as `A job`.
                && (letters > 1 || after_shouting);
            after_shouting = shouting;
            if shouting || ALLOWED_CAPITALS.iter().any(|(allowed, _)| *allowed == word) {
                continue;
            }
            return Some((start, word));
        }
        None
    }

    /// Every capital in `screens` that the rule does not allow, as the
    /// message a failing test prints: the screen, the line, the word marked
    /// and the fix. A word on one line is reported once, on the first screen
    /// that draws it.
    fn capital_report(screens: &[(String, Vec<String>)]) -> String {
        let mut report = String::new();
        // One record for a word on a line, however many screens draw it: the
        // fixed label of a tab shows on every walk that passes through it.
        let mut seen = std::collections::HashSet::new();
        for (screen, lines) in screens {
            for line in lines {
                let Some((at, word)) = capital_violation(line) else {
                    continue;
                };
                if !seen.insert((line.clone(), word.clone())) {
                    continue;
                }
                let mut fixed = word.chars();
                let lower: String = fixed
                    .next()
                    .into_iter()
                    .flat_map(char::to_lowercase)
                    .chain(fixed)
                    .collect();
                report += &format!(
                    "a TUI word starts with a capital:\n  {screen}\n  \"{line}\"\n  {}{}\n\
                     TUI text is lowercase: write \"{lower}\".\n\
                     All-caps words, dates, proper nouns, the greeting and row values \
                     are allowed.\n\
                     If this word must keep its capital, add it to the allowed list \
                     beside this test.\n\n",
                    " ".repeat(at + 1),
                    "^".repeat(word.chars().count()),
                );
            }
        }
        report
    }

    /// One task in the queue, written as `queue add` would leave it. Written
    /// by hand because `queue add` prints a line per task, which a failing
    /// run would show above the report.
    fn queue_directly(
        repo: &crate::Repo,
        id: &str,
        depends_on: Option<&str>,
        group: &str,
        stage: &str,
    ) {
        let depends_on = depends_on.map_or(String::new(), |on| format!("depends_on:\n- {on}\n"));
        let doc = format!(
            "---\nid: {id}\ntitle: {id}, done\nstage: queued\n{depends_on}pipeline: default\n\
             group: {group}\nbranch: task/{id}\nbase: group/demo\n---\n## Goal\n\ndo it.\n"
        );
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(repo.queue_dir().join(format!("{id}.md")), doc).unwrap();
        if stage != "queued" {
            let mut task = repo.task(id).unwrap();
            task.set_stage(stage, None);
            task.save().unwrap();
        }
    }

    /// A project with something on every tab: a task queued, tasks pending
    /// in two groups, routines in two folders, a job on one of them and a
    /// job whose routine is gone. Every name is lowercase, so only the
    /// words the screens bring themselves can start with a capital.
    fn full_project(name: &str) -> (crate::Repo, crate::scratch::ScratchRoot) {
        let (repo, guard) = crate::status::testutil::fixture(name);
        for (id, depends_on, group, stage) in [
            ("wire", None, "demo", "queued"),
            ("route", Some("wire"), "demo", "queued"),
            ("stuck", None, "one", "blocked"),
            ("held", None, "two", "paused"),
            ("review", None, "three", "review-code"),
        ] {
            queue_directly(&repo, id, depends_on, group, stage);
        }
        std::fs::create_dir_all(repo.pending_dir()).unwrap();
        for (id, group) in [("cart", "shop"), ("pay", "shop"), ("logo", "site")] {
            std::fs::write(
                repo.pending_dir().join(format!("{id}.md")),
                format!(
                    "---\nid: {id}\ntitle: {id}, done\ngroup: {group}\npipeline: default\n\
                     ---\n## Goal\n\ndo it.\n"
                ),
            )
            .unwrap();
        }
        for (folder, id) in [("nightly", "audit"), ("weekly", "deps")] {
            let dir = repo.routines_dir().join(folder);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{id}.md")),
                format!("---\nid: {id}\ntitle: {id}, done\ngroup: demo\n---\n## Goal\n\ndo it.\n"),
            )
            .unwrap();
        }
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::write(
            repo.user_jobs_file(),
            "[jobs.maintenance]\nschedule = \"0 3 * * 1-5\"\npipeline = \"bugfix\"\n\
             routine = \"nightly\"\n\n[jobs.orphan]\nschedule = \"0 4 * * *\"\n\
             pipeline = \"bugfix\"\nroutine = \"gone\"\n",
        )
        .unwrap();
        (repo, guard)
    }

    /// A capital the rule does not allow is reported the way the guard's
    /// failure reads: the screen, the line, the word marked under it, and
    /// the lowercase word to write.
    #[test]
    fn a_capitalised_word_is_reported_with_its_screen_line_and_fix() {
        let screens = [(
            "routines tab, details of a job whose routine is gone".to_string(),
            vec![
                "  no routine to open.".to_string(),
                "  Press o to point this job at a routine.".to_string(),
            ],
        )];
        assert_eq!(
            capital_report(&screens),
            "a TUI word starts with a capital:\n\
             \x20 routines tab, details of a job whose routine is gone\n\
             \x20 \"  Press o to point this job at a routine.\"\n\
             \x20    ^^^^^\n\
             TUI text is lowercase: write \"press\".\n\
             All-caps words, dates, proper nouns, the greeting and row values \
             are allowed.\n\
             If this word must keep its capital, add it to the allowed list \
             beside this test.\n\n"
        );
    }

    /// A word on one line is reported once, on the first screen that draws
    /// it, so a tab's fixed label does not repeat for every walk that
    /// passes through the tab.
    #[test]
    fn a_word_on_a_line_is_reported_once_however_many_screens_draw_it() {
        let line = "  [h] Show done tasks".to_string();
        let screens = [
            ("queue tab".to_string(), vec![line.clone()]),
            ("queue tab, popup \"site\"".to_string(), vec![line.clone()]),
            (
                "dispatch tab".to_string(),
                vec!["  [h] Show done tasks!".to_string()],
            ),
        ];
        let report = capital_report(&screens);
        assert_eq!(report.matches("a TUI word starts").count(), 2, "{report}");
        assert!(!report.contains("popup"), "{report}");
    }

    /// A screen is named by the tab the strip marks and the title of the
    /// popup over it, and by the fallback tab when there is no strip.
    #[test]
    fn a_screen_is_named_by_its_tab_and_the_popup_over_it() {
        let lines = |text: &str| text.lines().map(str::to_string).collect::<Vec<_>>();
        let plain = lines(
            "\n  ←  DISPATCH       [QUEUE]       ROUTINES        EVAL  →\n\n ╭─ groups ──┬─ tasks ──╮\n │ > a       │          │\n",
        );
        assert_eq!(shown_screen(&plain, "eval"), "queue tab");
        let popup = lines(
            "\n  ←  DISPATCH        QUEUE       [ROUTINES]       EVAL  →\n\n ╭─ routines ┬─ details ╮\n │   ╭─ weekly ───╮  │\n",
        );
        assert_eq!(
            shown_screen(&popup, "eval"),
            "routines tab, popup \"weekly\""
        );
        let bare =
            lines(" ╭─ [eval]   directories   trials ── by pipeline ─╮\n │  ╭─ filters ─╮ │");
        assert_eq!(shown_screen(&bare, "eval"), "eval tab, popup \"filters\"");
    }

    /// Each case the rule allows passes, and the capitals next to them do
    /// not: a sentence that starts `A`, a lone `R` after a lowercase word,
    /// and a greeting's first word cut off from the rest of it.
    #[test]
    fn only_the_allowed_capitals_pass() {
        for line in [
            "│ PIPELINE  TASKS  CACHE R  OUT USD │",
            "next: Mon 26 Oct 03:00, Monday to Friday",
            "next  Tue 27 Oct  03:00  (Aug, Dec)",
            "Good morning.",
            "Working late.",
            "needs Windows or Unix, and a Jira key",
            "[enter] confirm   [esc] close   [↑↓] move",
        ] {
            assert_eq!(capital_violation(line), None, "{line}");
        }
        for (line, word) in [
            ("A job is due", "A"),
            ("pick one R", "R"),
            ("Good night.", "Good"),
            ("press Enter", "Enter"),
            ("(Delete the routine)", "Delete"),
            ("edit src/Cargo.toml", "Cargo"),
            ("queued — Waiting", "Waiting"),
            ("We could not reach it", "We"),
        ] {
            let found = capital_violation(line).map(|(_, word)| word);
            assert_eq!(found.as_deref(), Some(word), "{line}");
        }
    }

    /// No word of TUI text starts with a capital outside the allowed cases.
    ///
    /// Renders every tab with something on it and with nothing on it, the
    /// details panes and the popups the keys below open, then reads each
    /// line: the dispatch, queue, routines and overrides tabs through the
    /// shell on a scripted terminal, the eval tab and its trials table
    /// through its own synchronous loader, the popups no key reaches from
    /// where they are built, and every list of asking popups. Every name in the fixtures
    /// is lowercase, so a capital found is a word a screen brought itself.
    /// A screen is named by what it shows, since a walk's early frames are
    /// not where its keys end.
    ///
    /// No walk presses `enter` on the dispatch tab outside a popup, which
    /// starts a dispatcher from the test binary, or picks a board row's
    /// open action, which starts `$EDITOR`.
    #[test]
    fn tui_text_is_lowercase() {
        let _t = test_terminal((100, 30));
        let mut screens: Vec<(String, Vec<String>)> = Vec::new();
        let (left, right, down) = ("\x1b[D", "\x1b[C", "\x1b[B");
        let walks: Vec<String> = vec![
            // The tabs, each as the screen opens or is reached.
            String::new(),
            left.into(),
            right.into(),
            // Queue tab: panes, filters, the options menu and what each
            // item asks, then what answering it says.
            "\t".into(),
            "f".into(),
            "fwire".into(),
            "fzzz".into(),
            "h".into(),
            "h\t".into(),
            " ".into(),
            " \r".into(),
            "\r".into(),
            format!("{down}\r"),
            "\r\r".into(),
            "o".into(),
            "o\r".into(),
            "o\r\r".into(),
            "o\r\r\r".into(),
            format!("o{down}\r"),
            format!("o{down}\r\r"),
            format!("o{down}{down}\r"),
            format!("o{down}{down}\r\r"),
            format!("o{down}{down}\r\x1b"),
            // Routines tab: the list, its details pane, the job and the
            // routine without a job or without a routine, and the options
            // each has.
            format!("{right}\t"),
            format!("{right}\r"),
            format!("{right}{down}"),
            format!("{right}{down}{down}"),
            format!("{right}{down}{down}\t"),
            format!("{right}o"),
            format!("{right}o\r"),
            format!("{right}o\r\r"),
            format!("{right}o\r\r\r"),
            format!("{right}o\r\r\r\r"),
            format!("{right}o\r1\r"),
            format!("{right}o{down}\r"),
            format!("{right}o{down}\r\r"),
            format!("{right}o{down}{down}\r"),
            format!("{right}o{down}{down}\r\r"),
            format!("{right}{down}o"),
            format!("{right}{down}o\r"),
            format!("{right}{down}o\r\r"),
            format!("{right}{down}o\r\r\r"),
            format!("{right}{down}{down}o"),
            format!("{right}{down}{down}o\r"),
            format!("{right} \r"),
            format!("{right} \r\r"),
            // Overrides tab: each section, the search row, both config
            // popups with something typed into them, and the pipeline
            // section's step selection, searched, and its value field. The
            // prompts section's own walks are below.
            format!("{right}{right}"),
            format!("{right}{right}\t"),
            format!("{right}{right}\t\t\t"),
            format!("{right}{right}fdispatch"),
            format!("{right}{right}fzzz"),
            format!("{right}{right}\rdispatch.x\t5"),
            format!("{right}{right}{down}\r"),
            format!("{right}{right}\t\r"),
            format!("{right}{right}\t\rfnot set"),
            format!("{right}{right}\t\ra\rx"),
            // Dispatch tab: the board's rows, their options and the
            // resume and skip-a-hook questions. Never `enter` outside a
            // popup, and never an option that opens a file.
            format!("{left}u"),
            format!("{left}o"),
            format!("{left}{down}"),
            format!("{left}{down}{down}{down}"),
            format!("{left}{down}o"),
            format!("{left}{down}{down}{down}o"),
            format!("{left}{down}{down}{down}{down}o"),
            format!("{left}{down}{down}{down}r"),
            format!("{left}{down}{down}{down}k"),
        ];
        for (i, keys) in walks.iter().enumerate() {
            let (repo, _guard) = full_project(&format!("capitals-{i}"));
            let frames = crate::screen::shell::drive_frames(&repo, keys);
            screens.extend(walk_screens("queue", &frames));
        }
        // The overrides tab's prompts section with a prompt to list: its
        // search, both popups `enter` opens, and the `open workspace` popup
        // `w` opens when the folder cannot be opened.
        let prompt_walks = [
            format!("{right}{right}\t\t"),
            format!("{right}{right}\t\tfimpl"),
            format!("{right}{right}\t\t\r"),
            format!("{right}{right}\t\t\r\r\r"),
            format!("{right}{right}w"),
        ];
        crate::browser::refuse_opens("could not open the overrides folder: no display is set");
        for (i, keys) in prompt_walks.iter().enumerate() {
            let (repo, _guard) = full_project(&format!("capitals-prompts-{i}"));
            let path = crate::prompt::directory_form(&repo, "implementer");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "implementer\n").unwrap();
            let frames = crate::screen::shell::drive_frames(&repo, keys);
            screens.extend(walk_screens("queue", &frames));
        }
        // A project with nothing in it, where each tab says so and the
        // dispatch tab greets instead of listing.
        let empty_walks = [
            String::new(),
            "o".into(),
            left.into(),
            format!("{left}o"),
            right.into(),
            format!("{right}o"),
        ];
        for (i, keys) in empty_walks.iter().enumerate() {
            let (repo, _guard) = crate::status::testutil::fixture(&format!("capitals-empty-{i}"));
            let frames = crate::screen::shell::drive_frames(&repo, keys);
            screens.extend(walk_screens("queue", &frames));
        }
        for (name, panel) in [
            crate::status::asking_panels(),
            vec![
                ("stop dispatching", dispatcher::stop_panel(0)),
                (
                    "stop dispatching, tasks queued",
                    dispatcher::stop_queued_panel(1, 0),
                ),
            ],
            crate::commands::dispatch_asking_panels(),
            crate::commands::queue_asking_panels(),
            crate::commands::jobs_asking_panels(),
        ]
        .concat()
        {
            screens.push((format!("popup: {name}"), panel));
        }
        screens.extend(crate::eval::tui_screens());
        screens.extend(crate::screen::shell::tui_screens());
        let report = capital_report(&screens);
        assert!(report.is_empty(), "{report}");
    }

    /// Every popup in the project that asks a person something closes on
    /// `[enter] confirm   [esc] close` and nothing else: no letter answers,
    /// no key row of its own. Each module hands over its own asking popups,
    /// drawn once; a listed popup that stops drawing through [`ask`] or
    /// [`pick`], or grows a key row of its own, fails here. A new popup has
    /// to be added to its module's list to be walked.
    ///
    /// The popups named in `confirmations` have one answer, so [`ask`]
    /// draws no `>` row on them; every other one has its cursor on exactly
    /// one answer.
    #[test]
    fn every_popup_that_asks_closes_on_confirm_and_close() {
        let popups = [
            crate::status::asking_panels(),
            vec![
                ("stop dispatching", dispatcher::stop_panel(0)),
                (
                    "stop dispatching, tasks queued",
                    dispatcher::stop_queued_panel(1, 0),
                ),
            ],
            crate::commands::dispatch_asking_panels(),
            crate::commands::queue_asking_panels(),
            crate::commands::jobs_asking_panels(),
        ]
        .concat();
        assert_eq!(popups.len(), 22, "every popup is walked");
        let confirmations = [
            "issue tracking",
            "delete routine",
            "delete pending",
            "pause, command step",
            "restart",
            "restart, command step",
            "unqueue",
            "unqueue, with a dependent",
            "unqueue, a routine copy",
            "unqueue, a chain with a routine copy",
            "skip hook, done",
            "skip hook, queued",
        ];
        let row = |line: &str| line.trim_matches(['│', ' ']).to_string();
        for (name, panel) in popups {
            let drawn = panel.join("\n");
            assert!(
                panel[panel.len() - 1].starts_with(BOTTOM_LEFT),
                "{name}: {drawn}"
            );
            assert_eq!(
                row(&panel[panel.len() - 2]),
                answer_keys(),
                "{name}: {drawn}"
            );
            let keyed: Vec<String> = panel
                .iter()
                .map(|line| row(line))
                .filter(|line| line.starts_with('['))
                .collect();
            assert_eq!(keyed, [answer_keys()], "{name}: one key row: {drawn}");
            let cursors = usize::from(!confirmations.contains(&name));
            assert_eq!(
                panel
                    .iter()
                    .filter(|line| row(line).starts_with("> "))
                    .count(),
                cursors,
                "{name}: {cursors} answer rows under the cursor: {drawn}"
            );
        }
    }

    // Pins the exact bytes the board's own key line already draws — see
    // `src/status/mod.rs`'s hint — so a screen that switches over to
    // `key_hint` gets that spelling byte for byte, not a close approximation.
    #[test]
    fn key_hint_wraps_a_leading_space_and_the_pairs_in_dim_and_reset() {
        assert_eq!(
            key_hint(&[("o", "open"), ("r", "routines")]),
            "\x1b[2m [o] open   [r] routines\x1b[0m"
        );
    }

    // A key line wider than the area breaks between two pairs, never inside
    // one, and every row it takes starts with the first row's own dim and
    // space and ends with its reset — a row of its own, not a tail.
    #[test]
    fn a_key_line_breaks_between_pairs_and_each_row_keeps_its_lead() {
        let line = key_hint(&[("a", "one"), ("b", "two"), ("c", "three")]);
        assert_eq!(
            key_rows_to(&line, 20),
            [
                "\x1b[2m [a] one   [b] two\x1b[0m",
                "\x1b[2m [c] three\x1b[0m",
            ]
        );
    }

    // The lit pair alone leaves the dim: it closes the line's dim before
    // itself and opens it again after, and every other pair stays inside.
    #[test]
    fn key_hint_lit_draws_its_one_pair_out_of_the_dim() {
        assert_eq!(
            key_hint_lit(&[("o", "open"), ("u", "update"), ("q", "quit")], "u"),
            "\x1b[2m [o] open   \x1b[0m[u] update\x1b[2m   [q] quit\x1b[0m"
        );
        assert_eq!(
            key_hint_lit(&[("o", "open"), ("q", "quit")], "u"),
            key_hint(&[("o", "open"), ("q", "quit")])
        );
    }

    // Broken at a narrow width, a line with a lit pair still starts every
    // row dim and ends it on a reset, and the lit pair stays whole with its
    // own codes, whether it lands mid-row, at a row's end or at its start.
    #[test]
    fn a_key_line_with_a_lit_pair_breaks_with_every_row_dim() {
        let line = key_hint_lit(&[("a", "one"), ("u", "two"), ("c", "three")], "u");
        let lit = "\x1b[0m[u] two\x1b[2m";
        assert_eq!(
            key_rows_to(&line, 20),
            [
                format!("\x1b[2m [a] one   {lit}\x1b[0m"),
                "\x1b[2m [c] three\x1b[0m".to_string(),
            ]
        );
        assert_eq!(
            key_rows_to(&line, 10),
            [
                "\x1b[2m [a] one\x1b[0m".to_string(),
                format!("\x1b[2m {lit}\x1b[0m"),
                "\x1b[2m [c] three\x1b[0m".to_string(),
            ]
        );
        // First on the line, the lit pair's reset must not become the lead
        // every later row starts with.
        let first = key_hint_lit(&[("u", "two"), ("c", "three")], "u");
        assert_eq!(
            key_rows_to(&first, 10),
            [
                format!("\x1b[2m {lit}\x1b[0m"),
                "\x1b[2m [c] three\x1b[0m".to_string(),
            ]
        );
    }

    // A line exactly as wide as the area is one row; a column narrower and
    // its last pair moves down. An empty line is still a row.
    #[test]
    fn a_key_line_as_wide_as_the_area_stays_one_row() {
        let line = key_hint(&[("a", "one"), ("b", "two")]);
        let width = crate::status::strip_ansi(&line).chars().count();
        assert_eq!(key_rows_to(&line, width), std::slice::from_ref(&line));
        assert_eq!(key_rows_to(&line, width - 1).len(), 2);
        assert_eq!(key_rows_to("", 10), [""]);
    }

    // The eval tab's key line carries a space of its own before the dim;
    // every row it wraps onto keeps both, so its rows line up under the
    // first.
    #[test]
    fn a_key_line_with_its_own_indent_keeps_it_on_every_row() {
        let line = format!(" {}", key_hint(&[("a", "one"), ("b", "two")]));
        assert_eq!(
            key_rows_to(&line, 12),
            [" \x1b[2m [a] one\x1b[0m", " \x1b[2m [b] two\x1b[0m",]
        );
    }

    // Hosted, the drawing area is the terminal less one column each side
    // and one row at the foot; with no shell around a screen it is the
    // whole terminal, so nothing printed outside bare `spoolway` moves.
    #[test]
    fn the_drawing_area_keeps_the_margin_only_while_hosted() {
        let _terminal = test_terminal((100, 30));
        assert_eq!(drawing_area(), Some((100, 30)));
        let _hosting = shell::Hosting::open(shell::Tab::Queue);
        assert_eq!(drawing_area(), Some((98, 29)));
        assert_eq!(pane_size(), (100, 30));
    }

    #[test]
    fn a_long_line_wraps_under_its_own_indent() {
        let lines = wrap("  one two three four", 11);
        assert_eq!(lines, ["  one two", "  three", "  four"]);
    }

    #[test]
    fn a_word_too_long_for_a_row_is_broken_across_rows() {
        let lines = wrap("  at /a/very/long/path end", 10);
        assert_eq!(lines, ["  at", "  /a/very/", "  long/pat", "  h end"]);
    }

    // The shape every notice over a tab takes, as the queue tab's own
    // refusal is drawn: a blank row, the text, a blank row, the keys.
    #[test]
    fn a_notice_is_its_text_between_blank_rows_over_its_keys() {
        let panel = notice("trial refused", "a b", &confirm(), 40);
        assert_eq!(
            panel,
            [
                format!("{TOP_LEFT}─ trial refused ───{TOP_RIGHT}").as_str(),
                "│                   │",
                "│  a b              │",
                "│                   │",
                "│  [enter] confirm  │",
                format!("{BOTTOM_LEFT}───────────────────{BOTTOM_RIGHT}").as_str(),
            ]
        );
    }

    /// A real OS pipe, so a test can control exactly when the reading end
    /// sees a byte — the one thing a `Cursor` can never simulate, since its
    /// bytes are always all there from the start.
    fn pipe() -> (std::os::unix::io::RawFd, std::os::unix::io::RawFd) {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a well-formed two-element buffer `pipe` fills in.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    }

    fn close(fd: std::os::unix::io::RawFd) {
        // SAFETY: `fd` is a descriptor this test opened and is done with.
        unsafe {
            libc::close(fd);
        }
    }

    // A signal ends the wait early with no byte, and is drained; a byte
    // typed first still answers `true`. Before `wait_input_or_wake`, the
    // dispatch tab's wait could not be cut short by its reader.
    #[test]
    fn a_wake_ends_the_wait_early_and_a_byte_still_answers() {
        let (read_end, write_end) = pipe();
        let wake = Wake::new().unwrap();
        let long = std::time::Duration::from_secs(10);

        wake.notify();
        let began = std::time::Instant::now();
        assert!(!wait_input_or_wake(read_end, &wake, long));
        assert!(began.elapsed() < std::time::Duration::from_secs(5));
        assert!(
            !fd_has_byte_within(wake.read_fd(), std::time::Duration::ZERO),
            "the signal is drained"
        );

        // SAFETY: `write_end` is this test's own live pipe fd.
        unsafe { libc::write(write_end, b"x".as_ptr().cast(), 1) };
        assert!(wait_input_or_wake(read_end, &wake, long));

        close(read_end);
        close(write_end);
    }

    // Regression for the review finding that pressing Escape alone froze the
    // screen: `read_escape`'s first read used to block indefinitely on a
    // byte that, for a lone `Esc`, is never coming. `fd_has_byte_within` is
    // what stands between that blocking read and the caller now — proving
    // it returns promptly, not just eventually, is what proves the freeze is
    // gone rather than just shortened.
    #[test]
    fn a_pipe_with_nothing_written_is_not_pending_and_does_not_hang() {
        let (read_end, write_end) = pipe();
        let start = std::time::Instant::now();
        let pending = fd_has_byte_within(read_end, std::time::Duration::from_millis(50));
        let elapsed = start.elapsed();
        close(read_end);
        close(write_end);

        assert!(!pending, "nothing was ever written to this pipe");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "the wait should be bounded by the timeout, not hang: took {elapsed:?}"
        );
    }

    #[test]
    fn a_pipe_with_a_byte_already_written_is_pending_immediately() {
        let (read_end, write_end) = pipe();
        // SAFETY: `write_end` is the pipe's own write end, `buf` a live byte.
        let buf = *b"[";
        unsafe {
            libc::write(write_end, buf.as_ptr().cast(), 1);
        }

        let start = std::time::Instant::now();
        let pending = fd_has_byte_within(read_end, std::time::Duration::from_millis(50));
        let elapsed = start.elapsed();
        close(read_end);
        close(write_end);

        assert!(pending, "a byte was already sitting in the pipe");
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "an already-ready byte should not wait out the timeout: took {elapsed:?}"
        );
    }

    /// A pipe closed from the write end (`Ok(())` never dropped, so no
    /// writer is left) reads as `PollableRead::byte_pending` would see a real
    /// EOF: `poll` still reports it "ready" — the read that follows returns
    /// `0`, same as the existing empty-`Cursor` path `read_key` already
    /// handles as end of input, not as a lone `Esc`.
    #[test]
    fn a_closed_pipe_is_pending_too_so_read_still_sees_the_eof() {
        let (read_end, write_end) = pipe();
        close(write_end);

        let pending = fd_has_byte_within(read_end, std::time::Duration::from_millis(50));
        close(read_end);

        assert!(pending, "a closed write end must not look like a live wait");
    }

    fn keys(s: &str) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(s.as_bytes().to_vec())
    }

    // A lone Esc at end of input — nothing following it — is exactly what
    // used to hang on a real tty. `Cursor` can't reproduce the hang itself
    // (its "no more bytes" already reads as an immediate `0`, not a block),
    // but this pins the decoded result: a bare `Esc`, not swallowed into a
    // `None` that would end the whole screen.
    #[test]
    fn a_lone_escape_at_end_of_input_decodes_as_esc_not_end_of_input() {
        let mut input = keys("\x1b");
        assert_eq!(read_key(&mut input), Some(Key::Esc));
    }

    /// The `File` half of a real pipe, wired up to [`PollableRead`] the same
    /// way [`RawStdin`] is, so a test can drive `read_key` end to end
    /// over a descriptor whose timing it actually controls — a `Cursor`
    /// can't stand in here, since every one of its bytes is already "there"
    /// the instant it exists and can never model a byte arriving late.
    struct PipeRead(std::fs::File);

    impl std::io::Read for PipeRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl PollableRead for PipeRead {
        fn byte_pending(&self, timeout: std::time::Duration) -> bool {
            use std::os::unix::io::AsRawFd;
            fd_has_byte_within(self.0.as_raw_fd(), timeout)
        }
    }

    // The review's own repro: Escape typed alone, with the very next key
    // landing well after `ESCAPE_TIMEOUT` (real terminals reported gaps of
    // 0.3-1s and it still swallowed the key) — proving read_key neither
    // hangs on the lone Esc nor eats the keystroke that follows it once the
    // ambiguity is resolved.
    #[test]
    fn a_key_typed_well_after_a_lone_escape_is_not_swallowed() {
        use std::os::unix::io::FromRawFd;
        let (read_end, write_end) = pipe();
        // SAFETY: `read_end` is a pipe read fd this test owns and hands off
        // to `File`, which becomes responsible for closing it.
        let mut input = PipeRead(unsafe { std::fs::File::from_raw_fd(read_end) });

        // SAFETY: `write_end` is this test's own live pipe fd.
        unsafe {
            libc::write(write_end, c"\x1b".as_ptr().cast(), 1);
        }
        let writer = std::thread::spawn(move || {
            // Comfortably past ESCAPE_TIMEOUT (50ms) — the gap the review's
            // repro actually hit was two to twenty times this.
            std::thread::sleep(std::time::Duration::from_millis(150));
            // SAFETY: `write_end` is still this thread's own live pipe fd.
            unsafe {
                libc::write(write_end, c"e".as_ptr().cast(), 1);
            }
            close(write_end);
        });

        let start = std::time::Instant::now();
        assert_eq!(read_key(&mut input), Some(Key::Esc));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "the lone Esc must resolve in roughly ESCAPE_TIMEOUT, not wait for the next key"
        );
        assert_eq!(
            read_key(&mut input),
            Some(Key::Char('e')),
            "the delayed keystroke must still be its own key, not consumed disambiguating Esc"
        );

        writer.join().unwrap();
    }

    // Regression for the review finding that arrow keys silently stopped
    // working: a real terminal sends `\x1b[X` as one burst, and a reader
    // that buffers past what one `read` call asked for can drain all three
    // bytes off the kernel in the very first read, leaving `byte_pending`'s
    // kernel-level `poll` looking at an empty fd for bytes the reader is
    // already holding. `PipeRead` reads straight off the fd the same way
    // `RawStdin` does — one byte per `read` call, nothing buffered ahead of
    // it — so this proves that discipline is what keeps `poll` and `read` in
    // agreement for a burst sent all at once, the normal way a tty delivers
    // one.
    #[test]
    fn an_arrow_key_sent_as_one_burst_is_not_misread_as_a_bare_escape() {
        use std::os::unix::io::FromRawFd;
        let (read_end, write_end) = pipe();
        // SAFETY: `read_end` is a pipe read fd this test owns and hands off
        // to `File`, which becomes responsible for closing it.
        let mut input = PipeRead(unsafe { std::fs::File::from_raw_fd(read_end) });

        // One write, all three bytes together — exactly how a real terminal
        // delivers an arrow key, not three separate keystrokes.
        // SAFETY: `write_end` is this test's own live pipe fd.
        unsafe {
            libc::write(write_end, c"\x1b[B".as_ptr().cast(), 3);
        }
        close(write_end);

        assert_eq!(
            read_key(&mut input),
            Some(Key::Down),
            "a burst \\x1b[B must decode as Down, not a bare Esc with the rest dropped"
        );
    }

    #[test]
    fn poll_ready_answers_each_fd_independently() {
        let (read_a, write_a) = pipe();
        let (read_b, write_b) = pipe();
        // SAFETY: `write_a` is this test's own live pipe fd.
        unsafe {
            libc::write(write_a, c"x".as_ptr().cast(), 1);
        }

        let start = std::time::Instant::now();
        let ready = poll_ready(&[read_a, read_b], std::time::Duration::from_millis(50));
        let elapsed = start.elapsed();

        close(read_a);
        close(write_a);
        close(read_b);
        close(write_b);

        assert_eq!(
            ready,
            vec![true, false],
            "only the fd a byte was actually written to should read ready"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "a fd that is already ready must not wait out the timeout: took {elapsed:?}"
        );
    }

    /// A scratch directory stands in for the commands directory a real
    /// [`DirWatch`] joins — see `commands::dispatch`'s own wait loop.
    /// Changing it must wake `poll_ready` on the watch's own fd and
    /// [`DirWatch::drain`] must say so.
    #[cfg(target_os = "linux")]
    #[test]
    fn dir_watch_wakes_on_a_commands_directory_change() {
        let commands_dir = crate::scratch::root("dir-watch-commands");
        let _ = std::fs::remove_dir_all(&commands_dir);
        std::fs::create_dir_all(&commands_dir).unwrap();

        let watch = DirWatch::new(&commands_dir).expect("inotify must be available");

        change_commands_dir(&commands_dir, || {});
        let ready = poll_ready(&[watch.fd()], std::time::Duration::from_millis(500));
        assert_eq!(
            ready,
            vec![true],
            "creating a subdirectory must wake the watch"
        );
        assert!(watch.drain(), "the commands directory changed");

        // Nothing pending: a poll with no writes since the last drain must
        // time out rather than report a stale wake.
        let ready = poll_ready(&[watch.fd()], std::time::Duration::from_millis(50));
        assert_eq!(ready, vec![false]);
        assert!(!watch.drain());

        let _ = std::fs::remove_dir_all(&commands_dir);
    }

    /// The change [`dir_watch_wakes_on_a_commands_directory_change`] makes
    /// to the watched directory. It creates a subdirectory, which queues
    /// `IN_CREATE` at once and opens no descriptor. A file written here
    /// would be the wrong trigger: a child forked on another test thread
    /// while it is open holds it until the child execs, and the file's
    /// `IN_CLOSE_WRITE` is then queued late, after the test's `drain()`.
    /// `during` runs right after the change, where such a fork can land.
    #[cfg(target_os = "linux")]
    fn change_commands_dir(commands_dir: &std::path::Path, during: impl FnOnce()) {
        std::fs::create_dir(commands_dir.join("demo.d")).unwrap();
        during();
    }

    /// The forked child of the test below, and the write end of the pipe it
    /// blocks on. Dropping it releases and reaps the child, so an assertion
    /// that panics before the test does so itself cannot leave the child
    /// blocked, holding a copy of every descriptor open at fork time, for
    /// the rest of the test process.
    #[cfg(target_os = "linux")]
    struct LateChild {
        release_write: std::os::unix::io::RawFd,
        pid: libc::pid_t,
    }

    #[cfg(target_os = "linux")]
    impl LateChild {
        /// Lets the child go and waits until it has exited. A second call
        /// does nothing.
        fn release(&mut self) {
            if self.release_write < 0 {
                return;
            }
            close(self.release_write);
            self.release_write = -1;
            if self.pid > 0 {
                // SAFETY: `pid` is this test's own child process.
                unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) };
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for LateChild {
        fn drop(&mut self) {
            self.release();
        }
    }

    /// Guards the stale wake the test above once hit under a full
    /// `cargo test` run. Other tests spawn children with a `pre_exec` hook
    /// (`command_step`, `headless`), which forces a real `fork`. A child
    /// forked while a watched file is open holds it until the child execs,
    /// so that file's `IN_CLOSE_WRITE` is queued only then, after `drain()`
    /// when the child is slow. The child here waits on a pipe instead of
    /// exec, so a late close would land after `drain()` every time. The
    /// change holds no file open, so a slow child has nothing to close.
    #[cfg(target_os = "linux")]
    #[test]
    fn dir_watch_stays_quiet_when_a_forked_child_closes_the_file_late() {
        let commands_dir = crate::scratch::root("dir-watch-late-close");
        let _ = std::fs::remove_dir_all(&commands_dir);
        std::fs::create_dir_all(&commands_dir).unwrap();

        let watch = DirWatch::new(&commands_dir).expect("inotify must be available");

        // Close-on-exec, so a process another test thread spawns while this
        // pipe is open does not inherit the write end: the forked child below
        // reads until every copy of it is closed, and `waitpid` would then
        // wait out that unrelated process.
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a well-formed two-element buffer `pipe2` fills in.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let (release_read, release_write) = (fds[0], fds[1]);
        let mut child = LateChild {
            release_write,
            pid: 0,
        };
        change_commands_dir(&commands_dir, || {
            // SAFETY: the child calls only async-signal-safe functions
            // (`close`, `read`, `_exit`) before it leaves.
            child.pid = unsafe { libc::fork() };
            assert!(child.pid >= 0, "fork failed");
            if child.pid == 0 {
                // SAFETY: async-signal-safe calls on descriptors the child
                // inherited; it blocks until the parent closes its end.
                unsafe {
                    libc::close(release_write);
                    let mut byte = 0u8;
                    libc::read(release_read, (&raw mut byte).cast(), 1);
                    libc::_exit(0);
                }
            }
        });
        close(release_read);

        let ready = poll_ready(&[watch.fd()], std::time::Duration::from_millis(500));
        assert_eq!(
            ready,
            vec![true],
            "creating a subdirectory must wake the watch"
        );
        assert!(watch.drain(), "the commands directory changed");

        // Let the child go, and wait until it has exited and released every
        // descriptor it inherited.
        child.release();

        // The same check as the test above: no writes since the drain.
        let ready = poll_ready(&[watch.fd()], std::time::Duration::from_millis(50));
        assert_eq!(ready, vec![false], "stale wake after drain");
        assert!(!watch.drain());

        let _ = std::fs::remove_dir_all(&commands_dir);
    }
}
