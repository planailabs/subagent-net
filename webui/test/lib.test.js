import { test } from "node:test";
import assert from "node:assert/strict";

import { bar, buildPlots, filterAgents, glyph, tokenShare, treePrefix, typeName } from "../src/lib/park.js";
import { applyNotice, emptyLive, lastLine, tileLine } from "../src/lib/live.js";
import { sections } from "../src/lib/transcript.js";

test("a full transcript in sections: what the model sees, and each compaction folded", () => {
  const m = (c) => ({ role: "user", content: c });
  const msgs = ["task", "a", "b", "c", "d", "e"].map(m);
  assert.deepEqual(sections(msgs).map((s) => [s.compacted, s.items.map((x) => x.i)]), [[null, [0, 1, 2, 3, 4, 5]]], "no compactions: one section");
  const s = sections(msgs, [{ from: 1, to: 3, summary: "one" }, { from: 3, to: 5, summary: "two" }]);
  assert.deepEqual(s.map((x) => [x.compacted?.summary ?? null, x.items.map((y) => y.m.content)]), [
    [null, ["task"]],
    ["one", ["a", "b"]],
    ["two", ["c", "d"]],
    [null, ["e"]],
  ]);
  assert.equal(s[3].items[0].i, 5, "indexes are the transcript's");
});

const agent = (id, extra = {}) => ({
  id,
  type: "worker@abc",
  parent: null,
  phase: "idle",
  paused: false,
  usage: { prompt_tokens: 0, completion_tokens: 0 },
  budget: { max_tokens: null },
  reserved: 0,
  ...extra,
});

test("glyphs encode state by shape", () => {
  assert.equal(glyph(agent("a", { phase: "thinking" })), "●");
  assert.equal(glyph(agent("a", { phase: "tools" })), "▣");
  assert.equal(glyph(agent("a")), "○");
  assert.equal(glyph(agent("a", { phase: "thinking", paused: true })), "‖");
  assert.equal(glyph(agent("a", { phase: "failed" })), "✕");
  assert.equal(glyph(agent("a", { phase: "cancelled" })), "·");
  assert.equal(glyph(agent("a", { phase: "tools", awaiting_approval: { id: "c" } })), "?");
});

test("plots group descendants under their root, depth first", () => {
  const agents = [
    agent("r1"),
    agent("k2", { parent: "r1" }),
    agent("k1", { parent: "r1" }),
    agent("g1", { parent: "k1" }),
    agent("r2", { phase: "thinking" }),
    agent("orphan", { parent: "gone" }),
  ];
  const plots = buildPlots(agents);
  assert.deepEqual(
    plots.map((p) => p.root.id),
    ["r2", "orphan", "r1"],
    "live plots first, then by id",
  );
  const r1 = plots.find((p) => p.root.id === "r1");
  assert.deepEqual(
    r1.rows.map((r) => [r.agent.id, r.depth]),
    [["r1", 0], ["k1", 1], ["g1", 2], ["k2", 1]],
  );
  assert.equal(treePrefix(r1.rows[1]), "├ ");
  assert.equal(treePrefix(r1.rows[2]), "  └ ");
  assert.equal(treePrefix(r1.rows[3]), "└ ");
});

test("token bars", () => {
  assert.equal(tokenShare(agent("a")), null);
  const a = agent("a", { budget: { max_tokens: 100 }, usage: { prompt_tokens: 30, completion_tokens: 10 }, reserved: 10 });
  assert.equal(tokenShare(a), 0.5);
  assert.equal(bar(0.5, 6), "███░░░");
  assert.equal(bar(null, 3), "···");
  assert.equal(bar(1, 2), "██");
});

test("filter and names", () => {
  const agents = [agent("abcd"), agent("ef", { type: "lead@1", paused: true })];
  assert.equal(typeName("lead@123"), "lead");
  assert.deepEqual(filterAgents(agents, "lead").map((a) => a.id), ["ef"]);
  assert.deepEqual(filterAgents(agents, "paused").map((a) => a.id), ["ef"]);
  assert.equal(filterAgents(agents, "  ").length, 2);
});

test("notices stream text and mark summaries stale", () => {
  const live = emptyLive();
  const delta = (c) => ({ kind: "agent", agent: "a", event: { type: "llm_delta", delta: { content: c } } });
  applyNotice(live, delta("Hello\nwor"));
  applyNotice(live, delta("ld"));
  assert.equal(tileLine(live, "a"), "world");
  assert.equal(live.stale.size, 0);
  applyNotice(live, { kind: "agent", agent: "a", event: { type: "llm_done" } });
  assert.ok(live.stale.has("a"));
  assert.equal(tileLine(live, "a"), "world", "the finished answer stays visible");
  applyNotice(live, { kind: "agent", agent: "b", event: { type: "inbox", from: "user:x", content: "hi" } });
  assert.ok(live.stale.has("*"), "unknown agents trigger a refetch");
});

test("sense and delivery feeds are bounded", () => {
  const live = emptyLive();
  for (let i = 0; i < 5; i++) applyNotice(live, { kind: "sense", sense: "s", data: { i } }, 3);
  assert.deepEqual(live.senses.map((s) => s.data.i), [4, 3, 2]);
  applyNotice(live, { kind: "delivery", route: "r", payload: {}, outcomes: [] });
  assert.equal(live.deliveries.length, 1);
});

test("last lines are trimmed", () => {
  assert.equal(lastLine("a\n\nb  \n"), "b");
  assert.equal(lastLine("x".repeat(100), 10).length, 10);
  assert.equal(lastLine(""), "");
});
