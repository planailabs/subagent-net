<script setup>
import { inject, onMounted, ref } from "vue";
import { call } from "../lib/api.js";

const me = inject("me");
const nodes = ref([]);
const cluster = ref(null);
const history = ref([]);
const files = ref([]);
const changes = ref(null);
const error = ref("");
const note = ref("");

async function refresh() {
  try {
    nodes.value = await call("list_nodes");
    cluster.value = await call("get_cluster");
    history.value = await call("cluster_history");
    files.value = (cluster.value.files || []).map((f) => ({ ...f }));
    if (!files.value.length) files.value = [{ name: "cluster.hcl", text: "" }];
  } catch (e) {
    error.value = e.message;
  }
}

async function apply(dry) {
  error.value = note.value = "";
  try {
    const r = await call("apply_cluster", { files: files.value, dry_run: dry });
    changes.value = r.changes;
    note.value = dry ? "dry run" : r.version ? `applied as version ${r.version}` : "nothing changed";
    if (!dry) await refresh();
  } catch (e) {
    error.value = e.message;
  }
}

async function rollback(version) {
  error.value = "";
  try {
    await call("rollback_cluster", { version });
    await refresh();
  } catch (e) {
    error.value = e.message;
  }
}

onMounted(refresh);
</script>

<template>
  <h2>nodes</h2>
  <table>
    <tr><th></th><th>node</th><th>running</th><th>agent types</th><th>mcp</th><th>problems</th></tr>
    <tr v-for="n in nodes" :key="n.name">
      <td class="glyph">{{ n.configured ? "●" : "○" }}</td>
      <td class="hi">{{ n.name }}</td>
      <td>{{ n.running }} / {{ n.capacity }}</td>
      <td>{{ n.agents.map((a) => a.split("@")[0]).join(", ") || "–" }}</td>
      <td>{{ n.mcps.map((a) => a.split("@")[0]).join(", ") || "–" }}</td>
      <td class="dim"><div v-for="(e, id) in n.errors" :key="id">{{ id }}: {{ e }}</div></td>
    </tr>
  </table>
  <h2>cluster <span class="dim" v-if="cluster?.version">version {{ cluster.version.version }} by {{ cluster.version.applied_by }}</span></h2>
  <div v-for="(f, i) in files" :key="i">
    <input v-model="f.name" aria-label="file name" />
    <textarea v-model="f.text" spellcheck="false" :readonly="me.role !== 'admin'" aria-label="cluster file"></textarea>
  </div>
  <div class="row" v-if="me.role === 'admin'">
    <button @click="files.push({ name: `extra-${files.length}.hcl`, text: '' })">add file</button>
    <button @click="apply(true)">dry run</button>
    <button @click="apply(false)">apply</button>
    <span class="dim">{{ note }}</span>
  </div>
  <div v-if="error" class="err" role="alert"><pre>{{ error }}</pre></div>
  <table v-if="changes?.length">
    <tr v-for="c in changes" :key="c.kind + c.name"><td>{{ c.action }}</td><td>{{ c.kind }}</td><td class="hi">{{ c.name }}</td></tr>
  </table>
  <h2>history</h2>
  <table>
    <tr v-for="v in history" :key="v.version">
      <td class="hi">{{ v.version }}</td>
      <td>{{ new Date(v.applied_at * 1000).toLocaleString() }}</td>
      <td>{{ v.applied_by }}</td>
      <td><button v-if="me.role === 'admin' && v.version !== cluster?.version?.version" @click="rollback(v.version)">roll back to this</button></td>
    </tr>
  </table>
</template>
