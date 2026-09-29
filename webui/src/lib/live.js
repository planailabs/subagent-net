// Folding the hub's notice stream into what the UI shows. Pure functions.

/** Last line of streamed output per agent, capped. */
export function lastLine(text, max = 80) {
  const lines = (text || "").split("\n").map((l) => l.trim()).filter(Boolean);
  const l = lines.length ? lines[lines.length - 1] : "";
  return l.length > max ? "…" + l.slice(l.length - max + 1) : l;
}

/** Events after which an agent's summary (phase, pause, usage) is stale. */
const CHANGES_SUMMARY = new Set([
  "inbox",
  "llm_done",
  "llm_aborted",
  "llm_failed",
  "tool_result",
  "tool_aborted",
  "approval",
  "child_spawned",
  "child_report",
  "pause_requested",
  "resumed",
  "cancelled",
  "recovered",
]);

/**
 * Applies one notice to `live` = { text: {id: streamed text}, stale: Set(ids),
 * senses: [recent sense events], deliveries: [recent deliveries] } and returns it.
 */
export function applyNotice(live, n, keep = 50) {
  if (n.kind === "agent") {
    const t = n.event.type;
    if (t === "llm_delta" && n.event.delta.content) {
      live.text[n.agent] = ((live.text[n.agent] || "") + n.event.delta.content).slice(-2000);
    }
    if (t === "llm_done") {
      // Keep the finished text visible until the next answer starts.
      live.done[n.agent] = live.text[n.agent] || "";
      delete live.text[n.agent];
    }
    if (CHANGES_SUMMARY.has(t)) live.stale.add(n.agent);
    // A new event for an agent we don't know yet: the list must be refetched.
    if (t === "inbox" || t === "child_spawned") live.stale.add("*");
  } else if (n.kind === "sense") {
    live.senses.unshift(n);
    live.senses.length = Math.min(live.senses.length, keep);
  } else if (n.kind === "delivery") {
    live.deliveries.unshift(n);
    live.deliveries.length = Math.min(live.deliveries.length, keep);
  }
  return live;
}

export function emptyLive() {
  return { text: {}, done: {}, stale: new Set(), senses: [], deliveries: [] };
}

/** What a tile shows as its last line: streaming text, else the last answer. */
export function tileLine(live, id) {
  return lastLine(live.text[id] ?? live.done[id] ?? "");
}
