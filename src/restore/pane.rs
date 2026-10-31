//! Whether a pane the server restored still shows the login shell it started there, read from the
//! server before anything is typed into it, and the line typed.

use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;

use crate::rex::{CallError, Rex, items, str_at};

/// What the pane shows.
#[derive(Debug, PartialEq, Eq)]
pub enum Pane {
    /// The login shell is the pane's foreground and nothing typed shows before the cursor, at its
    /// prompt or, while the shell still reads its startup files, where the terminal echoes what it
    /// holds for the shell's first prompt.
    Shell,
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
    let Some(modes) = call("modes", json!({}))? else {
        return Ok(Pane::Gone);
    };
    if str_at(&modes, "/screen") != Some("primary") {
        return Ok(Pane::Busy("a full-screen program shows in the pane".into()));
    }
    let (Some(text), Some(vt), Some(size)) = (
        call("format", json!({}))?,
        call("format", json!({ "format": "vt", "extra_cursor": true }))?,
        call("size", json!({}))?,
    ) else {
        return Ok(Pane::Gone);
    };
    let columns = size
        .get("columns")
        .and_then(Value::as_u64)
        .and_then(|c| usize::try_from(c).ok());
    Ok(
        match typed_at_prompt(
            str_at(&text, "/content").unwrap_or(""),
            str_at(&vt, "/content").unwrap_or(""),
            columns,
        ) {
            Some(false) => Pane::Shell,
            Some(true) => Pane::Busy(
                "text is typed in the pane, or its prompt does not end in a space".into(),
            ),
            None => Pane::Busy("the cursor of the pane could not be read".into()),
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

/// Whether the screen shows typed text at the cursor: the cell just before the cursor holds a
/// character, text follows the cursor, as after typing and moving back to the line's start, or a
/// row below holds something, as a typed line that wraps does. A prompt ends in a space, and text
/// typed after it in something else. Text after the cursor is a right-hand prompt only when a
/// blank separates it from the cursor and it ends in the row's last two columns (zsh leaves the
/// last one blank by default); without the pane's width `columns`, it counts as typed. Before the
/// first prompt, the cursor starts a line after the startup files' output. `text` is the screen's
/// rows; `None` when `vt`, the screen as escape sequences with the cursor's position last, does
/// not give that position.
fn typed_at_prompt(text: &str, vt: &str, columns: Option<usize>) -> Option<bool> {
    let (row, column) = cursor(vt)?;
    let rows: Vec<&str> = text.lines().collect();
    let line = rows.get(row).copied().unwrap_or("");
    let mut width = 0;
    let mut before = None;
    let mut at = line.len();
    for (index, c) in line.char_indices() {
        let next = width + line[index..index + c.len_utf8()].width();
        if next > column {
            at = index;
            break;
        }
        before = Some(c);
        width = next;
    }
    if width == column && before.is_some_and(|c| !c.is_whitespace()) {
        return Some(true);
    }
    let below = rows.iter().skip(row + 1).all(|r| r.trim().is_empty());
    let after = &line[at..];
    let rest = after.trim_start();
    if rest.is_empty() {
        return Some(!below);
    }
    let right_prompt = rest.len() < after.len()
        && columns.is_some_and(|columns| line.trim_end().width() + 1 >= columns)
        && below;
    Some(!right_prompt)
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
/// (Ctrl-U), so that nothing the check could not see, or took for a right-hand prompt, runs with
/// the gate, a leading space that keeps the line out of a history that ignores such lines, the
/// gate's command, and Return. Ctrl-U in bash kills only what stands before the cursor, and both
/// shells' emacs keymaps keep the whole kill for Ctrl-Y; their vi insert modes, and the terminal
/// before the first prompt, take Ctrl-E as a character, which Ctrl-U kills with the rest.
pub fn line(gate: &[String]) -> Result<String, shlex::QuoteError> {
    let command = shlex::try_join(gate.iter().map(String::as_str))?;
    Ok(format!("\u{5}\u{15} {command}\r"))
}

#[cfg(test)]
mod tests;
