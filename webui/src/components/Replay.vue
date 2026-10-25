<script setup>
// The flight recorder: one agent's log as a timeline to scrub through, or two
// runs aligned side by side to see where they part.
import { computed, onMounted, ref, watch } from "vue";
import { call } from "../lib/api.js";

const agents = ref([]);
const id = ref(location.hash.startsWith("#replay/") ? location.hash.slice(8) : "");
const other = ref("");
const log = ref(null);
const pos = ref(0);
const state = ref(null);
const cmp = ref(null);
const error = ref("");

const who = (a) => `${a.type.split("@")[0]} · ${a.id.slice(0, 8)}`;
const step = computed(() => log.value?.steps?.[pos.value] ?? null);
// Scrub to the end of the selected step: the state after it happened.
const at = computed(() => (step.value ? step.value.last : null));
const now = computed(() => log.value && log.value.head === at.value);

function when(ms) {
  if (!ms) return "";
  return new Date(ms).toLocaleTimeString([], { hour12: false });
}
const pct = (f) => `${Math.round((f ?? 0) * 100)}%`;
const mark = { same: " ", changed: "~", only_a: "-", only_b: "+" };

async function refreshAgents() {
  try {
    agents.value = await call("list_agents");
    if (!id.value && agents.value.length) id.value = agents.value[0].id;
    error.value = "";
  } catch (e) {
    error.value = e.message;
  }
}

watch(id, async (v) => {
  history.replaceState(null, "", v ? `#replay/${v}` : "#replay");
  log.value = null;
  state.value = null;
  pos.value = 0;
  if (!v) return;
  try {
    log.value = await call("events", { id: v });
    pos.value = Math.max(0, (log.value.steps?.length ?? 0) - 1);
    error.value = "";
  } catch (e) {
    error.value = e.message;
  }
});

watch([id, other], async ([a, b]) => {
  cmp.value = null;
  if (!a || !b || a === b) return;
  try {
    cmp.value = await call("diff", { a, b });
    error.value = "";
  } catch (e) {
    error.value = e.message;
  }
});

watch([id, at], async ([a, n]) => {
  state.value = null;
  if (!a || n == null) return;
  try {
    state.value = await call("transcript", { id: a, at: n });
    error.value = "";
  } catch (e) {
    error.value = e.message;
  }
});

// A row of the comparison: scrub A to it.
function pickRow(r) {
  if (r.a == null || !cmp.value) return;
  pos.value = cmp.value.steps_a.findIndex((s) => s.seq === cmp.value.steps_a[r.a].seq);
}

async function fork() {
  if (!id.value || at.value == null) return;
  error.value = "";
  try {
    const f = await call("fork", { id: id.value, at: at.value });
    other.value = id.value;
    id.value = f.id;
  } catch (e) {
    error.value = e.message;
  }
}

const last = computed(() => {
  const m = state.value?.messages ?? [];
  const a = [...m].reverse().find((x) => x.role === "assistant" && x.content);
  return a?.content ?? "";
});

onMounted(refreshAgents);
</script>

