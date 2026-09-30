<script setup>
import { ref } from "vue";

const emit = defineEmits(["in"]);
const name = ref("");
const password = ref("");
const error = ref("");
const busy = ref(false);

async function submit() {
  busy.value = true;
  error.value = "";
  const r = await fetch("/api/login", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ name: name.value, password: password.value }) });
  busy.value = false;
  if (r.ok) emit("in", (await r.json()).name);
  else error.value = "wrong name or password";
}
</script>

<template>
  <form class="login" @submit.prevent="submit">
    <h1>Vesper</h1>
    <p class="dim">knock, and come in</p>
    <input v-model="name" placeholder="name" autocomplete="username" autofocus required />
    <input v-model="password" type="password" placeholder="password" autocomplete="current-password" required />
    <button :disabled="busy">enter</button>
    <p v-if="error" class="err">{{ error }}</p>
  </form>
</template>
