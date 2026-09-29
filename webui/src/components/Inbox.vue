<script setup>
import { onMounted, onUnmounted, ref } from "vue";
import { call } from "../lib/api.js";

const mail = ref([]);
const peekAddr = ref("mailbox:");
const peeked = ref(null);
const error = ref("");
let running = true;

// Long-poll for mail addressed to the signed-in principal.
async function loop() {
  while (running) {
    try {
      const got = await call("wait_inbox", { timeout_ms: 25000 });
      mail.value = [...got.reverse(), ...mail.value].slice(0, 200);
      error.value = "";
    } catch (e) {
      error.value = e.message;
      await new Promise((r) => setTimeout(r, 3000));
    }
  }
}

async function peek() {
  error.value = "";
  try {
    peeked.value = await call("peek_mail", { addr: peekAddr.value, max: 50 });
  } catch (e) {
    error.value = e.message;
  }
}

onMounted(loop);
onUnmounted(() => (running = false));
</script>

<template>
  <h2>your inbox</h2>
  <div v-if="error" class="err">{{ error }}</div>
  <p v-if="!mail.length" class="dim">Answers to what you spawn or send arrive here.</p>
  <div v-for="(m, i) in mail" :key="i" class="msg">
    <div class="who">{{ m.from }}<span v-if="m.status"> · {{ m.status }}</span></div>
    <pre>{{ m.content }}</pre>
  </div>
  <h2>look into an address</h2>
  <form class="row" @submit.prevent="peek">
    <input v-model="peekAddr" class="grow" placeholder="mailbox:door-events or route:speech" aria-label="address" />
    <button type="submit">peek</button>
  </form>
  <div v-for="(m, i) in peeked || []" :key="i" class="msg">
    <div class="who">{{ m.from }}</div>
    <pre>{{ m.content }}</pre>
  </div>
  <p v-if="peeked && !peeked.length" class="dim">empty</p>
</template>
