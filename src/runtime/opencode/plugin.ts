// Written by `rexume install` and kept current by the rexume daemon; `rexume uninstall` removes it.
import { execFile } from "node:child_process";
import { appendFileSync, type FSWatcher, mkdirSync, readdirSync, readFileSync, renameSync, statSync, watch } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import type { SessionMessageInfo } from "@opencode/client";
import type { Plugin } from "@opencode/plugin/tui";
import { createEffect, createRoot } from "solid-js";

const state = join(homedir(), ".local/state/rexume/opencode");

// rexume follows this record by byte offset, so it is only ever appended to.
const recordOf = (id: string) => join(state, `${id}.jsonl`);

// rexume (`opencode::inbox`) writes each message to a conversation's inbox as `<id>.json`, and reads
// `<id>.accepted` as the receipt that OpenCode holds it.
const inboxOf = (id: string) => join(state, "inbox", id);

// The most messages OpenCode lists at once.
const PAGE = 200;

// Each writes a message into a session or ends one; the stream drops events while disconnected, so
// every sync reads the session's messages instead.
const settling = [
  "session.inbox.delivered",
  "session.synthetic",
  "session.step.ended",
  "session.step.failed",
  "session.execution.succeeded",
  "session.execution.failed",
  "session.execution.interrupted",
] as const;

// OpenCode ends on SIGTERM, and on SIGHUP, which it raises again as SIGTERM, when the terminal closes:
// a shutdown or logout ends the TUI before the Rex server stops.
const signals = ["SIGTERM", "SIGHUP"] as const;

const leadingToken = (text: string) => text.match(/^\[rexume [^\]\s]+\]/)?.[0];

function recordLength(file: string): number | undefined {
  try {
    return statSync(file, { throwIfNoEntry: false })?.size ?? 0;
  } catch {
    return undefined;
  }
}

// What `opencode::Parser` reads of a message; a type it does not read is left out.
function reduce(message: SessionMessageInfo): object | undefined {
  switch (message.type) {
    case "user":
      return { id: message.id, type: "user", received: leadingToken(message.text) };
    case "synthetic":
      return { id: message.id, type: "synthetic" };
    case "assistant":
      return {
        id: message.id,
        type: "assistant",
        finish: message.finish,
        error: message.error?.message,
        text: message.content.flatMap((part) => (part.type === "text" ? [part.text] : [])).join("\n"),
      };
    case "idle":
      return { id: message.id, type: "idle", outcome: message.outcome, error: message.error?.message };
  }
}

