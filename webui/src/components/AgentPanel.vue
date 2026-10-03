<script setup>
import { computed, inject, ref, watch } from "vue";
import { call } from "../lib/api.js";
import { glyph, typeName } from "../lib/park.js";
import { sections } from "../lib/transcript.js";

const props = defineProps({ id: { type: String, required: true } });
const emit = defineEmits(["close", "changed"]);
const live = inject("live");
const t = ref(null);
const msg = ref("");
const error = ref("");

async function load() {
  try {
    // Everything as it happened: compacted parts too, folded below.
    t.value = await call("transcript", { id: props.id, full: true });
    error.value = "";
  } catch (e) {
    error.value = e.message;
  }
}

async function act(op, args = {}) {
  error.value = "";
  try {
    const r = await call(op, { id: props.id, ...args });
    emit("changed");
    await load();
    return r;
  } catch (e) {
    error.value = e.message;
  }
}

async function send() {
  if (!msg.value.trim()) return;
  error.value = "";
  try {
    await call("send", { to: `agent:${props.id}`, content: msg.value });
    msg.value = "";
    await load();
  } catch (e) {
    error.value = e.message;
  }
}

async function upgrade() {
  const r = await act("upgrade", { tree: true });
  if (r) error.value = `upgraded: now ${r.id}`;
}

async function fork(tree) {
  const r = await act("fork", { tree });
  if (r) error.value = `forked as ${r.id}`;
}

watch(() => props.id, load, { immediate: true });
// Reload when this agent's summary changed; stream text comes from `live`.
watch(() => live.stale.has(props.id) || live.stale.has("*"), (s) => s && load());
const streaming = computed(() => live.text[props.id] || t.value?.partial?.content || "");
const content = (m) => m.content ?? (m.tool_calls?.length ? m.tool_calls.map((c) => `→ ${c.function.name}(${c.function.arguments})`).join("\n") : "");
</script>

<template>
  <aside class="panel" aria-label="agent">
    <div class="row">
      <span class="glyph">{{ t ? glyph(t) : "" }}</span>
      <span class="hi">{{ t ? typeName(t.type) : "" }}</span>
      <span class="dim">{{ id }}</span>
      <span class="grow"></span>
      <button aria-label="close" @click="emit('close')">×</button>
    </div>
    <div v-if="t" class="dim">
      {{ t.phase }}<span v-if="t.pause"> · pause {{ t.pause }}{{ t.paused ? "" : " (finishing)" }}</span> · node {{ t.node || "–" }} ·
      {{ t.usage.prompt_tokens + t.usage.completion_tokens }} tokens<span v-if="t.usage.cached_prompt_tokens"> ({{ t.usage.cached_prompt_tokens }} cached)</span><span v-if="t.budget.max_tokens"> of {{ t.budget.max_tokens }}</span>
    </div>
    <div v-if="t?.outdated" class="err">
      Runs an older version of its type, so no node resumes it.
      <button @click="upgrade">upgrade</button>
    </div>
    <div class="row" style="margin: 8px 0">
      <button @click="act('pause', { mode: 'safe' })">pause safe</button>
      <button @click="act('pause', { mode: 'quick' })">quick</button>
      <button @click="act('pause', { mode: 'hard' })">hard</button>
      <button @click="act('resume')">resume</button>
      <button title="summarise its conversation now (nothing is lost: the full transcript keeps it)" @click="act('compact')">compact</button>
      <button @click="fork(false)">fork</button>
      <button @click="fork(true)">fork tree</button>
      <button @click="act('cancel')">cancel</button>
    </div>
    <div v-if="t?.error" class="err">
      failed: {{ t.error }}
      <button @click="act('resume')">resume</button>
    </div>
    <div v-if="t?.awaiting_approval" class="err">
      waiting for approval: <span class="hi">{{ t.awaiting_approval.function.name }}</span>
      <pre>{{ t.awaiting_approval.function.arguments }}</pre>
      <div class="row">
        <button @click="act('approve', { call_id: t.awaiting_approval.id, approved: true })">approve</button>
        <button @click="act('approve', { call_id: t.awaiting_approval.id, approved: false })">deny</button>
      </div>
    </div>
    <div v-for="h in t?.hooks ?? []" :key="h.run" class="err">
      waiting for hook <span class="hi">{{ h.name }}</span> <span class="dim">({{ h.run }})</span>
      <div class="row">
        <button title="decide it by hand: as if the hook allowed" @click="act('settle_hook', { run: h.run, decision: 'allow' })">allow</button>
        <button title="decide it by hand: as if the hook denied" @click="act('settle_hook', { run: h.run, decision: 'deny', reason: 'denied by hand' })">deny</button>
      </div>
    </div>
    <div v-if="error" class="err">{{ error }}</div>
    <form class="row" @submit.prevent="send">
      <input v-model="msg" class="grow" placeholder="message" aria-label="message" />
      <button type="submit">send</button>
    </form>
    <div v-if="t">
      <template v-for="(s, k) in sections(t.messages, t.compacted)" :key="k">
        <details v-if="s.compacted" class="compacted">
          <summary>compacted: {{ s.items.length }} messages, which the model now sees as this summary</summary>
          <pre class="summary">{{ s.compacted.summary }}</pre>
          <div v-for="{ m, i } in s.items" :key="i" :class="['msg', m.role]">
            <div class="who">{{ m.role }}<span v-if="m.tool_call_id"> · {{ m.tool_call_id }}</span></div>
            <pre>{{ content(m) }}</pre>
          </div>
        </details>
        <template v-else>
          <div v-for="{ m, i } in s.items" :key="i" :class="['msg', m.role]">
            <div class="who">{{ m.role }}<span v-if="m.tool_call_id"> · {{ m.tool_call_id }}</span></div>
            <pre>{{ content(m) }}</pre>
          </div>
        </template>
      </template>
      <div v-if="streaming" class="msg assistant partial">
        <div class="who">assistant · streaming</div>
        <pre>{{ streaming }}</pre>
      </div>
      <div v-if="t.inbox.length" class="msg">
        <div class="who">queued</div>
        <pre v-for="(q, i) in t.inbox" :key="i">{{ q.from }}: {{ q.content }}</pre>
      </div>
    </div>
  </aside>
</template>
