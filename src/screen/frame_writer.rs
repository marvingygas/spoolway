//! The shared writer every redrawing screen will call: one frame, one write,
//! painted over the last frame rather than erasing first. An erase followed
//! by one write per line is N+1 writes a slow terminal (WSL, behind Herdr
//! and the Windows console) can paint between, showing a blank or
//! half-drawn frame. This writer sends the whole frame as bytes:
//! `ESC[?2026h ESC[H`, then each row followed by `ESC7 ESC[0m ESC[K ESC8`
//! and `\n` (a row whose visible text fills the pane gets no clear at all),
//! then `ESC7 ESC[0m ESC[J ESC8 ESC[?2026l`. The `ESC7`/`ESC8` pair around
//! each clear is explained at [`CLEAR_TO_END`]. The `?2026` pair is synchronized
//! output: a terminal that understands it holds its paint until the closing
//! code arrives, so even a write a kernel pipe splits into several reads
//! still paints as one frame. Every redrawing screen — `commands::queue`'s
//! `paint`, `commands::jobs`'s `draw_jobs`, `eval`'s `draw`, and
//! `screen::shell`'s `draw_board` and `message_frame` — now calls this
//! instead of erasing and writing its own rows.

/// Moves the cursor home and tells a terminal that understands synchronized
/// output to hold its paint until [`FRAME_END`] arrives.
const FRAME_START: &str = "\x1b[?2026h\x1b[H";

/// Clears the rest of a row past its own text — every row gets this except
/// one whose visible text already reaches the pane's right edge, where the
/// terminal itself would eat the last character sitting under the cursor.
///
/// A row may leave a colour or weight open, with no reset before it ends.
/// An erased cell takes on the terminal's current rendition, so a bare
/// `ESC[K` would tint the cleared tail, which today's whole-screen erase
/// never does. A bare reset before `ESC[K` fixes the tail but breaks the
/// rows after it: today the open colour carries on into the next row's
/// text, and the reset stopped that (seen 2026-09-29 in the `vt100` proof).
/// So the clear runs between `ESC7` and `ESC8` (DECSC/DECRC), which save
/// and restore the cursor together with its rendition: the cleared cells
/// come out plain, and the next row inherits exactly what it did before.
const CLEAR_TO_END: &str = "\x1b7\x1b[0m\x1b[K\x1b8";

/// Clears every row below the frame just written (so a shorter frame does
/// not leave a longer one's tail behind) and ends the synchronized-output
/// hold, letting the terminal paint. The clear is wrapped in `ESC7`/`ESC8`
/// for the same reason as [`CLEAR_TO_END`]: the rows below come out plain,
/// and a colour the last row left open still reaches the next frame's
/// first row, as it does today, since `ESC[2J` never reset it either.
const FRAME_END: &str = "\x1b7\x1b[0m\x1b[J\x1b8\x1b[?2026l";

/// A frame's rows together with the pane size they were drawn for — what
/// [`FrameWriter`] compares a new frame against to decide whether there is
/// anything to write at all.
type Frame = (Vec<String>, (usize, usize));

/// Remembers the last frame it wrote, so a screen that redraws on every poll
/// tick — the dispatch tab redraws once a second whether or not anything
/// moved — can ask this writer to paint and get back silence on every tick
/// where the picture would be identical.
pub(crate) struct FrameWriter {
    last: Option<Frame>,
}

impl FrameWriter {
    pub(crate) fn new() -> FrameWriter {
        FrameWriter { last: None }
    }

    /// Forget the last frame written, so the next [`Self::write_frame`] call
    /// always writes — for a screen that draws something outside this
    /// writer's own memory, such as a tab switch overwriting the whole
    /// pane, or a one-shot popup that must not be mistaken for this
    /// writer's own last frame on the next redraw.
    pub(crate) fn forget(&mut self) {
        self.last = None;
    }

