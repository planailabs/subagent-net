<script setup>
import { onMounted, onUnmounted, provide, reactive, ref } from "vue";
import { call, logout, subscribe } from "./lib/api.js";
import { applyNotice, emptyLive } from "./lib/live.js";
import Login from "./components/Login.vue";
import Park from "./components/Park.vue";
import Senses from "./components/Senses.vue";
import Switchboard from "./components/Switchboard.vue";
import Cluster from "./components/Cluster.vue";
import Inbox from "./components/Inbox.vue";

const tabs = { park: Park, senses: Senses, switchboard: Switchboard, cluster: Cluster, inbox: Inbox };
const initial = location.hash.slice(1).split("/")[0];
const tab = ref(initial in tabs ? initial : "park");
const me = ref(null);
const checked = ref(false);
const live = reactive(emptyLive());
provide("live", live);
provide("me", me);

let stop = null;
function start() {
  stop?.();
  stop = subscribe({}, (n) => applyNotice(live, n));
}

async function whoami() {
  try {
    me.value = await call("whoami");
    start();
  } catch {
    me.value = null;
  }
  checked.value = true;
}

function go(t) {
  tab.value = t;
  location.hash = t;
}

async function signOut() {
  await logout();
  stop?.();
  me.value = null;
}

onMounted(whoami);
onUnmounted(() => stop?.());
</script>

<template>
  <template v-if="checked">
    <Login v-if="!me" @done="whoami" />
    <template v-else>
      <header>
        <span class="brand">subagent-net</span>
        <button v-for="(_, t) in tabs" :key="t" :class="{ on: tab === t }" @click="go(t)">{{ t }}</button>
        <span class="grow"></span>
        <span class="dim">{{ me.addr }} · {{ me.role }}</span>
        <button @click="signOut">sign out</button>
      </header>
      <main><component :is="tabs[tab]" /></main>
    </template>
  </template>
</template>
