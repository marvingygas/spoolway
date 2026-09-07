//! Questions `init` asks, and what it does when there is nobody to ask.
//!
//! Every question here has a defensible default, so nothing in this module can
//! block: with no terminal on the other end — a script, a pipe, CI — the
//! default is taken silently and the run is exactly what it was before any of
//! this existed. That is the property the e2e suites depend on, and it is
//! checked here rather than remembered at each call site: [`interactive`] is
//! the only thing that decides, and every function below goes through it.
//!
//! Fixed choices use a terminal selector so a person can move the highlighted
//! answer with the arrow keys and accept it, while free text remains an
//! ordinary line. The decision about whether anyone is there to answer still
//! belongs here rather than to the prompting library.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::Result;

/// Whether there is a person here to answer.
///
/// Both halves, and both for the same reason: stdin not being a terminal means
/// an answer can never arrive, and stdout not being one means the question is
/// going somewhere nobody is reading. A pipeline of the form `spoolway init |
/// tee log` has a person at the keyboard and no visible prompt, which is a
/// hang; refusing to ask makes it a default instead.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// One question with a fixed set of answers, as an interactive selector.
///
/// `options` are `(value, note)`; the note is what the value means, shown
/// beside it. The default starts highlighted, arrow keys move the highlight,
/// and Enter or Space accepts it. Returns the chosen value's index.
pub fn choose(question: &str, options: &[(&str, &str)], default: usize) -> Result<usize> {
    debug_assert!(default < options.len(), "the default must be on the menu");
    if !interactive() {
        return Ok(default);
    }

    let width = options
        .iter()
        .map(|(value, _)| value.len())
        .max()
        .unwrap_or(0);
    let items: Vec<String> = options
        .iter()
        .map(|(value, note)| format!("{value:width$}  {note}"))
        .collect();
    let term = dialoguer::console::Term::stdout();
    Ok(dialoguer::Select::new()
        .with_prompt(question)
        .items(&items)
        .default(default)
        .interact_on(&term)?)
}

/// One question with a free-text answer, or `None` for an empty one.
///
/// `None` rather than a default value because the caller that asks this — the
/// model name — has no default it could invent: spoolway names no model, and a
/// blank answer has to stay blank so that the placeholder the pipelines ship
/// with survives to be reported as unset.
pub fn line(question: &str, hint: &str) -> Result<Option<String>> {
    if !interactive() {
        return Ok(None);
    }
    println!("{question}");
    let answer = read(hint)?;
    let answer = answer.trim().to_string();
    Ok(Some(answer).filter(|a| !a.is_empty()))
}

/// Write the prompt, flush it — an unflushed prompt is an invisible one, and a
/// person waiting at a blank screen cannot tell that from a hang — and take the
/// line.
///
/// EOF is not an error: a terminal that closed mid-question is a caller that
/// gets its default, the same as one that was never a terminal at all.
fn read(hint: &str) -> Result<String> {
    print!("  > {hint} ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    println!();
    Ok(answer)
}
