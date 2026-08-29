//! Questions `init` asks, and what it does when there is nobody to ask.
//!
//! Every question here has a defensible default, so nothing in this module can
//! block: with no terminal on the other end — a script, a pipe, CI — the
//! default is taken silently and the run is exactly what it was before any of
//! this existed. That is the property the e2e suites depend on, and it is
//! checked here rather than remembered at each call site: [`interactive`] is
//! the only thing that decides, and every function below goes through it.
//!
//! Deliberately hand-rolled rather than a prompting crate. Three questions
//! asked once, at the one moment a project is created, do not justify a
//! dependency — and a dependency would still have to be taught this module's
//! actual subject, which is not how to draw a menu but when to refuse to.

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

/// One question with a fixed set of answers, as a numbered menu.
///
/// `options` are `(value, note)`; the note is what the value means, shown
/// beside it. Returns the chosen value's index. An empty line takes `default`,
/// and so does anything unrecognised — after saying so, because a person who
/// typed `codx` should see that they got claude rather than find out from the
/// paths in the summary.
pub fn choose(question: &str, options: &[(&str, &str)], default: usize) -> Result<usize> {
    debug_assert!(default < options.len(), "the default must be on the menu");
    if !interactive() {
        return Ok(default);
    }

    println!("{question}");
    let width = options
        .iter()
        .map(|(value, _)| value.len())
        .max()
        .unwrap_or(0);
    for (i, (value, note)) in options.iter().enumerate() {
        let mark = if i == default { "*" } else { " " };
        println!("  {mark} {}) {value:width$}  {note}", i + 1);
    }

    let answer = read(&format!("[1-{}, default {}]", options.len(), default + 1))?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(default);
    }
    // By number, or by name — a person who has read the menu has both in front
    // of them and no reason to think only one of them counts.
    if let Some(index) = answer
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=options.len()).contains(n))
        .map(|n| n - 1)
    {
        return Ok(index);
    }
    if let Some(index) = options
        .iter()
        .position(|(value, _)| value.eq_ignore_ascii_case(answer))
    {
        return Ok(index);
    }

    println!(
        "  (`{answer}` is not on the menu — taking {})",
        options[default].0
    );
    Ok(default)
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
