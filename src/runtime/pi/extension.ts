// Written by `rexume install` and kept current by the rexume daemon; `rexume uninstall` removes it.
import { execFile } from "node:child_process";
import { type FSWatcher, mkdirSync, readdirSync, readFileSync, renameSync, statSync, watch } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

// rexume writes each notice or request for this conversation as `<id>.json` in the conversation's
// inbox, and reads `<id>.accepted` as the receipt that Pi holds it.
const inboxRoot = join(homedir(), ".local/state/rexume/inbox");

// A file that cannot be measured here leaves the measurement, and its error, to the report.
function recordLength(file: string | undefined): number | undefined {
  if (file === undefined) return 0;
  try {
    return statSync(file, { throwIfNoEntry: false })?.size ?? 0;
  } catch {
    return undefined;
  }
}

export default function (pi: ExtensionAPI) {
  let watcher: FSWatcher | undefined;

  pi.on("session_start", (_event, ctx) => {
    const id = ctx.sessionManager.getSessionId();
    // Pi has no command hooks, so report the way Claude Code and Codex hooks do: as a direct child
    // of the agent with the session on stdin. The report exits 0 unless it could not run.
    const report = execFile("rexume", ["report", "pi"], { timeout: 5000 }, (error) => {
      if (error && ctx.hasUI) ctx.ui.notify(`rexume could not record this session: ${error.message}`, "warning");
    });
    // The exit status above is the outcome. Writing the input also fails when the report ends
    // without reading it, and an unhandled stream error would end Pi.
    report.stdin?.on("error", () => {});
    const file = ctx.sessionManager.getSessionFile();
    report.stdin?.end(
      JSON.stringify({
        session_id: id,
        session_file: file,
        // Measured here, before Pi sends a prompt; the report runs while the session goes on.
        session_file_size: recordLength(file),
        mode: ctx.mode,
        provider: ctx.model?.provider,
        model: ctx.model?.id,
        thinking: ctx.thinkingLevel,
      }),
    );

    watcher?.close();
    watcher = undefined;
    if (ctx.mode !== "tui") return;
    const inbox = join(inboxRoot, id.replaceAll("/", "_"));
    const drain = () => {
      let names: string[];
      try {
        names = readdirSync(inbox);
      } catch {
        return;
      }
      for (const name of names.filter((n) => n.endsWith(".json"))) {
        const pending = join(inbox, name);
        const accepted = pending.replace(/\.json$/, ".accepted");
        try {
          const { text } = JSON.parse(readFileSync(pending, "utf8"));
          // Claiming the file before sending keeps a message from reaching Pi twice; a message Pi
          // refused goes back for the next change.
          renameSync(pending, accepted);
          try {
            pi.sendMessage({ customType: "rexume", content: text, display: true }, { triggerTurn: true, deliverAs: "followUp" });
          } catch (error) {
            renameSync(accepted, pending);
            throw error;
          }
        } catch (error) {
          if (ctx.hasUI) ctx.ui.notify(`rexume could not deliver message ${name}: ${error}`, "warning");
        }
      }
    };
    try {
      mkdirSync(inbox, { recursive: true });
      watcher = watch(inbox, drain);
    } catch (error) {
      if (ctx.hasUI) ctx.ui.notify(`rexume cannot watch its inbox for messages: ${error}`, "warning");
      return;
    }
    drain();
  });

  pi.on("session_shutdown", () => {
    watcher?.close();
    watcher = undefined;
  });
}
