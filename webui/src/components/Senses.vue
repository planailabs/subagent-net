<script setup>
import { inject, onMounted, onUnmounted, ref } from "vue";
import { call } from "../lib/api.js";

const live = inject("live");
const senses = ref([]);
const error = ref("");
const inject_ = ref({ sense: "", data: "{}" });

async function refresh() {
  try {
    senses.value = await call("list_senses");
  } catch (e) {
    error.value = e.message;
  }
}

async function injectEvent() {
  error.value = "";
  try {
    await call("inject_event", { sense: inject_.value.sense, data: JSON.parse(inject_.value.data) });
  } catch (e) {
    error.value = e.message;
  }
}

let poll = null;
onMounted(() => {
  refresh();
  poll = setInterval(refresh, 3000);
});
onUnmounted(() => clearInterval(poll));
</script>

<template>
  <h2>senses</h2>
  <div v-if="error" class="err">{{ error }}</div>
  <table>
    <tr><th></th><th>sense</th><th>node</th><th>source</th><th>stages</th><th>problem</th></tr>
    <tr v-for="s in senses" :key="s.name">
      <td class="glyph">{{ s.running ? "●" : s.error ? "✕" : "○" }}</td>
      <td class="hi">{{ s.name }}</td>
      <td>{{ s.node }}</td>
      <td>{{ s.source }}</td>
      <td>{{ s.stages.join(" → ") || "–" }}</td>
      <td class="dim">{{ s.error || "" }}</td>
    </tr>
  </table>
  <h2>inject an event</h2>
  <form class="row" @submit.prevent="injectEvent">
    <select v-model="inject_.sense" aria-label="sense">
      <option v-for="s in senses" :key="s.name" :value="s.name">{{ s.name }}</option>
    </select>
    <input v-model="inject_.data" class="grow" aria-label="event data (JSON)" />
    <button type="submit" :disabled="!inject_.sense">inject</button>
  </form>
  <h2>live events</h2>
  <div class="feed">
    <table>
      <tr v-for="e in live.senses" :key="e.id">
        <td class="dim">{{ new Date(e.at).toLocaleTimeString() }}</td>
        <td class="hi">{{ e.sense }}</td>
        <td class="dim">{{ e.node }}</td>
        <td><pre>{{ JSON.stringify(e.data) }}</pre></td>
      </tr>
    </table>
  </div>
</template>