<template>
  <div class="row" style="margin-bottom: 10px">
    <select v-model="id" aria-label="agent">
      <option v-if="!agents.length" value="">no agents</option>
      <option v-for="a in agents" :key="a.id" :value="a.id">{{ who(a) }}</option>
    </select>
    <span class="dim">vs</span>
    <select v-model="other" aria-label="compare with">
      <option value="">nothing (timeline)</option>
      <option v-for="a in agents" :key="a.id" :value="a.id" :disabled="a.id === id">{{ who(a) }}</option>
    </select>
    <span class="grow"></span>
    <template v-if="cmp">
      <span class="dim">same {{ cmp.diff.same }}</span>
      <span class="dim">changed {{ cmp.diff.changed }}</span>
      <span class="dim">only A {{ cmp.diff.only_a }} · only B {{ cmp.diff.only_b }}</span>
      <span class="hi">{{ pct(cmp.diff.similarity) }} identical</span>
    </template>
    <span v-else-if="log" class="dim">{{ log.steps.length }} steps · head {{ log.head }}</span>
  </div>
  <div v-if="error" class="err">{{ error }}</div>

  <p v-if="!agents.length" class="dim">No agents yet. Spawn one: <code>subnet spawn &lt;type&gt; "task"</code>.</p>

  <div v-else-if="!cmp" class="split">
    <section>
      <div v-if="log" class="row" style="margin-bottom: 8px">
        <input v-model.number="pos" type="range" class="grow" min="0" :max="Math.max(0, log.steps.length - 1)" aria-label="scrub" />
        <button :disabled="now" @click="pos = log.steps.length - 1">now</button>
      </div>
      <div v-if="log" class="tl">
        <div v-for="(s, i) in log.steps" :key="s.seq" class="st" :class="{ sel: i === pos }" @click="pos = i">
          <span class="dim">{{ when(s.at_ms) }}</span>
          <span class="dim">{{ s.kind }}</span>
          <span class="lab">{{ s.label }}<span v-if="s.preview" class="pv"> — {{ s.preview }}</span></span>
        </div>
      </div>
    </section>
    <aside class="panel" aria-label="state at this step">
      <div class="row">
        <span class="hi">state</span>
        <span class="dim">at {{ at ?? "–" }} / {{ log?.head ?? "–" }}</span>
        <span class="grow"></span>
        <button :disabled="!step" @click="fork">fork from here</button>
      </div>
      <div v-if="step" class="msg">
        <div class="dim">{{ step.kind }} · seq {{ step.seq }}..{{ step.last }}</div>
        <pre>{{ step.preview || step.label }}</pre>
      </div>
      <div v-if="state" class="dim">
        {{ state.phase }}<span v-if="state.pause"> · pause {{ state.pause }}</span> · {{ state.messages.length }} messages ·
        {{ state.usage.prompt_tokens + state.usage.completion_tokens }} tokens
      </div>
      <div v-if="state" class="msg">
        <div class="who">task</div>
        <pre>{{ state.messages[0]?.content || "–" }}</pre>
      </div>
      <div v-if="last" class="msg assistant">
        <div class="who">last answer</div>
        <pre>{{ last }}</pre>
      </div>
      <p v-if="state?.inbox?.length" class="dim">{{ state.inbox.length }} queued message(s)</p>
    </aside>
  </div>

  <div v-else>
    <div v-if="cmp.spec_changed" class="err">
      built differently: {{ cmp.a_ty }} vs {{ cmp.b_ty }} — read the steps with that in mind.
    </div>
    <div v-if="cmp.diff.first" class="msg">
      <span class="hi">first divergence</span>
      <span class="dim">· {{ cmp.diff.first.cause }} · row {{ cmp.diff.first.row + 1 }}</span>
      <div>{{ cmp.diff.first.note }}</div>
    </div>
    <p v-else class="dim">The two runs are the same, step for step.</p>
    <div class="row dim" style="margin: 8px 0">
      <span><code> </code>same</span><span><code>~</code>changed</span><span><code>-</code>only A</span><span><code>+</code>only B</span>
      <span class="grow"></span>
      <span>{{ pct(cmp.diff.alignment) }} aligned</span>
    </div>
    <div class="cmp">
      <div v-for="(r, i) in cmp.diff.rows" :key="i" class="dr" :class="r.status" :title="`row ${i + 1}`" @click="pickRow(r)">
        <span class="mark">{{ mark[r.status] }}</span>
        <span class="side">
          <template v-if="r.a != null">
            <span class="dim">{{ cmp.steps_a[r.a].kind }}</span> {{ cmp.steps_a[r.a].label }}
            <span v-if="cmp.steps_a[r.a].preview" class="pv">— {{ cmp.steps_a[r.a].preview }}</span>
          </template>
        </span>
        <span class="side">
          <template v-if="r.b != null">
            <span class="dim">{{ cmp.steps_b[r.b].kind }}</span> {{ cmp.steps_b[r.b].label }}
            <span v-if="cmp.steps_b[r.b].preview" class="pv">— {{ cmp.steps_b[r.b].preview }}</span>
          </template>
        </span>
      </div>
    </div>
    <p class="dim">Click a row to scrub A to it.</p>
  </div>
</template>
