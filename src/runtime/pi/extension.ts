// Written by `rexume install` and kept current by the rexume daemon; `rexume uninstall` removes it.
import { execFile } from "node:child_process";
import { appendFileSync, type FSWatcher, mkdirSync, readdirSync, readFileSync, renameSync, statSync, watch } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const state = join(homedir(), ".local/state/rexume");

// rexume (`pi::inbox`) writes each message to a conversation's inbox as `<id>.json`, and reads
// `<id>.accepted` as the receipt that Pi holds it.
const inboxRoot = join(state, "inbox");

// `pi::killed_at` reads the signals' ends here. An entry in Pi's session file would become the
// branch Pi opens the session on next, even from a process that only had it open, and would move the
// file's modification time, by which `pi --continue` picks a session. Pi takes the ID of a session
// file it opens from the file, so it may hold a `/`.
const exitsOf = (id: string) => join(state, "pi", `${id.replaceAll("/", "_")}.jsonl`);

function recordLength(file: string | undefined): number | undefined {
  if (file === undefined) return 0;
  try {
    return statSync(file, { throwIfNoEntry: false })?.size ?? 0;
  } catch {
    return undefined;
  }
}

// Pi ends on SIGTERM and SIGHUP as on `/quit` and tells extensions of a quit either way, so the
// extension notes the signal and records it: a shutdown or logout ends Pi before the Rex server stops.
const signals = ["SIGTERM", "SIGHUP"] as const;

export default function (pi: ExtensionAPI) {
  let watcher: FSWatcher | undefined;
  let signalled: NodeJS.Signals | undefined;
  // Pi's own handlers, registered before any session starts, end Pi; this one only notes the signal.
  const note = (signal: NodeJS.Signals) => {
    signalled = signal;
  };

  pi.on("session_start", (_event, ctx) => {
    const id = ctx.sessionManager.getSessionId();
    // `rexume report` records the session only when the program above it is the pane's foreground
    // program, so it runs as Pi's child.
    const report = execFile("rexume", ["report", "pi"], { timeout: 5000 }, (error) => {
      if (error && ctx.hasUI) ctx.ui.notify(`rexume could not record this session: ${error.message}`, "warning");
    });
    // An unhandled stream error, as when the report ends without reading its input, would end Pi;
    // the exit status above is the outcome.
    report.stdin?.on("error", () => {});
    const file = ctx.sessionManager.getSessionFile();
    report.stdin?.end(
      JSON.stringify({
        session_id: id,
        session_file: file,
        // Measured before Pi sends a prompt, since the report runs while the session goes on;
        // `undefined` leaves the measurement, and its error, to the report.
        session_file_size: recordLength(file),
        mode: ctx.mode,
        provider: ctx.model?.provider,
        model: ctx.model?.id,
        thinking: ctx.thinkingLevel,
      }),
    );

    watcher?.close();
    watcher = undefined;
    for (const signal of signals) process.off(signal, note);
    // Outside a Rex pane nothing reports the session, so no restore would read its end.
    if (ctx.mode === "tui" && process.env.REX_BLOCK) for (const signal of signals) process.on(signal, note);
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
      mkdirSync(inbox, { recursive: true, mode: 0o700 });
      // An `error` event nothing handles would end the process.
      watcher = watch(inbox, drain).on("error", (error) => {
        if (ctx.hasUI) ctx.ui.notify(`rexume stopped watching its inbox for messages: ${error}`, "warning");
      });
    } catch (error) {
      if (ctx.hasUI) ctx.ui.notify(`rexume cannot watch its inbox for messages: ${error}`, "warning");
      return;
    }
    drain();
  });

  pi.on("session_shutdown", (event, ctx) => {
    watcher?.close();
    watcher = undefined;
    for (const signal of signals) process.off(signal, note);
    // One write per line to a file opened for appending, so rexume reads each line whole or not yet.
    // Another Pi may have the same session open, so the line names this process.
    if (event.reason === "quit" && signalled) {
      const line = { pid: process.pid, signal: signalled, at: Date.now() };
      appendFileSync(exitsOf(ctx.sessionManager.getSessionId()), `${JSON.stringify(line)}\n`);
    }
  });
}
