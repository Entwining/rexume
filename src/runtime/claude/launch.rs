//! What Claude Code was started with that reopening it needs again: the command a launcher
//! declared, launch flags, and provider variables, never the launch prompt or a variable that
//! holds a credential.

use serde_json::{Map, Value};

use crate::proc;

/// How many values a flag takes, as Claude Code's parser, Commander, reads them: an optional
/// value is the next argument unless that is an option, a required one is the next argument
/// whatever it is, and a variadic list goes on until the next option. A value attached with `=`
/// is the only one.
enum Arity {
    None,
    Optional,
    One,
    Many,
}

/// Flags that shape the session rather than say what it is about: each names a path, a name, a
/// switch, or a tool rule. Prompts, the session's identity, and what the record already carries
/// (`--model`, `--effort`, `--permission-mode`) are not here.
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

/// Variables that choose the provider, where it runs, and which models its aliases name. An
/// endpoint URL can carry a credential, and skipping a provider's authentication works only with
/// the gateway's URL, so both stay in Claude Code's settings.
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

/// A Vertex model's region, one variable per model.
const ENV_FAMILY: &str = "VERTEX_REGION_CLAUDE_";

/// The variables a launcher sets for the process it `exec`s (`docs/cli.md`): that process, and the
/// command that starts the same launch again.
const OWNER: &str = "REXUME_LAUNCH_PID";
const COMMAND: &str = "REXUME_LAUNCH";

/// The launch settings of the process `pid`, started as `launch`, as a patch over the recorded
/// settings. It names every key, `null` when empty, so that the report replaces or removes each.
/// The process's own environment is read rather than the hook's, which also carries the `env` of
/// Claude Code's settings: Claude Code applies those itself on resume, and saving them would keep
/// one the user later removed. A launcher that declared its command for the process sets the
/// provider again when it runs, so the provider variables are saved only without one.
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

/// The command a launcher declared for process `pid`, or why it cannot be used; `None` when the
/// declaration is not this process's. Children inherit their parent's, including a program Claude
/// Code starts by its own path, so it counts only for the process `REXUME_LAUNCH_PID` names.
fn declaration(launch: &proc::Launch, pid: u32) -> Option<Result<Vec<String>, String>> {
    if launch.var(OWNER)?.parse::<u32>().ok()? != pid {
        return None;
    }
    // The gate claims the restore before its shell runs the command, so a command that cannot run
    // there stops here: an empty one would reopen as `claude`, a NUL cannot be quoted for the
    // shell, an empty first element names no program, and one with `=` is a variable assignment.
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
