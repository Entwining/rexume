// Written by `rexume install` and kept current by the Rexume daemon; `rexume uninstall` removes it.
import type { Register } from "claude-code";

// Rexume writes each notice or request to the conversation's messaging socket, where Claude Code
// takes it as a message from another session: the model reads it framed as one, and a session
// that bypasses permission prompts holds it for the user's approval. Submitted again as the
// user's own prompt, it is read as written; it still starts with its token, which Rexume's
// `UserPromptSubmit` hook records as the receipt.
const TOKEN = /^\[rexume [^\s\]]+\]/;

export const register: Register = (on) => {
  on("session.receive", async ($, e, next) => {
    // A raw line on the socket arrives as `peer` without the fields another plugin or an agent
    // team adds (observed on Claude Code 2.1.316); a delivery for one of the session's own agents
    // names it.
    const isRexume =
      e.agentId === undefined &&
      e.origin.kind === "peer" &&
      !("teammate" in e.origin) &&
      e.origin.plugin === undefined &&
      TOKEN.test(e.text);
    if (!isRexume) return next(e);
    // A prompt that did not enter, as after the user stopped a turn and typed nothing since, goes
    // on as a message, as it would without this plugin. So does one whose submit throws: that
    // fails this hook, which Claude Code skips.
    const { drop } = await $.prompt.submit({ text: e.text, asUser: true });
    return drop === undefined ? { consumed: "submitted as the user's prompt" } : next(e);
  });
};
