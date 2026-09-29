<script setup>
import { inject, onMounted, onUnmounted, ref } from "vue";
import { call } from "../lib/api.js";

const live = inject("live");
const routes = ref([]);
const deliveries = ref([]);
const error = ref("");

async function refresh() {
  try {
    routes.value = await call("list_routes");
    deliveries.value = await call("list_deliveries", { limit: 50 });
  } catch (e) {
    error.value = e.message;
  }
}

const outcome = (o) => ("error" in o ? `✕ ${o.error}` : `✓ ${typeof o.ok === "string" ? o.ok : JSON.stringify(o.ok)}`);
let poll = null;
onMounted(() => {
  refresh();
  poll = setInterval(refresh, 2000);
});
onUnmounted(() => clearInterval(poll));
</script>

<template>
  <h2>routes</h2>
  <div v-if="error" class="err">{{ error }}</div>
  <table>
    <tr><th>route</th><th>from</th><th>seen</th><th>filtered</th><th>deduped</th><th>debounced</th><th>throttled</th><th>delivered</th><th>errors</th><th>active</th><th>queued</th></tr>
    <tr v-for="r in routes" :key="r.name">
      <td class="hi">{{ r.name }}</td>
      <td>{{ r.from }}</td>
      <td>{{ r.counters.seen }}</td>
      <td>{{ r.counters.filtered }}</td>
      <td>{{ r.counters.deduped }}</td>
      <td>{{ r.counters.debounced }}</td>
      <td>{{ r.counters.throttled }}</td>
      <td>{{ r.counters.delivered }}</td>
      <td :title="r.last_error || ''">{{ r.counters.errors }}</td>
      <td>{{ r.active }}</td>
      <td>{{ r.queued }}</td>
    </tr>
  </table>
  <h2>deliveries</h2>
  <div class="feed">
    <table>
      <tr v-for="d in deliveries" :key="d.id">
        <td class="dim">{{ new Date(d.at).toLocaleTimeString() }}</td>
        <td class="hi">{{ d.route }}</td>
        <td><pre>{{ JSON.stringify(d.payload.batch.length > 1 ? d.payload.batch : d.payload.event) }}</pre></td>
        <td><pre v-for="(o, i) in d.outcomes" :key="i">{{ o.action.kind }} {{ outcome(o) }}</pre></td>
      </tr>
    </table>
  </div>
  <p class="dim">{{ live.deliveries.length }} deliveries since this page opened.</p>
</template>
