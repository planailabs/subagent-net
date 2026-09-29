// The hub's API from the browser: RPC calls and the notice stream. The
// session cookie set by /v1/login authenticates both.

export class ApiError extends Error {
  constructor(message, status, kind) {
    super(message);
    this.status = status;
    this.kind = kind;
  }
}

export async function call(op, args = {}) {
  const r = await fetch(`/v1/ops/${op}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(args),
    credentials: "same-origin",
  });
  const body = await r.json().catch(() => null);
  if (!r.ok) throw new ApiError(body?.message || r.statusText, r.status, body?.kind);
  return body;
}

export async function login(token) {
  const r = await fetch("/v1/login", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token }),
    credentials: "same-origin",
  });
  const body = await r.json().catch(() => null);
  if (!r.ok) throw new ApiError(body?.message || "login failed", r.status, body?.kind);
  return body;
}

export async function logout() {
  await fetch("/v1/logout", { method: "POST", credentials: "same-origin" });
}

/** Subscribes to notices; returns a function that stops. Reconnects by itself. */
export function subscribe(query, onNotice) {
  const es = new EventSource(`/v1/events?${new URLSearchParams(query)}`);
  es.onmessage = (e) => {
    try {
      onNotice(JSON.parse(e.data));
    } catch {
      /* a malformed notice isn't worth stopping the stream for */
    }
  };
  return () => es.close();
}
