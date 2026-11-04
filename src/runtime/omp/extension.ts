// Written by `rexume install` and kept current by the rexume daemon; `rexume uninstall` removes it.
import { execFile } from "node:child_process";
import { appendFileSync, type FSWatcher, mkdirSync, readdirSync, readFileSync, renameSync, statSync, watch } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI, ExtensionContext, MessageEndEvent } from "@oh-my-pi/pi-coding-agent";

type Message = MessageEndEvent["message"];

const state = join(homedir(), ".local/state/rexume");

// omp replaces its session file when it rewrites it, so rexume reads the session's turns from this
// record instead, which only this extension writes and only ever appends to. omp's session IDs are
// UUIDs, which name a file as they are.
const recordOf = (id: string) => join(state, "omp", `${id}.jsonl`);

// rexume writes each notice or request for this conversation as `<id>.json` in the conversation's
// inbox, and reads `<id>.accepted` as the receipt that omp holds it. Pi's inboxes are elsewhere,
// since omp opens a Pi session file under the ID it records (`omp::inbox`).
const inboxOf = (id: string) => join(state, "omp", "inbox", id.replaceAll("/", "_"));

const leadingToken = (text: string) => text.match(/^\[rexume [^\]\s]+\]/)?.[0];

function recordLength(file: string): number | undefined {
  try {
    return statSync(file, { throwIfNoEntry: false })?.size ?? 0;
  } catch {
    return undefined;
  }
}

export default function (omp: ExtensionAPI) {
  // rexume follows only a session that runs in a Rex pane, as its report does.
  if (!process.env.REX_BLOCK) return;
  let watcher: FSWatcher | undefined;

  // omp binds this extension to the pane's session and again to every subagent session it runs in
  // its process; only the pane's session is rexume's.
  const own = (ctx: ExtensionContext) => ctx.agent?.kind === "main";

  // Each line goes in one write to a file opened for appending, so rexume reads it whole or not
  // yet. A line that cannot be written is lost, and rexume reads the record as if omp had not done
  // what it reports.
  const append = (ctx: ExtensionContext, line: object) => {
    try {
      appendFileSync(recordOf(ctx.sessionManager.getSessionId()), `${JSON.stringify(line)}\n`);
    } catch (error) {
      if (ctx.hasUI) ctx.ui.notify(`rexume could not record this turn: ${error}`, "warning");
    }
  };

  // The session `rexume report` last named, which rexume reopens from its file and sends its
  // messages to.
  let reported: { id: string; file: string | undefined } | undefined;

  // Report the way Claude Code and Codex hooks do: as a direct child of the agent with the session
  // on stdin. The report exits 0 unless it could not run.
  const report = (ctx: ExtensionContext) => {
    const id = ctx.sessionManager.getSessionId();
    reported = { id, file: ctx.sessionManager.getSessionFile() };
    const child = execFile("rexume", ["report", "omp"], { timeout: 5000 }, (error) => {
      if (error && ctx.hasUI) ctx.ui.notify(`rexume could not record this session: ${error.message}`, "warning");
    });
    // The exit status above is the outcome. Writing the input also fails when the report ends
    // without reading it, and an unhandled stream error would end omp.
    child.stdin?.on("error", () => {});
    child.stdin?.end(
      JSON.stringify({
        session_id: id,
        session_file: reported.file,
        // Measured here, before the extension's next line; the report runs while the session goes
        // on. A record that cannot be measured here leaves the measurement, and its error, to the
        // report.
        record_length: recordLength(recordOf(id)),
        mode: ctx.mode,
      }),
    );
  };

  const watchInbox = (ctx: ExtensionContext) => {
    watcher?.close();
    watcher = undefined;
    if (ctx.mode !== "tui") return;
    const inbox = inboxOf(ctx.sessionManager.getSessionId());
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
          // Claiming the file before sending keeps a message from reaching omp twice; a message
          // omp refused goes back for the next change.
          renameSync(pending, accepted);
          try {
            omp.sendMessage({ customType: "rexume", content: text, display: true }, { triggerTurn: true, deliverAs: "followUp" });
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
      watcher = watch(inbox, drain);
    } catch (error) {
      if (ctx.hasUI) ctx.ui.notify(`rexume cannot watch its inbox for messages: ${error}`, "warning");
      return;
    }
    drain();
  };

  const started = (_event: unknown, ctx: ExtensionContext) => {
    if (!own(ctx)) return;
    // Only its user may read rexume's state directory, whichever program creates it. A directory
    // that cannot be made fails each write to the record, which warns.
    try {
      mkdirSync(join(state, "omp"), { recursive: true, mode: 0o700 });
    } catch {}
    report(ctx);
    // Where this process took the session up, for a turn an earlier one left open (`omp::Parser`).
    append(ctx, { event: "session" });
    watchInbox(ctx);
  };

  // omp moves a session to another file on `/move`, and to another ID and file when another
  // process holds its file or the file changed under it, and tells extensions neither; the next
  // event the session records reports where it went.
  const follow = (ctx: ExtensionContext) => {
    if (ctx.sessionManager.getSessionId() !== reported?.id) {
      report(ctx);
      watchInbox(ctx);
    } else if (ctx.sessionManager.getSessionFile() !== reported?.file) {
      report(ctx);
    }
  };

  // omp starts a session once per process; `/new`, `/resume`, and forks switch to another, and a
  // branch moves to a new session file.
  omp.on("session_start", started);
  omp.on("session_switch", started);
  omp.on("session_branch", started);

  omp.on("session_shutdown", () => {
    watcher?.close();
    watcher = undefined;
  });

  // The record keeps no prompt text but a leading notice or request token, and of a run's end only
  // its last answer, error, and stop reason; rexume reads each turn's end from it (`omp::Parser`).
  omp.on("agent_start", (_event, ctx) => {
    if (!own(ctx)) return;
    follow(ctx);
    append(ctx, { event: "start" });
  });

  omp.on("message_end", ({ message }, ctx) => {
    if (!own(ctx)) return;
    follow(ctx);
    // Input from the user or the command line; omp marks the input it adds itself as synthetic.
    if (message.role === "user" && !message.synthetic) {
      const text = typeof message.content === "string" ? message.content : message.content[0]?.type === "text" ? message.content[0].text : "";
      append(ctx, { event: "prompt", token: leadingToken(text) });
    }
    // A notice or request is in the conversation once its message ends.
    if (message.role === "custom" && message.customType === "rexume" && typeof message.content === "string") {
      append(ctx, { event: "received", token: leadingToken(message.content) });
    }
  });

  omp.on("agent_end", (event, ctx) => {
    if (!own(ctx)) return;
    follow(ctx);
    const answer = event.messages.findLast((m): m is Extract<Message, { role: "assistant" }> => m.role === "assistant");
    append(ctx, {
      event: "end",
      will_continue: event.willContinue === true,
      awaiting_async_work: event.awaitingAsyncWork === true,
      id: Bun.randomUUIDv7(),
      stop_reason: answer?.stopReason,
      answer: answer?.content.flatMap((part) => (part.type === "text" ? [part.text] : [])).join("\n"),
      error: answer?.errorMessage,
    });
  });
}
