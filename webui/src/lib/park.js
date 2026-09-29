// The agent park: agents grouped into plots (a root agent and its
// descendants), each plot a tree in spawn order. Pure functions, no DOM.

/** Phase and pause as one glyph. Monochrome UI: state is shape, not colour. */
export function glyph(a) {
  if (a.phase === "cancelled") return "·";
  if (a.phase === "failed") return "✕";
  if (a.paused) return "‖";
  if (a.awaiting_approval) return "?";
  if (a.phase === "thinking") return "●";
  if (a.phase === "tools") return "▣";
  return "○";
}

/** Short label for an agent type id (`name@hash` → `name`). */
export function typeName(ty) {
  return (ty || "").split("@")[0];
}

export function shortId(id) {
  return (id || "").slice(0, 4);
}

/** Share of the token budget used (0–1), or null without a limit. */
export function tokenShare(a) {
  const max = a.budget && a.budget.max_tokens;
  if (!max) return null;
  const used = (a.usage?.prompt_tokens || 0) + (a.usage?.completion_tokens || 0) + (a.reserved || 0);
  return Math.min(1, used / max);
}

/** A text bar of `width` cells for `share` (null → empty track). */
export function bar(share, width = 6) {
  if (share === null || share === undefined) return "·".repeat(width);
  const full = Math.round(share * width);
  return "█".repeat(full) + "░".repeat(width - full);
}

/**
 * Groups agents into plots: [{ root, rows: [{ agent, depth, last }] }].
 * Agents whose parent isn't listed are roots. Plots are ordered with live
 * ones first, then by root id; rows are depth-first.
 */
export function buildPlots(agents) {
  const byId = new Map(agents.map((a) => [a.id, a]));
  const kids = new Map();
  const roots = [];
  for (const a of agents) {
    if (a.parent && byId.has(a.parent)) {
      if (!kids.has(a.parent)) kids.set(a.parent, []);
      kids.get(a.parent).push(a);
    } else {
      roots.push(a);
    }
  }
  for (const list of kids.values()) list.sort((x, y) => x.id.localeCompare(y.id));
  const plots = roots.map((root) => {
    const rows = [];
    const walk = (a, depth, last) => {
      rows.push({ agent: a, depth, last });
      const ks = kids.get(a.id) || [];
      ks.forEach((k, i) => walk(k, depth + 1, i === ks.length - 1));
    };
    walk(root, 0, true);
    return { root, rows };
  });
  const live = (p) => p.rows.some((r) => ["thinking", "tools"].includes(r.agent.phase) && !r.agent.paused);
  plots.sort((a, b) => Number(live(b)) - Number(live(a)) || a.root.id.localeCompare(b.root.id));
  return plots;
}

/** Tree prefix for a row at `depth` (the root has none). */
export function treePrefix(row) {
  if (row.depth === 0) return "";
  return "  ".repeat(row.depth - 1) + (row.last ? "└ " : "├ ");
}

/** Filters agents by free text (type, id prefix, phase, node). */
export function filterAgents(agents, q) {
  const s = (q || "").trim().toLowerCase();
  if (!s) return agents;
  return agents.filter((a) =>
    [typeName(a.type), a.id, a.phase, a.node || "", a.paused ? "paused" : ""].some((f) => f.toLowerCase().includes(s)),
  );
}
