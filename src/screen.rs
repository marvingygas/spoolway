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
//! handling — only what is generic across any of them lives here.

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
        #[cfg(unix)]
        {
            // SAFETY: `buf` is a valid, appropriately sized buffer for the
            // duration of this call, and stdin's descriptor is open for the
            // life of the process.
            let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(n as usize)
        }
        #[cfg(not(unix))]
        {
            std::io::Read::read(&mut std::io::stdin(), buf)
        }
    }
}

impl PollableRead for RawStdin {
    #[cfg(unix)]
    fn byte_pending(&self, timeout: std::time::Duration) -> bool {
        fd_has_byte_within(libc::STDIN_FILENO, timeout)
    }

    // No Windows termios means no Windows raw mode either (see
    // `platform::TermGuard`), so nothing on that platform ever blocks stdin
    // waiting on a key in the first place — the old always-ready behaviour
    // is exactly right here too.
    #[cfg(not(unix))]
    fn byte_pending(&self, _timeout: std::time::Duration) -> bool {
        true
    }
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
    let mut fds = [libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }];
    let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `fds` holds one well-formed `pollfd` on a descriptor the
    // caller keeps alive for the duration of the call.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) };
    ready > 0 && fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
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

/// A framed box — a title and whatever lines fill it — to be drawn over a
/// screen's own frame.
///
/// Sits on top of the layout rather than pushed into it: a list inserted
/// between two rows moves everything below it down, and a person reading a
/// row loses the one they had the cursor on.
///
/// Shared by [`panel`], which appends its own key line as one more row after
/// a blank spacer, and by callers whose body lines are already the choices a
/// person reads and need no footer after them.
pub(crate) fn boxed(title: &str, body: &[String]) -> Vec<String> {
    let head = format!("─ {title} ");
    let widest = body
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let inner = (widest + 4).max(head.chars().count() + 2);

    let mut out = vec![format!(
        "┌{head}{}┐",
        "─".repeat(inner - head.chars().count())
    )];
    for line in body {
        out.push(format!("│{}│", pad_to(&format!("  {line}"), inner)));
    }
    out.push(format!("└{}┘", "─".repeat(inner)));
    out
}

/// A framed box with a line of keys under its body, one blank row apart.
pub(crate) fn panel(title: &str, body: &[String], keys: &str) -> Vec<String> {
    let mut full = body.to_vec();
    full.push(String::new());
    full.push(keys.to_string());
    boxed(title, &full)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A real OS pipe, so a test can control exactly when the reading end
    /// sees a byte — the one thing a `Cursor` can never simulate, since its
    /// bytes are always all there from the start.
    #[cfg(unix)]
    fn pipe() -> (std::os::unix::io::RawFd, std::os::unix::io::RawFd) {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a well-formed two-element buffer `pipe` fills in.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    }

    #[cfg(unix)]
    fn close(fd: std::os::unix::io::RawFd) {
        // SAFETY: `fd` is a descriptor this test opened and is done with.
        unsafe {
            libc::close(fd);
        }
    }

    // Regression for the review finding that pressing Escape alone froze the
    // screen: `read_escape`'s first read used to block indefinitely on a
    // byte that, for a lone `Esc`, is never coming. `fd_has_byte_within` is
    // what stands between that blocking read and the caller now — proving
    // it returns promptly, not just eventually, is what proves the freeze is
    // gone rather than just shortened.
    #[test]
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
    struct PipeRead(std::fs::File);

    #[cfg(unix)]
    impl std::io::Read for PipeRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
}
