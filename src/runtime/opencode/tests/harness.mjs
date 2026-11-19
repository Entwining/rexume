// A TUI with the session service's messages, events, and routes in memory. A page's cursors name
// its newest message, to list the messages after it, and its oldest, to list those before it, as
// OpenCode's do.
import { registerHooks } from "node:module";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
const effects = [];
globalThis.effects = effects;
const solid = "export const createRoot = (f) => f(() => {}); "
  + "export const createEffect = (f) => { globalThis.effects.push(f); f(); };";
registerHooks({
  resolve: (specifier, context, next) => specifier === "solid-js"
    ? { url: `data:text/javascript,${encodeURIComponent(solid)}`, shortCircuit: true }
    : next(specifier, context),
});
const [plugin, home] = process.argv.slice(1);
const { default: definition } = await import(plugin);
const messages = { ses_a: [], ses_b: [] };
const created = {};
const page = (list) => ({
  data: list.toReversed(),
  cursor: list.length ? { previous: `c:${list.at(-1).id}`, next: `n:${list[0].id}` } : {},
});
const prompted = [];
const warnings = [];
let refuse = true;
const handlers = {};
let route = { type: "home" };
const ctx = {
  client: {
    message: {
      list: async ({ sessionID, cursor, limit }) => {
        const all = messages[sessionID];
        if (!cursor) return page(all.slice(-limit));
        const at = all.findIndex((m) => m.id === cursor.slice(2));
        if (at < 0) return page([]);
        return page(cursor.startsWith("c:") ? all.slice(at + 1, at + 1 + limit) : all.slice(Math.max(0, at - limit), at));
      },
    },
    session: {
      get: async ({ sessionID }) => ({ id: sessionID, time: { created: created[sessionID] ?? 0 } }),
      message: {
        get: async ({ sessionID, messageID }) => {
          const found = messages[sessionID].find((m) => m.id === messageID);
          if (!found) throw { _tag: "MessageNotFoundError" };
          return found;
        },
      },
      prompt: async (input) => {
        if (input.text.includes("refused") && refuse) throw new Error("busy");
        prompted.push(input);
        return { data: {} };
      },
    },
  },
  data: {
    on: (type, handler) => {
      (handlers[type] ??= []).push(handler);
      return () => handlers[type].splice(handlers[type].indexOf(handler), 1);
    },
    session: { root: (id) => (id === "ses_child" ? "ses_a" : id) },
  },
  ui: {
    router: { current: () => route },
    toast: { show: ({ message }) => warnings.push(message) },
  },
};
const settle = () => new Promise((resolve) => setTimeout(resolve, 100));
const emit = async (type, sessionID) => {
  for (const handler of handlers[type] ?? []) handler({ type, data: { sessionID } });
  await settle();
};
const navigate = async (to) => {
  route = to;
  for (const effect of effects) effect();
  await settle();
};
const now = () => ({ created: Date.now() });
const user = (id, text) => ({ id, type: "user", text, time: now() });
const answer = (id, text) => ({
  id, type: "assistant", finish: "stop", content: [{ type: "reasoning" }, { type: "text", text }],
  time: { ...now(), completed: Date.now() },
});
const idle = (id) => ({ id, type: "idle", outcome: "succeeded", time: now() });

messages.ses_a.push(user("m1", "go"), answer("m2", "ONE"), idle("m3"));
const inbox = join(home, ".local/state/rexume/opencode/inbox/ses_a");
mkdirSync(inbox, { recursive: true });
writeFileSync(join(inbox, "d.json"), JSON.stringify({ text: "[rexume d] done" }));
writeFileSync(join(inbox, "r.json"), JSON.stringify({ text: "[rexume r] refused" }));

const cleanup = definition.setup(ctx);
await navigate({ type: "session", sessionID: "ses_child" });
await navigate({ type: "session", sessionID: "ses_a" });

// An answer is created when its step started, but comes after a shell command the user ran while
// it waited for the model, and before one the user runs while it is written.
const shell = (id) => ({ id, type: "shell", time: now() });
const started = { created: Date.now() - 1 };
messages.ses_a.push(user("m4", "[rexume n] read this"), shell("m4s"));
const streaming = { id: "m5", type: "assistant", content: [{ type: "text", text: "TW" }], time: started };
messages.ses_a.push(streaming, shell("m5s"));
await emit("session.step.ended", "ses_a");
Object.assign(streaming, answer("m5", "TWO"), { time: { ...started, completed: Date.now() } });
messages.ses_a.push(idle("m6"));
await emit("session.execution.succeeded", "ses_a");

// The next prompt commits a revert, which deletes the newest messages.
messages.ses_a.splice(3);
messages.ses_a.push(user("m7", "again"), answer("m8", "THREE"), idle("m9"));
await emit("session.execution.succeeded", "ses_a");

// The stream drops the events of a change made while it was disconnected.
messages.ses_a.push(idle("m10"));
await emit("server.connected");

// A revert, and more messages than a page holds, the last answer still being written; then as
// many again after an anchor still there.
const filler = (prefix) => Array.from({ length: 200 }, (_, i) => idle(`${prefix}${i}`));
const long = { id: "p2", type: "assistant", content: [{ type: "text", text: "LO" }], time: now() };
messages.ses_a.splice(-1);
messages.ses_a.push(user("p0", "[rexume p] page"), ...filler("p1."), long);
await emit("server.connected");
Object.assign(long, answer("p2", "LONG"));
await emit("session.step.ended", "ses_a");
messages.ses_a.push(...filler("p3."), idle("p4"));
await emit("server.connected");

messages.ses_b.push(idle("b1"));
await emit("session.execution.succeeded", "ses_b");

// A message waits while the TUI shows another screen, and goes once it shows the session again,
// as does one refused before; then the next change to the inbox delivers its message.
await navigate({ type: "home" });
refuse = false;
writeFileSync(join(inbox, "h.json"), JSON.stringify({ text: "[rexume h] home" }));
await settle();
const atHome = prompted.map((p) => p.id);
await navigate({ type: "session", sessionID: "ses_a" });
const shownAgain = prompted.map((p) => p.id);
writeFileSync(join(inbox, "e.json"), JSON.stringify({ text: "[rexume e] next" }));
await settle();

// A session this TUI shows first holds messages from before it started, and a fork the history
// it copied with the times it had.
created.ses_c = Date.now();
messages.ses_c = [{ ...user("c1", "old"), time: { created: 0 } }, { ...idle("c2"), time: { created: created.ses_c - 1 } }];
await navigate({ type: "session", sessionID: "ses_c" });
messages.ses_c.push(user("c3", "new"), idle("c4"));
await emit("session.execution.succeeded", "ses_c");
// The next prompt reverts a turn whose answer was still being written.
messages.ses_c.push(user("c5", "undone"), { id: "c6", type: "assistant", content: [], time: now() });
await emit("session.step.ended", "ses_c");
messages.ses_c.splice(-2);
messages.ses_c.push(user("c7", "again"), idle("c8"));
await emit("session.execution.succeeded", "ses_c");

process.emit("SIGHUP", "SIGHUP");
process.emit("SIGTERM", "SIGTERM");
cleanup?.();
messages.ses_a.push(idle("m11"));
await emit("session.execution.succeeded", "ses_a");
console.log(JSON.stringify({ pid: process.pid, prompted, warnings, atHome, shownAgain }));
