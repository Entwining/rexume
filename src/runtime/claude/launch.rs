//! `docs/cli.md`'s `report` section specifies what this keeps of Claude Code's launch, and why.

use serde_json::{Map, Value};

use crate::proc;

/// A flag's values as Commander, Claude Code's argument parser, reads them.
enum Arity {
    None,
    Optional,
    One,
    Many,
}

const FLAGS: &[(&str, Arity)] = &[
    ("--remote-control", Arity::Optional),
    ("--rc", Arity::Optional),
    ("--add-dir", Arity::Many),
    ("--mcp-config", Arity::Many),
    ("--allowedTools", Arity::Many),
    ("--allowed-tools", Arity::Many),
    ("--disallowedTools", Arity::Many),
    ("--disallowed-tools", Arity::Many),
    ("--tools", Arity::Many),
    ("--settings", Arity::One),
    ("--plugin-dir", Arity::One),
    ("--setting-sources", Arity::One),
    ("--strict-mcp-config", Arity::None),
    ("--chrome", Arity::None),
    ("--no-chrome", Arity::None),
];

const ENV: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_PROFILE",
    "ANTHROPIC_BEDROCK_REGION_PREFIX",
    "ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION",
    "CLOUD_ML_REGION",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_FOUNDRY_RESOURCE",
    "ANTHROPIC_AWS_WORKSPACE_ID",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
];

const ENV_FAMILY: &str = "VERTEX_REGION_CLAUDE_";

const OWNER: &str = "REXUME_LAUNCH_PID";
const COMMAND: &str = "REXUME_LAUNCH";

pub fn patch(launch: &proc::Launch, pid: u32) -> Map<String, Value> {
    let list = |values: Vec<String>| {
        if values.is_empty() {
            Value::Null
        } else {
            values.into()
        }
    };
    let (command, error, env) = match declaration(launch, pid) {
        None => (Value::Null, Value::Null, list(env(&launch.env))),
        Some(Ok(argv)) => (argv.into(), Value::Null, Value::Null),
        Some(Err(reason)) => (Value::Null, reason.into(), Value::Null),
    };
    [
        ("launch_command", command),
        ("launch_error", error),
        ("launch_flags", list(flags(&launch.argv))),
        ("launch_env", env),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect()
}

fn declaration(launch: &proc::Launch, pid: u32) -> Option<Result<Vec<String>, String>> {
    if launch.var(OWNER)?.parse::<u32>().ok()? != pid {
        return None;
    }
    // The gate claims the restore before its shell runs the command, so one that cannot run there
    // stops here: `[]` would reopen as `claude`, a NUL cannot be quoted for the shell, and a first
    // element with `=` is a variable assignment.
    let argv = launch
        .var(COMMAND)
        .and_then(|text| serde_json::from_str::<Vec<String>>(text).ok())
        .filter(|argv| {
            argv.first()
                .is_some_and(|program| !program.is_empty() && !program.contains('='))
                && argv.iter().all(|arg| !arg.contains('\0'))
        });
    Some(argv.ok_or_else(|| {
        format!(
            "`{COMMAND}` is missing or not a JSON array of strings without NUL whose first element \
             names a program"
        )
    }))
}

fn flags(argv: &[String]) -> Vec<String> {
    let is_option = |arg: &&String| arg.len() > 1 && arg.starts_with('-');
    // Inline JSON for `--settings` or `--mcp-config` can hold a token; a path cannot.
    let is_inline_json = |name: &str, value: &str| {
        matches!(name, "--settings" | "--mcp-config") && value.trim_start().starts_with(['{', '['])
    };
    let mut kept = Vec::new();
    let mut args = argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let (name, attached) = arg.split_once('=').unzip();
        let name = name.unwrap_or(arg);
        let Some((_, arity)) = FLAGS.iter().find(|(flag, _)| *flag == name) else {
            continue;
        };
        if let Some(value) = attached {
            if !is_inline_json(name, value) {
                kept.push(arg.clone());
            }
            continue;
        }
        let taken: Vec<&String> = match arity {
            Arity::None => Vec::new(),
            Arity::Optional => args.next_if(|arg| !is_option(arg)).into_iter().collect(),
            Arity::One => args.next().into_iter().collect(),
            Arity::Many => args
                .next()
                .into_iter()
                .chain(std::iter::from_fn(|| args.next_if(|arg| !is_option(arg))))
                .collect(),
        };
        let values: Vec<&String> = taken
            .into_iter()
            .filter(|v| !is_inline_json(name, v))
            .collect();
        if values.is_empty() && matches!(arity, Arity::One | Arity::Many) {
            continue;
        }
        kept.push(arg.clone());
        kept.extend(values.into_iter().cloned());
    }
    kept
}

fn env(env: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = env
        .iter()
        .filter(|entry| {
            entry
                .split_once('=')
                .is_some_and(|(name, _)| ENV.contains(&name) || name.starts_with(ENV_FAMILY))
        })
        .cloned()
        .collect();
    kept.sort();
    kept
}

#[cfg(test)]
mod tests;
