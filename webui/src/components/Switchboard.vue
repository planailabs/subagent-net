<script setup>
import { inject, onMounted, onUnmounted, ref } from "vue";
import { call } from "../lib/api.js";

const live = inject("live");
const routes = ref([]);
const deliveries = ref([]);
const holds = ref([]);
const error = ref("");

async function refresh() {
  try {
    routes.value = await call("list_routes");
    deliveries.value = await call("list_deliveries", { limit: 50 });
    holds.value = await call("list_holds");
  } catch (e) {
    error.value = e.message;
  }
}

/** Freezes or releases a hold by hand. */
async function toggle(h) {
  try {
    await call(h.frozen ? "release_hold" : "freeze_hold", { name: h.name });
    await refresh();
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
  <template v-if="holds.length">
    <h2>holds</h2>
    <table>
      <tr><th>hold</th><th>state</th><th>waiting</th><th>dropped</th><th>routes</th><th></th></tr>
      <tr v-for="h in holds" :key="h.name">
        <td class="hi">{{ h.name }}</td>
        <td>{{ h.frozen ? `frozen since ${new Date(h.since).toLocaleTimeString()}` : "open" }}</td>
        <td>{{ h.queued }}</td>
        <td>{{ h.dropped }}</td>
        <td>{{ h.routes.join(", ") }}</td>
        <td><button @click="toggle(h)">{{ h.frozen ? "release" : "freeze" }}</button></td>
      </tr>
    </table>
  </template>
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
