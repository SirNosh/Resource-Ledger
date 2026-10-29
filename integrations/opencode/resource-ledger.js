// resource-ledger OpenCode plugin: gives each session the ledger's guide and its key, and
// wakes idle sessions when one of their reservations changes state (granted, out of
// time, expired). Installed by `resource-ledger install opencode`, which fills in RESOURCE_LEDGER.
import { execFile } from "node:child_process";

const RESOURCE_LEDGER = "__RESOURCE_LEDGER_EXE__";
const POLL_MS = 15000;
const RESOURCE_LEDGER_CMD = /(^|[\s;&|(`])resource-ledger(\.exe)?(\s|$)/;

function hook(event, key) {
  return new Promise((resolve) =>
    execFile(RESOURCE_LEDGER, ["hook", "opencode", event, "--session", key], { windowsHide: true, timeout: 30000 },
      (err, stdout) => resolve(err ? null : stdout)));
}

async function poll(key) {
  try { return JSON.parse(await hook("poll", key)); } catch { return null; }
}

export const ResourceLedgerPlugin = async ({ client }) => {
  const key = (id) => `opencode:${id}`;
  const tracked = new Set(); // sessions that have run resource-ledger
  const idle = new Map(); // sessionID -> boolean
  const pending = new Map(); // sessionID -> [notice text]
  const intros = new Map(); // sessionID -> promise of the guide, this machine's pool and the key
  let timer = null;

  async function flush(id) {
    const texts = pending.get(id);
    if (!texts || !idle.get(id)) return;
    pending.delete(id);
    idle.set(id, false);
    await client.session.promptAsync({ path: { id }, body: { parts: [{ type: "text", text: texts.join("\n") }] } });
  }

  async function tickAll() {
    for (const id of [...tracked]) {
      const r = await poll(key(id));
      if (!r) continue;
      if (r.notices.length) pending.set(id, [...(pending.get(id) || []), ...r.notices]);
      if (!r.open && !pending.has(id)) tracked.delete(id);
      await flush(id);
    }
    if (!tracked.size && timer) {
      clearInterval(timer);
      timer = null;
    }
  }

  return {
    event: async ({ event }) => {
      const id = event.properties?.sessionID;
      if (!id) return;
      if (event.type === "session.idle") idle.set(id, true);
      else if (event.type === "session.status") idle.set(id, event.properties.status?.type === "idle");
      else return;
      await flush(id);
    },
    "tool.execute.before": async (input, output) => {
      if (input.tool === "bash" && RESOURCE_LEDGER_CMD.test(output.args?.command || "")) {
        tracked.add(input.sessionID);
        if (!timer) timer = setInterval(() => tickAll().catch(() => {}), POLL_MS);
      }
    },
    "shell.env": async (input, output) => {
      if (input.sessionID) output.env.RESOURCE_LEDGER_SESSION = key(input.sessionID);
    },
    "experimental.chat.system.transform": async (input, output) => {
      const id = input.sessionID;
      if (!id) return;
      if (!intros.has(id)) intros.set(id, hook("intro", key(id)));
      const text = await intros.get(id);
      if (text) output.system.push(text.trim());
      else intros.delete(id); // try again on the next message
    },
  };
};
