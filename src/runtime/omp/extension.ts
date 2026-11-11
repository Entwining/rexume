// Written by `rexume install` and kept current by the rexume daemon; `rexume uninstall` removes it.
import { execFile } from "node:child_process";
import { appendFileSync, type FSWatcher, mkdirSync, readdirSync, readFileSync, renameSync, statSync, watch } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI, ExtensionContext, MessageEndEvent } from "@oh-my-pi/pi-coding-agent";

type Message = MessageEndEvent["message"];

const state = join(homedir(), ".local/state/rexume");

// rexume follows this record by byte offset, so it is only ever appended to. omp's session IDs are
// UUIDs, which name a file as they are.
const recordOf = (id: string) => join(state, "omp", `${id}.jsonl`);

// rexume (`omp::inbox`) writes each message to a conversation's inbox as `<id>.json`, and reads
// `<id>.accepted` as the receipt that omp holds it.
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
  if (!process.env.REX_BLOCK) return;
  let watcher: FSWatcher | undefined;

  // omp also binds this extension to each subagent session it runs in its process.
  const own = (ctx: ExtensionContext) => ctx.agent?.kind === "main";

  // One write per line to a file opened for appending, so rexume reads each line whole or not yet.
  const append = (ctx: ExtensionContext, line: object) => {
    try {
      appendFileSync(recordOf(ctx.sessionManager.getSessionId()), `${JSON.stringify(line)}\n`);
    } catch (error) {
      if (ctx.hasUI) ctx.ui.notify(`rexume could not record this turn: ${error}`, "warning");
    }
  };

  let reported: { id: string; file: string | undefined } | undefined;

  // `rexume report` records the session only when the program above it is the pane's foreground
  // program, so it runs as omp's child.
  const report = (ctx: ExtensionContext) => {
    const id = ctx.sessionManager.getSessionId();
    reported = { id, file: ctx.sessionManager.getSessionFile() };
    const child = execFile("rexume", ["report", "omp"], { timeout: 5000 }, (error) => {
      if (error && ctx.hasUI) ctx.ui.notify(`rexume could not record this session: ${error.message}`, "warning");
    });
    // An unhandled stream error, as when the report ends without reading its input, would end omp;
    // the exit status above is the outcome.
    child.stdin?.on("error", () => {});
    child.stdin?.end(
      JSON.stringify({
        session_id: id,
        session_file: reported.file,
        // Measured before the extension's next line, since the report runs while the session goes
        // on; `undefined` leaves the measurement, and its error, to the report.
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
    try {
      mkdirSync(join(state, "omp"), { recursive: true, mode: 0o700 });
    } catch {}
    report(ctx);
    append(ctx, { event: "session" });
    watchInbox(ctx);
  };

  // omp moves a session to another file on `/move`, and to another ID and file when another
  // process holds its file or the file changed under it, and tells extensions neither.
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

  omp.on("agent_start", (_event, ctx) => {
    if (!own(ctx)) return;
    follow(ctx);
    append(ctx, { event: "start" });
  });

  omp.on("message_end", ({ message }, ctx) => {
    if (!own(ctx)) return;
    follow(ctx);
    // omp marks the user messages it adds itself as `synthetic`.
    if (message.role === "user" && !message.synthetic) {
      const text = typeof message.content === "string" ? message.content : message.content[0]?.type === "text" ? message.content[0].text : "";
      append(ctx, { event: "prompt", token: leadingToken(text) });
    }
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