    /// Paint `rows` at `pane_size` (columns, rows) to `out`, unless both
    /// `rows` and `pane_size` equal the last frame this writer painted, in
    /// which case nothing is written at all.
    pub(crate) fn write_frame(
        &mut self,
        rows: &[String],
        pane_size: (usize, usize),
        out: &mut impl std::io::Write,
    ) {
        if self.last.as_ref().is_some_and(|(last_rows, last_size)| {
            last_rows.as_slice() == rows && *last_size == pane_size
        }) {
            return;
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(FRAME_START.as_bytes());
        for row in rows {
            bytes.extend_from_slice(row.as_bytes());
            let visible = crate::status::strip_ansi(row).chars().count();
            if visible != pane_size.0 {
                bytes.extend_from_slice(CLEAR_TO_END.as_bytes());
            }
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(FRAME_END.as_bytes());
        // The whole frame goes out as one `write` call — the acceptance
        // criterion a counting-writer test checks below — but `write` is
        // free to accept fewer bytes than it was given; that is not an
        // error, only a short write. Dropping the remainder here would lose
        // the rest of the frame silently, `ESC[J ESC[?2026l` included, while
        // `self.last` below still records the frame as painted in full, so
        // any remainder still owed after that first call is finished with
        // `write_all` rather than left on the floor.
        if let Ok(n) = out.write(&bytes) {
            let _ = out.write_all(&bytes[n..]);
        }
        let _ = out.flush();
        self.last = Some((rows.to_vec(), pane_size));
    }
}

/// Today's write, frozen exactly as `commands::queue::paint` still did it —
/// erase the whole screen, then one `writeln!` per row — copied here rather
/// than called anywhere in production, so nothing outside tests ever runs
/// the shape this writer replaces. `pub(crate)` and outside `mod tests`
/// below so every redrawing screen's own test module can hold the same
/// proof — a frame it builds must paint the same picture through this and
/// through [`FrameWriter`] — not only the generic cases this module covers
/// on its own.
#[cfg(test)]
pub(crate) fn todays_write(rows: &[String], out: &mut impl std::io::Write) {
    let _ = write!(out, "\x1b[2J\x1b[H");
    for row in rows {
        let _ = writeln!(out, "{row}");
    }
}

/// A real terminal, taken raw by [`crate::platform::TermGuard`], still
/// leaves output processing on — see `platform::raw_mode`, which clears
/// only `ECHO` and `ICANON` — so the kernel's `ONLCR` translates every
/// outgoing `\n` into `\r\n` before it ever reaches the terminal.
/// `vt100::Parser` models the terminal side of that wire, not the
/// kernel's, so a test feeding it these bytes directly has to make the
/// same translation a real tty driver already made, or a bare `\n`
/// reads as "down, same column" instead of "down, column zero" and
/// every row after the first prints at the wrong indent. `pub(crate)` for
/// the same reason as [`todays_write`]: a test outside this module that
/// plays a screen's own captured output through `vt100` needs the same
/// translation, not only the generic cases this module covers on its own.
#[cfg(test)]
pub(crate) fn as_terminal_would_receive(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

/// Plays `old` and `new` into two `vt100` screens sized to `pane_size`
/// (columns, rows) and requires every cell to agree in text, colour and
/// bold — a byte comparison alone could pass on writes with the right
/// shape but the wrong picture, and only an emulator models what a
/// person watching the terminal would actually see. `pub(crate)` for the
/// same reason as [`todays_write`]: every redrawing screen's own
/// `_paints_as_before` test calls this against its own real frame.
///
/// Foreground colour and bold are compared only on cells that hold
/// text, because on a blank cell neither can be seen. Today's `ESC[2J`
/// erases with whatever rendition the previous frame left open, so its
/// blank cells can carry a foreground colour no one sees; matching that
/// would mean tinting the cleared tails instead. Background colour
/// shows on a blank cell, so it is compared on every cell.
#[cfg(test)]
pub(crate) fn assert_same_picture(old: &[u8], new: &[u8], pane_size: (usize, usize)) {
    let (width, height) = (pane_size.0 as u16, pane_size.1 as u16);
    let mut old_parser = vt100::Parser::new(height, width, 0);
    old_parser.process(&as_terminal_would_receive(old));
    let mut new_parser = vt100::Parser::new(height, width, 0);
    new_parser.process(&as_terminal_would_receive(new));

    for row in 0..height {
        for col in 0..width {
            let old_cell = old_parser.screen().cell(row, col);
            let new_cell = new_parser.screen().cell(row, col);
            assert_eq!(
                old_cell.map(vt100::Cell::contents),
                new_cell.map(vt100::Cell::contents),
                "text differs at row {row}, col {col}"
            );
            assert_eq!(
                old_cell.map(vt100::Cell::bgcolor),
                new_cell.map(vt100::Cell::bgcolor),
                "background differs at row {row}, col {col}"
            );
            if !old_cell.is_some_and(vt100::Cell::has_contents) {
                continue;
            }
            assert_eq!(
                old_cell.map(vt100::Cell::fgcolor),
                new_cell.map(vt100::Cell::fgcolor),
                "colour differs at row {row}, col {col}"
            );
            assert_eq!(
                old_cell.map(vt100::Cell::bold),
                new_cell.map(vt100::Cell::bold),
                "bold differs at row {row}, col {col}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `rows` through both [`todays_write`] and [`FrameWriter`] at
    /// `pane_size` and requires the two pictures to match — the shared shape
    /// every case below drives.
    fn assert_writer_matches_todays_write(rows: &[String], pane_size: (usize, usize)) {
        let mut old = Vec::new();
        todays_write(rows, &mut old);

        let mut new = Vec::new();
        FrameWriter::new().write_frame(rows, pane_size, &mut new);

        assert_same_picture(&old, &new, pane_size);
    }

    #[test]
    fn an_ordinary_frame_paints_the_same_picture_as_todays_write() {
        assert_writer_matches_todays_write(
            &["one two".to_string(), "three four".to_string()],
            (20, 5),
        );
    }

    #[test]
    fn a_full_width_row_paints_the_same_picture_as_todays_write() {
        assert_writer_matches_todays_write(&["a".repeat(10)], (10, 3));
    }

    /// The board's own green (`State::Running`'s own colour, in
    /// `status::view`) — inlined rather than imported, since that constant
    /// is private to `status::view` and this test only needs the bytes a
    /// coloured row actually carries, not the name behind them.
    const GREEN: &str = "\x1b[32m";

    // Plain rows alone never exercise the colour and bold half of
    // `assert_same_picture` — every cell it looks at is the emulator's own
    // default, so that half of the check could not fail no matter what the
    // writer did to a coloured or bold cell. The second row here leaves its
    // colour open, with no `RESET` before the row ends, as the two carry-over
    // cases below also do.
    #[test]
    fn a_coloured_and_bold_frame_paints_the_same_picture_as_todays_write() {
        let bold_row = format!("{}bold{}", crate::status::BOLD, crate::status::RESET);
        let green_row_left_open = format!("{GREEN}green, no reset");
        assert_writer_matches_todays_write(&[bold_row, green_row_left_open], (20, 4));
    }

    // Today a colour a row leaves open carries on into the next row's text,
    // since nothing resets it in between. The clear after the open row must
    // not stop that, or the next row's text comes out in a different colour.
    #[test]
    fn a_colour_left_open_still_carries_into_the_next_row() {
        let rows = vec![format!("{GREEN}green open"), "plain next".to_string()];
        assert_writer_matches_todays_write(&rows, (20, 4));
    }

    // The same carry-over across frames: today's `ESC[2J` does not reset the
    // rendition, so a colour the last row of one frame leaves open reaches
    // the first row of the next. The clear below the frame must keep that.
    #[test]
    fn a_colour_left_open_at_the_end_of_a_frame_still_reaches_the_next_frame() {
        let pane_size = (20, 4);
        let first = vec![format!("{GREEN}green open")];
        let second = vec!["plain first".to_string(), "plain second".to_string()];

        let mut old = Vec::new();
        todays_write(&first, &mut old);
        todays_write(&second, &mut old);

        let mut writer = FrameWriter::new();
        let mut new = Vec::new();
        writer.write_frame(&first, pane_size, &mut new);
        writer.write_frame(&second, pane_size, &mut new);

        assert_same_picture(&old, &new, pane_size);
    }

    // A shorter frame written after a longer one must not leave the longer
    // one's extra rows on screen — `ESC[J` in the new writer covers exactly
    // what the old writer's `ESC[2J` erase used to.
    #[test]
    fn a_shorter_frame_after_a_longer_one_clears_what_the_longer_one_left() {
        let pane_size = (10, 5);
        let longer = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        let shorter = vec!["only".to_string()];

        let mut old = Vec::new();
        todays_write(&longer, &mut old);
        todays_write(&shorter, &mut old);

        let mut writer = FrameWriter::new();
        let mut new = Vec::new();
        writer.write_frame(&longer, pane_size, &mut new);
        writer.write_frame(&shorter, pane_size, &mut new);

        assert_same_picture(&old, &new, pane_size);
    }

    // A frame with nothing in common with the one before it — as a tab
    // switch draws — must still come out looking like today's fresh erase
    // and redraw, not like the previous frame showing through.
    #[test]
    fn a_completely_different_frame_paints_the_same_picture_as_todays_write() {
        let pane_size = (10, 5);
        let first = vec!["aaaa".to_string(), "bbbb".to_string()];
        let second = vec![
            "zzzzzzzzzz".to_string(),
            "yyyy".to_string(),
            "x".to_string(),
        ];

        let mut old = Vec::new();
        todays_write(&first, &mut old);
        todays_write(&second, &mut old);

        let mut writer = FrameWriter::new();
        let mut new = Vec::new();
        writer.write_frame(&first, pane_size, &mut new);
        writer.write_frame(&second, pane_size, &mut new);

        assert_same_picture(&old, &new, pane_size);
    }

    // A row exactly as wide as the pane gets no `ESC[K`: the cursor is still
    // sitting on that row's own last column, and `ESC[K` there erases it in
    // most terminals rather than leaving it be.
    #[test]
    fn a_full_width_row_gets_no_clear_to_end() {
        let mut out = Vec::new();
        let mut writer = FrameWriter::new();
        writer.write_frame(&["abcde".to_string()], (5, 3), &mut out);
        assert_eq!(
            out,
            b"\x1b[?2026h\x1b[Habcde\n\x1b7\x1b[0m\x1b[J\x1b8\x1b[?2026l".to_vec()
        );
    }

    // Visible width is counted with escape codes left out, the same way
    // `crate::status::strip_ansi` already measures a row elsewhere — a
    // coloured row whose visible text reaches the pane's edge must be
    // treated as full width too, not as overflowing it by the length of its
    // colour codes.
    #[test]
    fn a_coloured_full_width_row_is_measured_by_its_visible_width() {
        let mut out = Vec::new();
        let mut writer = FrameWriter::new();
        let row = format!("{}abcde{}", crate::status::DIM, crate::status::RESET);
        writer.write_frame(std::slice::from_ref(&row), (5, 3), &mut out);
        let expected = format!("\x1b[?2026h\x1b[H{row}\n\x1b7\x1b[0m\x1b[J\x1b8\x1b[?2026l");
        assert_eq!(out, expected.into_bytes());
    }

    // A screen that redraws on every poll tick, whether or not anything
    // moved, must not write a byte on the ticks where the picture would be
    // identical to what is already on screen.
    #[test]
    fn the_same_rows_at_the_same_pane_size_write_nothing() {
        let mut out = Vec::new();
        let mut writer = FrameWriter::new();
        let rows = vec!["one".to_string()];
        writer.write_frame(&rows, (10, 5), &mut out);
        let after_first = out.len();
        writer.write_frame(&rows, (10, 5), &mut out);
        assert_eq!(
            out.len(),
            after_first,
            "an unchanged frame must not write anything more"
        );
    }

    // The same rows at a different pane size must still repaint — a resize
    // has to redraw even when the text itself did not change.
    #[test]
    fn the_same_rows_at_a_new_pane_size_write_again() {
        let mut out = Vec::new();
        let mut writer = FrameWriter::new();
        let rows = vec!["one".to_string()];
        writer.write_frame(&rows, (10, 5), &mut out);
        let after_first = out.len();
        writer.write_frame(&rows, (12, 5), &mut out);
        assert!(
            out.len() > after_first,
            "a pane resize must repaint even an unchanged frame"
        );
    }

    // `forget` is for a screen that draws something outside this writer's
    // own memory — a tab switch, say — so the very next frame must always be
    // written, even if it happens to match what was last painted.
    #[test]
    fn forget_makes_the_next_identical_frame_write_again() {
        let mut out = Vec::new();
        let mut writer = FrameWriter::new();
        let rows = vec!["one".to_string()];
        writer.write_frame(&rows, (10, 5), &mut out);
        let after_first = out.len();
        writer.forget();
        writer.write_frame(&rows, (10, 5), &mut out);
        assert!(
            out.len() > after_first,
            "forget must make the next identical frame write again"
        );
    }

    /// The scenario `forget` exists for, made concrete: a tab's own frame is
    /// painted, then something draws over it *outside* this writer's own
    /// accounting — a one-shot popup, or another tab entirely, neither of
    /// which updates what this writer remembers as "the last frame" — and
    /// then the very same tab draws its own frame again, unchanged from what
    /// it drew before the outside draw. Without `forget`, this writer still
    /// believes that frame is already on screen and skips the write,
    /// leaving whatever drew outside it in view; `forget` between the two is
    /// what makes the second draw repaint over it regardless. Proven two
    /// ways: the second `write_frame` call must actually emit bytes, and
    /// `vt100` must show the final screen as the tab's own frame alone, with
    /// nothing the outside draw left still showing.
    #[test]
    fn tab_switch_leaves_nothing_behind() {
        let pane_size = (20, 5);
        let tab_rows = vec!["tab A, row one".to_string(), "tab A, row two".to_string()];
        let mut writer = FrameWriter::new();
        let mut out = Vec::new();

        writer.write_frame(&tab_rows, pane_size, &mut out);
        let after_first = out.len();

        // Something drawn outside this writer's own accounting — a one-shot
        // popup or another tab's own frame — landing over the same rows,
        // the way a real switch would.
        let _ = std::io::Write::write_all(&mut out, b"\x1b[Hsomething else entirely");

        writer.forget();
        writer.write_frame(&tab_rows, pane_size, &mut out);
        assert!(
            out.len() > after_first,
            "forget must make the tab's own frame repaint over whatever drew outside it"
        );

        let mut parser = vt100::Parser::new(pane_size.1 as u16, pane_size.0 as u16, 0);
        parser.process(&as_terminal_would_receive(&out));
        let screen = parser.screen().contents();
        assert!(
            screen.contains("tab A, row one") && screen.contains("tab A, row two"),
            "the tab's own frame must be back on screen:\n{screen}"
        );
        assert!(
            !screen.contains("something else entirely"),
            "nothing the outside draw left must still show:\n{screen}"
        );
    }

    /// Counts calls to `write`, rather than bytes, so a test can check the
    /// writer's own promise that a frame goes out as exactly one `write`
    /// call — the whole point of gathering a frame into one buffer first,
    /// since a slow terminal can paint the gap between two separate writes
    /// even where a `Vec` capturing both back to back never would.
    #[derive(Default)]
    struct CountingWriter {
        calls: usize,
        bytes: Vec<u8>,
    }

    impl std::io::Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn one_frame_is_exactly_one_write_call() {
        let mut out = CountingWriter::default();
        let mut writer = FrameWriter::new();
        writer.write_frame(&["one".to_string()], (10, 5), &mut out);
        assert_eq!(out.calls, 1);
    }

    #[test]
    fn an_unchanged_frame_makes_no_write_call_at_all() {
        let mut out = CountingWriter::default();
        let mut writer = FrameWriter::new();
        let rows = vec!["one".to_string()];
        writer.write_frame(&rows, (10, 5), &mut out);
        writer.write_frame(&rows, (10, 5), &mut out);
        assert_eq!(out.calls, 1);
    }

    #[test]
    fn the_same_rows_at_a_new_pane_size_makes_a_second_write_call() {
        let mut out = CountingWriter::default();
        let mut writer = FrameWriter::new();
        let rows = vec!["one".to_string()];
        writer.write_frame(&rows, (10, 5), &mut out);
        writer.write_frame(&rows, (12, 5), &mut out);
        assert_eq!(out.calls, 2);
    }

    #[test]
    fn a_frame_after_forget_makes_a_second_write_call() {
        let mut out = CountingWriter::default();
        let mut writer = FrameWriter::new();
        let rows = vec!["one".to_string()];
        writer.write_frame(&rows, (10, 5), &mut out);
        writer.forget();
        writer.write_frame(&rows, (10, 5), &mut out);
        assert_eq!(out.calls, 2);
    }

    /// A writer that only ever accepts the first `cap` bytes of any one
    /// `write` call — a short write, the kind a signal delivered mid-write
    /// (`SIGWINCH` on a resize, most plausibly here) or a `LineWriter`
    /// splitting on the frame's own embedded `\n`s can hand back on real
    /// stdout. The frame's own `Vec` is always ready in one piece, so
    /// nothing in the writer itself forces this the way a real fd would —
    /// this stands in for that.
    struct ShortWriter {
        cap: usize,
        bytes: Vec<u8>,
    }

    impl std::io::Write for ShortWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.cap);
            self.bytes.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // A `write` call is free to accept fewer bytes than it was given — that
    // is not an error, and dropping the rest would silently lose the tail of
    // the frame, `ESC[J ESC[?2026l` included, while still recording the
    // frame as painted. The writer has to finish a short write itself.
    #[test]
    fn a_short_write_is_finished_rather_than_dropped() {
        let mut out = ShortWriter {
            cap: 3,
            bytes: Vec::new(),
        };
        let mut writer = FrameWriter::new();
        writer.write_frame(&["one".to_string(), "two".to_string()], (10, 5), &mut out);
        assert_eq!(
            out.bytes,
            b"\x1b[?2026h\x1b[Hone\x1b7\x1b[0m\x1b[K\x1b8\ntwo\x1b7\x1b[0m\x1b[K\x1b8\n\x1b7\x1b[0m\x1b[J\x1b8\x1b[?2026l"
                .to_vec()
        );
    }

    #[test]
    fn an_ordinary_frame_is_sync_start_each_row_cleared_then_sync_end() {
        let mut out = Vec::new();
        let mut writer = FrameWriter::new();
        writer.write_frame(&["one".to_string(), "two".to_string()], (10, 5), &mut out);
        assert_eq!(
            out,
            b"\x1b[?2026h\x1b[Hone\x1b7\x1b[0m\x1b[K\x1b8\ntwo\x1b7\x1b[0m\x1b[K\x1b8\n\x1b7\x1b[0m\x1b[J\x1b8\x1b[?2026l"
                .to_vec()
        );
    }
}
