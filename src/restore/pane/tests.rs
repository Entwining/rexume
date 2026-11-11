use super::*;

/// Screens a fresh zsh with a right-hand prompt showed on a Rex test server.
const IDLE: (&str, &str) = (
    "Last login: Sat Oct 31 20:35:51 on ttys013\np%                                                                         [rp]",
    "p% \u{1b}[72C[rp]\r\n\r\n\u{1b}[2;4H",
);
const TYPED: (&str, &str) = (
    "Last login: Sat Oct 31 20:35:51 on ttys013\np% echo typed                                                              [rp]",
    "p% echo typed\u{1b}[62C[rp]\r\n\r\n\u{1b}[2;14H",
);

/// A ZWJ emoji prompt and a right-hand prompt as a Rex test server laid them out.
const FAMILY: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466} ";

#[test]
fn the_cursor_must_follow_a_blank_with_nothing_below() {
    fn after<'a>((text, vt): (&'a str, &str)) -> Option<Option<&'a str>> {
        at_prompt(text, vt).map(|p| p.map(|(_, after)| after.trim_start()))
    }
    assert_eq!(after(IDLE), Some(Some("[rp]")));
    assert_eq!(after(TYPED), Some(None));
    let idle = format!("{FAMILY}{}[rp]", " ".repeat(63));
    let vt = format!("{FAMILY}\u{1b}[63C[rp]\u{1b}[1;10H");
    assert!(at_prompt(&idle, &vt).is_some_and(|p| p.is_some()));
    let typed = format!("{FAMILY}pwd{}[rp]", " ".repeat(60));
    let vt = format!("{FAMILY}pwd\u{1b}[60C[rp]\u{1b}[1;13H");
    assert_eq!(at_prompt(&typed, &vt), Some(None));
    assert_eq!(after(("p% 日本", "\u{1b}[1;8H")), Some(None));
    assert_eq!(after(("日本 ", "\u{1b}[1;6H")), Some(Some("")));
    assert_eq!(after(("p%", "\u{1b}[1;4H")), Some(Some("")));
    assert_eq!(after(("p% echo a\necho b", "\u{1b}[1;4H")), Some(None));
    assert_eq!(after(("p% echo a\necho b", "\u{1b}[2;1H")), Some(None));
    assert_eq!(after((IDLE.0, "p% ")), None);
}

#[test]
fn only_a_right_hand_prompt_may_follow_the_cursor() {
    let row = IDLE.0.lines().nth(1).unwrap_or_default();
    let (_, after) = row.split_at(3);
    assert!(!typed_after(row, after, Some(80)));
    assert!(typed_after(row, after, None));
    assert!(typed_after("p% echo kept", "echo kept", Some(80)));
    assert!(typed_after("p%  echo kept", " echo kept", Some(80)));
    assert!(!typed_after("p% ", "", None));
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
