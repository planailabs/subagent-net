<script setup>
import { ref } from "vue";
import { login } from "../lib/api.js";

const emit = defineEmits(["done"]);
const token = ref("");
const error = ref("");

async function submit() {
  error.value = "";
  try {
    await login(token.value.trim());
    emit("done");
  } catch (e) {
    error.value = e.message;
  }
}
</script>

<template>
  <main style="max-width: 420px; margin: 20vh auto">
    <h2>subagent-net</h2>
    <form class="row" @submit.prevent="submit">
      <input v-model="token" class="grow" type="password" placeholder="token" autocomplete="current-password" aria-label="token" />
      <button type="submit">sign in</button>
    </form>
    <p class="dim">A token from <code>subnet issue-token user &lt;you&gt;</code>, or the hub's admin token.</p>
    <div v-if="error" class="err" role="alert">{{ error }}</div>
  </main>
</template>
