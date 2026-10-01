<script setup>
import { computed, inject, onMounted, onUnmounted, ref, watch } from "vue";
import { call } from "../lib/api.js";
import { bar, buildPlots, filterAgents, glyph, shortId, tokenShare, treePrefix, typeName } from "../lib/park.js";
import { lastLine, tileLine } from "../lib/live.js";
import AgentPanel from "./AgentPanel.vue";

const live = inject("live");
const agents = ref([]);
const q = ref("");
// `#park/<id>` opens an agent directly.
const selected = ref(location.hash.startsWith("#park/") ? location.hash.slice(6) : null);
watch(selected, (id) => history.replaceState(null, "", id ? `#park/${id}` : "#park"));
const error = ref("");
const plots = computed(() => buildPlots(filterAgents(agents.value, q.value)));

async function refresh() {
  try {
    agents.value = await call("list_agents");
    error.value = "";
  } catch (e) {
    error.value = e.message;
  }
}

// Summaries go stale with most events; refetch at most every 500 ms.
let timer = null;
watch(
  () => live.stale.size,
  (n) => {
    if (n && !timer) {
      timer = setTimeout(async () => {
        timer = null;
        live.stale.clear();
        await refresh();
      }, 500);
    }
  },
);

let poll = null;
onMounted(() => {
  refresh();
  poll = setInterval(refresh, 10000);
});
onUnmounted(() => {
  clearInterval(poll);
  clearTimeout(timer);
});
</script>

<template>
  <div class="split">
    <section>
      <div class="row" style="margin-bottom: 10px">
        <input v-model="q" class="grow" placeholder="filter: type, id, phase, node, paused" aria-label="filter agents" />
        <span class="dim">{{ agents.length }} agents · {{ plots.length }} plots</span>
      </div>
      <div v-if="error" class="err">{{ error }}</div>
      <p v-if="!agents.length" class="dim">No agents yet. Spawn one: <code>subnet spawn &lt;type&gt; "task"</code>.</p>
      <div class="park">
        <div v-for="p in plots" :key="p.root.id" class="plot">
          <div class="title">
            <span>{{ typeName(p.root.type) }} {{ shortId(p.root.id) }}</span>
            <span>{{ p.rows.length }}</span>
          </div>
          <div
            v-for="r in p.rows"
            :key="r.agent.id"
            :class="['tile', r.agent.phase, { sel: selected === r.agent.id }]"
            role="button"
            tabindex="0"
            @click="selected = r.agent.id"
            @keydown.enter="selected = r.agent.id"
          >
            <span><span class="dim">{{ treePrefix(r) }}</span><span class="glyph" :title="r.agent.phase">{{ glyph(r.agent) }}</span></span>
            <span>{{ typeName(r.agent.type) }} <span class="dim">{{ shortId(r.agent.id) }}</span><span v-if="r.agent.outdated" class="err" title="an older version of its type: upgrade it to run"> ⇡</span></span>
            <span class="dim" :title="`${r.agent.usage.prompt_tokens + r.agent.usage.completion_tokens} tokens`">{{ bar(tokenShare(r.agent)) }}</span>
            <span class="last">{{ tileLine(live, r.agent.id) || (r.agent.paused ? 'paused: ' + r.agent.pause : lastLine(r.agent.last)) }}</span>
          </div>
        </div>
      </div>
      <p class="dim" style="margin-top: 12px">● thinking · ▣ tools · ○ idle · ‖ paused · ? needs approval · ✕ failed · · cancelled · ⇡ outdated (upgrade)</p>
    </section>
    <AgentPanel v-if="selected" :id="selected" @close="selected = null" @changed="refresh" />
  </div>
</template>
