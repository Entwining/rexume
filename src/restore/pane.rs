use serde_json::{Value, json};
use unicode_width::UnicodeWidthChar;

use crate::rex::{CallError, Rex, items, str_at};

#[derive(Debug, PartialEq, Eq)]
pub enum Pane {
    /// `typed_after`: text other than a right-hand prompt follows the cursor on its row, which a
    /// close would lose; the gate's line clears it except in a vi keymap.
    Shell {
        typed_after: bool,
    },
    Busy(String),
    Gone,
}

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
    // A line editor reads keys with canonical mode off; with it on, the shell still runs its
    // startup files, and the terminal holds a typed line for its first prompt.
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

/// The login shell Rex starts has at most one ancestor, the terminal's own program (`login` on
/// macOS).
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

/// In zsh, Ctrl-E and Ctrl-U clear only the current buffer line, so lines below would run with
/// the gate.
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

/// A right-hand prompt is set off from the cursor by a blank and ends in the row's last two
/// columns: zsh leaves the last one blank by default.
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

fn cursor(vt: &str) -> Option<(usize, usize)> {
    let report = vt.strip_suffix('H')?;
    let start = report.rfind("\u{1b}[")?;
    let (row, column) = report[start + 2..].split_once(';')?;
    Some((
        row.parse::<usize>().ok()?.checked_sub(1)?,
        column.parse::<usize>().ok()?.checked_sub(1)?,
    ))
}

/// In zsh and bash emacs keymaps, Ctrl-E then Ctrl-U clears the current line and saves it for
/// Ctrl-Y; vi insert mode leaves text after the cursor to run with the gate.
/// Before the first prompt, the terminal's Ctrl-U clears pending input, including Ctrl-E.
pub fn line(gate: &[String]) -> Result<String, shlex::QuoteError> {
    let command = shlex::try_join(gate.iter().map(String::as_str))?;
    Ok(format!("\u{5}\u{15} {command}\r"))
}

#[cfg(test)]
mod tests;
