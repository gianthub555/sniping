// Log in, then go to the dashboard.
const form = document.getElementById("login");
const msg = document.getElementById("msg");
form.addEventListener("submit", async (e) => {
  e.preventDefault();
  msg.textContent = "";
  const body = { username: form.username.value.trim(), password: form.password.value };
  try {
    const r = await fetch("/api/login", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
      credentials: "same-origin",
    });
    if (r.ok) {
      location.href = "/";
      return;
    }
    const j = await r.json().catch(() => ({}));
    msg.textContent = j.error || `Login failed (${r.status}).`;
  } catch {
    msg.textContent = "The server can't be reached.";
  }
  form.password.value = "";
});
