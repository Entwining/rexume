//! oh-my-pi (omp), a fork of Pi, through the extension `rexume install` writes and the daemon keeps
//! current. Its session record, notices, and inbox are Pi's (`super::pi`).

use super::Conversation;

/// The extension `rexume install` writes into omp's extensions directory. It starts with
/// `pi::WRITTEN_BY` too.
pub const EXTENSION: &str = include_str!("omp/extension.ts");

/// omp reopens a session from its file with the model and thinking level of the session's current
/// branch; a setting given on the command line would override those. `--session` is omp's alias
/// of `--resume`.
pub fn resume_argv(c: &Conversation) -> Vec<String> {
    vec![
        "omp".to_owned(),
        "--session".to_owned(),
        c.transcript.clone().unwrap_or_else(|| c.native_id.clone()),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;

    use anyhow::Result;
    use serde_json::Value;

    use super::super::tests::{node, settings, transcript};
    use super::super::{Receipt, Runtime, Turn, last_turn, pi};
    use super::*;

    #[test]
    fn a_session_reopens_from_its_file_only_once_it_exists() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let session = dir.path().join("session.jsonl");
        let path = session.to_string_lossy().into_owned();
        let mut c = Conversation {
            native_id: "s".into(),
            transcript: Some(path.clone()),
            settings: settings(&[("model", "fast"), ("effort", "low"), ("provider", "gw")]),
        };
        // omp writes the file with its first answer and refuses to resume a missing one.
        assert!(!Runtime::Omp.can_reopen(&c));
        std::fs::write(&session, "")?;
        assert!(Runtime::Omp.can_reopen(&c));
        Runtime::Omp.settings(&mut c)?;
        assert_eq!(Runtime::Omp.resume_argv(&c), ["omp", "--session", &path]);
        assert_eq!(Runtime::Omp.to_string(), "omp");
        assert_eq!("omp".parse::<Runtime>()?, Runtime::Omp);
        Ok(())
    }

    #[test]
    fn turns_and_receipts_follow_the_session_record() -> Result<()> {
        let header = r#"{"type":"session","version":3,"id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/w"}"#;
        let user = r#"{"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":"go"}}"#;
        let stop = r#"{"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"ok"}]}}"#;
        let error = r#"{"type":"message","id":"a2","parentId":"u1","message":{"role":"assistant","stopReason":"error","errorMessage":"overloaded","content":[]}}"#;
        let aborted = r#"{"type":"message","id":"a3","parentId":"u1","message":{"role":"assistant","stopReason":"aborted","content":[]}}"#;
        let notice = r#"{"type":"custom_message","id":"n1","parentId":"a1","customType":"rexume","content":"[rexume d1] done","display":true}"#;
        let other = r#"{"type":"custom_message","id":"n2","parentId":"a1","customType":"other","content":"[rexume d2] quoted","display":true}"#;
        let turn = |lines: &[&str]| last_turn(Runtime::Omp, transcript(lines).path(), 0);
        assert_eq!(turn(&[header])?, Some(Turn::None));
        assert_eq!(turn(&[header, user])?, Some(Turn::Open));
        assert_eq!(
            turn(&[header, user, stop])?,
            Some(Turn::Completed {
                reference: "a1".into(),
                text: "ok".into()
            })
        );
        assert_eq!(
            turn(&[header, user, error])?,
            Some(Turn::Failed {
                text: "overloaded".into()
            })
        );
        assert_eq!(turn(&[header, user, aborted])?, Some(Turn::Cancelled));
        assert_eq!(turn(&[header, user, stop, notice])?, Some(Turn::Open));
        let record = transcript(&[header, user, stop, other, notice]);
        let pending = ["d1".to_owned(), "d2".to_owned()];
        assert_eq!(
            Receipt::new(Runtime::Omp, record.path()).found(&pending)?,
            HashSet::from(["d1".to_owned()])
        );
        Ok(())
    }

    /// Loads the extension in Node and runs its handlers with a fake omp: `session_start` for the
    /// pane's session in `tui` mode, a subagent's start, and a switch to another session. Each
    /// report's input is printed, then whether each handler watched the inbox.
    const HARNESS: &str = r#"
        import { mkdirSync, writeFileSync } from "node:fs";
        import { join } from "node:path";
        const [extension, home] = process.argv.slice(1);
        const handlers = {};
        const sent = [];
        const api = { on: (event, handler) => (handlers[event] = handler), sendMessage: (m) => sent.push(m.content) };
        (await import(extension)).default(api);
        const ctx = (id, kind, mode) => ({
          mode, hasUI: false, agent: { kind },
          sessionManager: { getSessionId: () => id, getSessionFile: () => join(home, `${id}.jsonl`) },
        });
        const inbox = join(home, ".local/state/rexume/inbox");
        for (const id of ["main", "sub", "next"]) {
          mkdirSync(join(inbox, id), { recursive: true });
          writeFileSync(join(inbox, id, "d.json"), JSON.stringify({ text: `[rexume d] to ${id}` }));
        }
        handlers.session_start({ type: "session_start" }, ctx("sub", "sub", "print"));
        handlers.session_start({ type: "session_start" }, ctx("main", "main", "tui"));
        handlers.session_switch({ type: "session_switch", reason: "new" }, ctx("next", "main", "tui"));
        handlers.session_shutdown({ type: "session_shutdown" }, ctx("next", "main", "tui"));
        process.once("beforeExit", () => console.log(JSON.stringify(sent)));
    "#;

    /// The extension reports every session it starts or switches to, and takes notices only for
    /// the pane's own session, in `tui` mode.
    #[test]
    fn the_extension_reports_each_session_and_delivers_only_to_the_panes() -> Result<()> {
        assert!(EXTENSION.starts_with(pi::WRITTEN_BY));
        let node = node()?;
        let dir = tempfile::tempdir()?;
        let extension = dir.path().join("rexume.ts");
        std::fs::write(&extension, EXTENSION)?;
        let (bin, reports) = (dir.path().join("bin"), dir.path().join("reports"));
        std::fs::create_dir(&bin)?;
        std::fs::create_dir(&reports)?;
        let fake = bin.join("rexume");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n[ \"$*\" = 'report omp' ] || exit 2\n/bin/cat > \"$(/usr/bin/mktemp '{}/r.XXXXXX')\"\n",
                reports.display()
            ),
        )?;
        std::fs::set_permissions(&fake, PermissionsExt::from_mode(0o755))?;
        let out = std::process::Command::new(&node)
            .args(["--input-type=module", "-e", HARNESS])
            .arg(&extension)
            .arg(dir.path())
            .env("PATH", &bin)
            .env("HOME", dir.path())
            .output()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::ensure!(out.status.success(), "omp ended: {stderr}");
        let sent: Vec<String> = serde_json::from_slice(&out.stdout)?;
        assert_eq!(sent, ["[rexume d] to main", "[rexume d] to next"]);
        let mut reported = std::fs::read_dir(&reports)?
            .map(|entry| -> Result<(String, String, String)> {
                let r: Value = serde_json::from_slice(&std::fs::read(entry?.path())?)?;
                let at = |k: &str| r[k].as_str().unwrap_or_default().to_owned();
                Ok((at("session_id"), at("mode"), at("agent")))
            })
            .collect::<Result<Vec<_>>>()?;
        reported.sort();
        assert_eq!(
            reported,
            [
                ("main".into(), "tui".into(), "main".into()),
                ("next".into(), "tui".into(), "main".into()),
                ("sub".into(), "print".into(), "sub".into()),
            ]
        );
        let inbox = dir.path().join(".local/state/rexume/inbox");
        assert!(inbox.join("sub/d.json").exists());
        assert!(inbox.join("main/d.accepted").exists());
        Ok(())
    }
}