export default {
  id: "rexume",
  setup(ctx: Plugin.Context) {
    // Outside a Rex pane nothing reports the session, so nothing reads what the plugin would record.
    if (!process.env.REX_BLOCK) return;
    const warn = (message: string) => ctx.ui.toast.show({ variant: "warning", message: `rexume ${message}` });
    let disposed = false;

    // One write per line to a file opened for appending, so rexume reads each line whole or not yet.
    const append = (id: string, line: object) => appendFileSync(recordOf(id), `${JSON.stringify(line)}\n`);

    // OpenCode's service runs a session's turns whether or not a TUI shows it, so the record copies
    // the session's messages from the service: on start, after a reconnect, and on each change.
    const mirror = (id: string) => {
      // `anchor` is the newest message copied, with the cursor that lists the messages after it;
      // `since` holds the messages copied after its line, which an interrupted copy wrote.
      let anchor: { id: string; at: number; cursor: string | null } | undefined;
      const since = new Set<string>();
      let text = "";
      try {
        text = readFileSync(recordOf(id), "utf8");
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      }
      for (const raw of text.split("\n")) {
        let line;
        try {
          line = JSON.parse(raw);
        } catch {
          continue;
        }
        if ("cursor" in line) {
          anchor = { id: line.anchor, at: line.at, cursor: line.cursor };
          since.clear();
        } else if (line.id) {
          since.add(line.id);
        }
      }
      // When this TUI started, or the session, if later; set by the session's first copy.
      let first: number | undefined;

      // Copies `messages`, newest first, and anchors the record at the newest one copied.
      const copy = (messages: SessionMessageInfo[], cursor: string | null) => {
        // An answer is copied once it completed or a later message the parser reads follows it, as
        // after a step a crash cut off, which is never completed; a shell command the user runs
        // meanwhile does not end it. Until then the record is anchored at the message before it.
        const [newest] = messages;
        const at = messages.findIndex((message) => reduce(message));
        const answer = messages[at];
        const ready = answer?.type === "assistant" && !answer.time.completed ? messages.slice(at + 1) : messages;
        for (const message of ready.toReversed()) {
          if (since.has(message.id)) continue;
          const line = reduce(message);
          if (line) {
            append(id, line);
            since.add(message.id);
          }
        }
        const [last] = ready;
        if (!last) return;
        anchor = { id: last.id, at: last.time.created, cursor: last === newest ? cursor : null };
        append(id, { cursor: anchor.cursor, anchor: last.id, at: last.time.created });
        since.clear();
      };

      const gone = async (message: string) => {
        try {
          await ctx.client.session.message.get({ sessionID: id, messageID: message });
          return false;
        } catch (error) {
          if ((error as { _tag?: string })._tag !== "MessageNotFoundError") throw error;
          return true;
        }
      };

      const pass = async () => {
        // Whether the anchor is still there: a committed revert deletes messages from its boundary
        // on, the anchor with them, after which its cursor lists nothing.
        let kept = false;
        if (anchor?.cursor) {
          const page = await ctx.client.message.list({ sessionID: id, cursor: anchor.cursor, limit: PAGE });
          if (disposed) return;
          // A full page may not reach the newest message, which the listing below starts from.
          if (page.data.length > 0 && page.data.length < PAGE) return copy(page.data, page.cursor.previous ?? null);
          kept = page.data.length > 0;
          if (!kept && !(await gone(anchor.id))) return;
        } else if (anchor) {
          kept = !(await gone(anchor.id));
        }
        if (disposed) return;
        // The listing below goes back from the newest message to the anchor while it is there, since
        // an answer's creation time is when its step started, before the messages added while it
        // waited for the model, such as a shell command. Otherwise it goes back to the messages
        // created before the deleted anchor, which came before the revert, or, on a session's first
        // copy, before this TUI started: the report measured the record before this copy, as the
        // record of a resumed conversation already holds it, and the history a fork copied from its
        // parent keeps its times.
        const bound = kept
          ? -Infinity
          : (anchor?.at ??
            (first ??= Math.max(performance.timeOrigin, (await ctx.client.session.get({ sessionID: id })).time.created)));
        let page = await ctx.client.message.list({ sessionID: id, order: "desc", limit: PAGE });
        const cursor = page.cursor.previous ?? null;
        const messages: SessionMessageInfo[] = [];
        for (;;) {
          if (disposed) return;
          const end = page.data.findIndex((message) => message.id === anchor?.id || message.time.created < bound);
          messages.push(...(end < 0 ? page.data : page.data.slice(0, end)));
          if (end >= 0 || page.data.length < PAGE || !page.cursor.next) break;
          page = await ctx.client.message.list({ sessionID: id, cursor: page.cursor.next, limit: PAGE });
        }
        copy(messages, cursor);
      };

      let running = false;
      let again = false;
      const sync = () => {
        if (running) {
          again = true;
          return;
        }
        running = true;
        (async () => {
          do {
            again = false;
            await pass();
          } while (again && !disposed);
        })()
          .catch((error) => warn(`could not record session ${id}: ${error}`))
          .finally(() => {
            running = false;
          });
      };

      const inbox = inboxOf(id);
      const sending = new Set<string>();
      // rexume sends to a pane's conversation, which stays the session the TUI showed last while it
      // shows its home screen, so a message waits until a TUI shows its session.
      const drain = () => {
        if (shown !== id) return;
        let names: string[];
        try {
          names = readdirSync(inbox);
        } catch {
          return;
        }
        for (const name of names.filter((n) => n.endsWith(".json") && !sending.has(n))) {
          const pending = join(inbox, name);
          const message = name.replace(/\.json$/, "");
          sending.add(name);
          (async () => {
            const { text } = JSON.parse(readFileSync(pending, "utf8"));
            // OpenCode admits a message ID once, so a message sent again after a failed rename, a
            // crash, or by another TUI is not taken twice. A queued message waits for the turn's end.
            await ctx.client.session.prompt({
              sessionID: id,
              id: `msg_${message}`,
              text,
              delivery: "queue",
            });
            renameSync(pending, join(inbox, `${message}.accepted`));
          })()
            .catch((error) => warn(`could not deliver message ${name}: ${error}`))
            .finally(() => sending.delete(name));
        }
      };
      let watcher: FSWatcher | undefined;
      try {
        mkdirSync(inbox, { recursive: true, mode: 0o700 });
        // An `error` event nothing handles would end the process.
        watcher = watch(inbox, drain).on("error", (error) => {
          warn(`stopped watching its inbox for messages: ${error}`);
        });
      } catch (error) {
        warn(`cannot watch its inbox for messages: ${error}`);
      }
      const resume = () => {
        sync();
        drain();
      };
      resume();
      return { sync, drain, resume, close: () => watcher?.close() };
    };

    const mirrors = new Map<string, ReturnType<typeof mirror>>();
    let reported: string | undefined;
    let shown: string | undefined;

    // `rexume report` records the session only when the program above it is the pane's foreground
    // program, so it runs as the TUI's child. Reports run one at a time, so that the session shown
    // last is the one recorded last.
    let reporting = Promise.resolve();
    const report = (id: string) => {
      // Measured before the plugin's next line, since the report runs while the session goes on;
      // `undefined` leaves the measurement, and its error, to the report.
      const input = JSON.stringify({ session_id: id, record_length: recordLength(recordOf(id)) });
      reporting = reporting.then(() => new Promise<void>((done) => send(input, done)));
    };
    const send = (input: string, done: () => void) => {
      const child = execFile("rexume", ["report", "opencode"], { timeout: 5000 }, (error) => {
        if (error) warn(`could not record this session: ${error.message}`);
        done();
      });
      // An unhandled stream error, as when the report ends without reading its input, would end the
      // TUI; the exit status above is the outcome.
      child.stdin?.on("error", () => {});
      child.stdin?.end(input);
    };

    try {
      mkdirSync(state, { recursive: true, mode: 0o700 });
    } catch {}

    // The pane's conversation is the session the TUI shows; a subagent's session, which opens in the
    // same view, belongs to its parent's. A session shown earlier keeps its record, since OpenCode
    // runs it on.
    const disposeRoot = createRoot((dispose) => {
      createEffect(() => {
        const route = ctx.ui.router.current();
        const id = route.type === "session" ? ctx.data.session.root(route.sessionID) : undefined;
        shown = id;
        if (!id) return;
        if (id !== reported) {
          reported = id;
          report(id);
        }
        const shownBefore = mirrors.get(id);
        if (shownBefore) shownBefore.drain();
        else mirrors.set(id, mirror(id));
      });
      return dispose;
    });

    const offs = settling.map((type) => ctx.data.on(type, (event) => mirrors.get(event.data.sessionID)?.sync()));
    offs.push(ctx.data.on("server.connected", () => mirrors.forEach((m) => m.resume())));

    // Another TUI may have the same session open, so the line names this process.
    let signalled = false;
    const note = (signal: NodeJS.Signals) => {
      if (signalled || !reported) return;
      signalled = true;
      append(reported, { pid: process.pid, signal, at: Date.now() });
    };
    for (const signal of signals) process.on(signal, note);

    return () => {
      disposed = true;
      disposeRoot();
      for (const off of offs) off();
      for (const signal of signals) process.off(signal, note);
      mirrors.forEach((m) => m.close());
    };
  },
};
