use super::*;

/// The screens a fresh zsh showed on a test server, with a right-hand prompt: as text, and the
/// cursor's position report that ends them as escape sequences.
const IDLE: (&str, &str) = (
    "Last login: Sat Oct 31 20:35:51 on ttys013\np%                                                                         [rp]",
    "p% \u{1b}[72C[rp]\r\n\r\n\u{1b}[2;4H",
);
const TYPED: (&str, &str) = (
    "Last login: Sat Oct 31 20:35:51 on ttys013\np% echo typed                                                              [rp]",
    "p% echo typed\u{1b}[62C[rp]\r\n\r\n\u{1b}[2;14H",
);

#[test]
fn text_typed_at_the_prompt_shows_before_the_cursor() {
    let columns = Some(80);
    assert_eq!(typed_at_prompt(IDLE.0, IDLE.1, columns), Some(false));
    assert_eq!(typed_at_prompt(TYPED.0, TYPED.1, columns), Some(true));
    // Wide characters take two cells each.
    assert_eq!(
        typed_at_prompt("p% 日本", "\u{1b}[1;8H", columns),
        Some(true)
    );
    assert_eq!(
        typed_at_prompt("日本 ", "\u{1b}[1;6H", columns),
        Some(false)
    );
    // Before the first prompt: text the terminal echoes, or only the startup files' output.
    assert_eq!(
        typed_at_prompt("Last login\nls", "\u{1b}[2;3H", columns),
        Some(true)
    );
    assert_eq!(
        typed_at_prompt("Last login", "\u{1b}[2;1H", columns),
        Some(false)
    );
    assert_eq!(
        typed_at_prompt("Last login\n", "\u{1b}[2;1H", columns),
        Some(false)
    );
    // A screen without the position report says nothing.
    assert_eq!(typed_at_prompt(IDLE.0, "p% ", columns), None);
}

/// Text typed and then moved past, as by Ctrl-A, stands after the cursor; a right-hand prompt
/// stands there too, apart from the cursor and at the row's end.
#[test]
fn text_after_the_cursor_is_typed_unless_it_is_a_right_hand_prompt() {
    let columns = Some(80);
    let at_start = "\u{1b}[2;4H";
    assert_eq!(
        typed_at_prompt("Last login\np% :; echo ran", at_start, columns),
        Some(true)
    );
    // Typed text that reaches the row's end and wraps onto the next row.
    let wrapped = format!("p% {}\n{}", "x".repeat(77), "y");
    assert_eq!(
        typed_at_prompt(&wrapped, "\u{1b}[1;4H", columns),
        Some(true)
    );
    let spaced = format!("p% {}{}\ny", " ".repeat(4), "x".repeat(73));
    assert_eq!(typed_at_prompt(&spaced, "\u{1b}[1;4H", columns), Some(true));
    // Typed blanks that fill the row, before text that wraps onto the next.
    let blanks = format!("p% {}\n; echo ran", " ".repeat(77));
    assert_eq!(typed_at_prompt(&blanks, "\u{1b}[1;4H", columns), Some(true));
    // The right-hand prompt alone, unless the pane's width is unknown or the prompt falls short of
    // the row's end.
    assert_eq!(typed_at_prompt(IDLE.0, IDLE.1, None), Some(true));
    assert_eq!(typed_at_prompt(IDLE.0, IDLE.1, Some(100)), Some(true));
}

#[test]
fn only_the_login_shell_the_server_started_is_the_shell() {
    let process = |foreground: Value, ancestors: Value, last_exit: Value| json!({ "foreground": foreground, "ancestors": ancestors, "last_exit": last_exit });
    let shell = json!({ "pid": 2, "argv0": "-zsh", "name": "zsh" });
    assert_eq!(
        foreground(&process(shell.clone(), json!(["login"]), Value::Null)),
        None
    );
    assert!(foreground(&process(shell, json!(["login"]), json!(0))).is_some());
    // A command the shell's startup files run, and a shell started from the login shell.
    let sleep = json!({ "pid": 3, "argv0": "sleep", "name": "sleep" });
    assert!(foreground(&process(sleep, json!(["zsh", "login"]), Value::Null)).is_some());
    let nested = json!({ "pid": 4, "argv0": "-zsh", "name": "zsh" });
    assert!(foreground(&process(nested, json!(["zsh", "login"]), Value::Null)).is_some());
    assert!(foreground(&process(Value::Null, json!([]), Value::Null)).is_some());
}

#[test]
fn the_line_kills_the_whole_line_before_the_gate() -> Result<(), shlex::QuoteError> {
    let gate = [
        "/opt/rexume dir/rexume".to_owned(),
        "gate".to_owned(),
        "r".to_owned(),
    ];
    assert_eq!(
        line(&gate)?,
        "\u{5}\u{15} '/opt/rexume dir/rexume' gate r\r"
    );
    Ok(())
}
