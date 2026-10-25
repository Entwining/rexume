use clap::CommandFactory;

use super::*;

#[test]
fn verify_cli() {
    Args::command().debug_assert();
}

/// Every command has a summary, and every argument a help line without a final period, a
/// kebab-case long flag, and a value name when it takes a value.
#[test]
fn every_argument_is_documented() {
    fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<String>) {
        let name = if path.is_empty() {
            cmd.get_name().to_owned()
        } else {
            format!("{path} {}", cmd.get_name())
        };
        for arg in cmd.get_arguments() {
            let id = arg.get_id();
            if id == "help" || id == "version" {
                continue;
            }
            let help = arg.get_help().map(ToString::to_string).unwrap_or_default();
            if help.is_empty() {
                out.push(format!("{name}: {id} has no help"));
            }
            if help.ends_with('.') {
                out.push(format!("{name}: {id} help ends with a period"));
            }
            if arg.get_action().takes_values() && arg.get_value_names().is_none() {
                out.push(format!("{name}: {id} has no value_name"));
            }
            if let Some(long) = arg.get_long()
                && !long.split('-').all(|word| {
                    !word.is_empty()
                        && word
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                })
            {
                out.push(format!("{name}: --{long} is not kebab-case"));
            }
        }
        if cmd.get_name() != "help" && cmd.get_about().is_none_or(|a| a.to_string().is_empty()) {
            out.push(format!("{name}: no about"));
        }
        for sub in cmd.get_subcommands() {
            walk(sub, &name, out);
        }
    }
    let mut problems = Vec::new();
    walk(&Args::command(), "", &mut problems);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn examples_parse() {
    for text in [EXAMPLES, TASK_EXAMPLES] {
        for line in text
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("rexume "))
        {
            let argv = shlex::split(line).unwrap();
            Args::command()
                .try_get_matches_from(argv)
                .unwrap_or_else(|e| panic!("{line}: {e}"));
        }
    }
}

#[test]
fn help_fits_in_100_columns() {
    fn check(cmd: &mut clap::Command, out: &mut Vec<String>) {
        let text = cmd.render_long_help().to_string();
        for line in text.lines().filter(|l| l.chars().count() > 100) {
            out.push(format!("{}: {line}", cmd.get_name()));
        }
        for sub in cmd.get_subcommands_mut() {
            check(sub, out);
        }
    }
    let mut problems = Vec::new();
    check(&mut Args::command(), &mut problems);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
