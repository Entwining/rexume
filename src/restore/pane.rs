//! Whether a pane the server restored still shows the login shell it started there, read from the
//! server before anything is typed into it, and the line typed.

use serde_json::{Value, json};
use unicode_width::UnicodeWidthChar;

use crate::rex::{CallError, Rex, items, str_at};

/// What the pane shows.
#[derive(Debug, PartialEq, Eq)]
pub enum Pane {
    /// The login shell is the pane's foreground and takes a line now: it still reads its startup
    /// files, and a line typed now waits in the terminal for its first prompt, or its prompt shows
    /// with the cursor after a blank and nothing on the rows below. `typed_after` is whether text
    /// other than a right-hand prompt follows the cursor on its row, which the line clears but a
    /// close would lose.
    Shell {
        typed_after: bool,
    },
    /// Something else shows in the pane, as the reason says.
    Busy(String),
    Gone,
}

/// Reads the pane from the server. A refusal other than a missing pane is the caller's to report.
pub fn read(rex: &mut Rex, session: &str, block: &str) -> Result<Pane, CallError> {
    let mut call =
        |method: &str, args: Value| match rex.block_method_with(session, block, method, &args) {
            Err(e) if e.not_found() => Ok(None),
            result => result.map(Some),
        };
    let Some(process) = call("process", json!({}))? else {
        return Ok(Pane::Gone);
    };
    if let Some(busy) = foreground(&process) {
        return Ok(Pane::Busy(busy));
    }
    // A line editor reads keys with the terminal's canonical mode off; with it on, the shell still
    // runs its startup files.
    let Some(input) = call("input_mode", json!({}))? else {
        return Ok(Pane::Gone);
    };
    if input.get("canonical").and_then(Value::as_bool) == Some(true) {
        return Ok(Pane::Shell { typed_after: false });
    }
    let (Some(text), Some(vt)) = (
        call("format", json!({}))?,
        call("format", json!({ "format": "vt", "extra_cursor": true }))?,
    ) else {
        return Ok(Pane::Gone);
    };
    let text = str_at(&text, "/content").unwrap_or("");
    Ok(
        match at_prompt(text, str_at(&vt, "/content").unwrap_or("")) {
            None => Pane::Busy("the cursor of the pane could not be read".into()),
            Some(None) => Pane::Busy(
                "text is typed in the pane, or its prompt does not end in a space".into(),
            ),
            Some(Some((_, after))) if after.trim().is_empty() => Pane::Shell { typed_after: false },
            Some(Some((row, after))) => {
                let Some(size) = call("size", json!({}))? else {
                    return Ok(Pane::Gone);
                };
                let columns = size
                    .get("columns")
                    .and_then(Value::as_u64)
                    .and_then(|c| usize::try_from(c).ok());
                Pane::Shell {
                    typed_after: typed_after(row, after, columns),
                }
            }
        },
    )
}

/// Why the foreground is not the login shell the server started in the pane, or `None` when it
/// is: a process named as a login shell is (`-zsh`), whose parent, if any, is the terminal's own
/// program (`login` on macOS).
fn foreground(process: &Value) -> Option<String> {
    if !process.get("last_exit").is_none_or(Value::is_null) {
        return Some("the pane's shell has exited".into());
    }
    let Some(foreground) = process.get("foreground").filter(|f| f.is_object()) else {
        return Some("no program runs in the pane yet".into());
    };
    let name = str_at(foreground, "/argv0")
        .or(str_at(foreground, "/name"))
        .unwrap_or("a program");
    if !name.starts_with('-') || items(process, "/ancestors").len() > 1 {
        return Some(format!("{name} runs in the pane"));
    }
    None
}

/// The cursor's row and what follows the cursor on it, when the cell before the cursor is blank
/// and nothing shows on the rows below; `Some(None)` otherwise: text typed at the prompt, a line
/// of several the cursor was moved back to, which the line's Ctrl-U would not clear with the
/// others, or a prompt that does not end in a space. Each character takes its own width, as the
/// terminal lays them out. `text` is the screen's rows; `None` when `vt`, the screen as escape
/// sequences with the cursor's position last, does not give that position.
fn at_prompt<'a>(text: &'a str, vt: &str) -> Option<Option<(&'a str, &'a str)>> {
    let (row, column) = cursor(vt)?;
    let mut rows = text.lines().skip(row);
    let line = rows.next().unwrap_or("");
    let mut width = 0;
    let mut before = None;
    let mut at = line.len();
    for (index, c) in line.char_indices() {
        let next = width + c.width().unwrap_or(0);
        if next > column {
            at = index;
            break;
        }
        before = Some(c);
        width = next;
    }
    let blank = width < column || before.is_some_and(char::is_whitespace);
    Some((blank && rows.all(|r| r.trim().is_empty())).then(|| (line, &line[at..])))
}

/// Whether `after`, what follows the cursor on its row `line`, is typed text rather than a
/// right-hand prompt, which a blank separates from the cursor and which ends in the row's last two
/// columns (zsh leaves the last one blank by default); without the pane's width `columns`, it is
/// typed. A shell that measures a character otherwise than the terminal places its right-hand
/// prompt short of the row's end, which counts as typed.
fn typed_after(line: &str, after: &str, columns: Option<usize>) -> bool {
    let rest = after.trim_start();
    let width: usize = line
        .trim_end()
        .chars()
        .map(|c| c.width().unwrap_or(0))
        .sum();
    !rest.is_empty()
        && !(rest.len() < after.len() && columns.is_some_and(|columns| width + 1 >= columns))
}

/// The cursor's row and column, counted from 0, from the position report that ends `vt` (`ESC [
/// row ; column H`).
fn cursor(vt: &str) -> Option<(usize, usize)> {
    let report = vt.strip_suffix('H')?;
    let start = report.rfind("\u{1b}[")?;
    let (row, column) = report[start + 2..].split_once(';')?;
    Some((
        row.parse::<usize>().ok()?.checked_sub(1)?,
        column.parse::<usize>().ok()?.checked_sub(1)?,
    ))
}

/// The line typed into the pane: a move to the end of the line (Ctrl-E) and a kill of the line
/// (Ctrl-U), so that what the check does not read, after the cursor, or typed since, is cleared
/// rather than run with the gate, a leading space that keeps the line out of a history that
/// ignores such lines, the gate's command, and Return. Ctrl-U in bash kills only what stands
/// before the cursor, and both shells' emacs keymaps keep the whole kill for Ctrl-Y; their vi
/// insert modes, and the terminal before the first prompt, take Ctrl-E as a character, which
/// Ctrl-U kills with the rest.
pub fn line(gate: &[String]) -> Result<String, shlex::QuoteError> {
    let command = shlex::try_join(gate.iter().map(String::as_str))?;
    Ok(format!("\u{5}\u{15} {command}\r"))
}

#[cfg(test)]
mod tests;
