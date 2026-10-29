// resource-ledger OpenCode plugin: gives each session its ledger key and wakes idle
// sessions when one of their reservations changes state (granted, out of time,
// expired). Installed by `resource-ledger install opencode`, which fills in RESOURCE_LEDGER.
import { execFile } from "node:child_process";

const RESOURCE_LEDGER = "__RESOURCE_LEDGER_EXE__";
const POLL_MS = 15000;
const RESOURCE_LEDGER_CMD = /(^|[\s;&|(`])resource-ledger(\.exe)?(\s|$)/;

const INTRO = (key) =>
  "[resource-ledger] This machine has a shared resource ledger that every agent session (Claude Code, " +
  "Codex, OpenCode) uses so GPU/RAM/CPU-heavy work does not collide. Before starting heavy work " +
  "(training or fine-tuning, GPU inference or model servers, eval sweeps, big builds or test " +
  "suites, anything holding several GB of RAM or several cores for more than a few minutes), " +
  "reserve it with `resource-ledger request --vram GB --ram GB --cpus N --minutes M --purpose \"what it is\"`. " +
  "If GRANTED: do the work, then `resource-ledger release ID` as soon as it is done, even if time is left. " +
  "One ticket is one task: any further work needs a new request, which joins the queue; " +
  "`resource-ledger extend ID MINUTES` is only for the same task running long. " +
  "If QUEUED: do not start that work; end your turn and you will be woken when it is your turn. " +
  "`resource-ledger help` explains everything. " +
  `Your session key is ${key} (applied automatically).`;

function poll(key) {
  return new Promise((resolve) =>
    execFile(RESOURCE_LEDGER, ["hook", "opencode", "poll", "--session", key], { windowsHide: true, timeout: 30000 },
      (err, stdout) => {
        try { resolve(err ? null : JSON.parse(stdout)); } catch { resolve(null); }
      }));
}

export const ResourceLedgerPlugin = async ({ client }) => {
  const key = (id) => `opencode:${id}`;
  const tracked = new Set(); // sessions that have run resource-ledger
  const idle = new Map(); // sessionID -> boolean
  const pending = new Map(); // sessionID -> [notice text]
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
      if (input.sessionID) output.system.push(INTRO(key(input.sessionID)));
    },
  };
};
