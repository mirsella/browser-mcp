import { Plugin, Skill } from "@opencode/plugin";
import { execFile } from "node:child_process";
import { readdir, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { parse } from "yaml";

const execute = promisify(execFile);
const binary = fileURLToPath(new URL("../vendor/browser-mcp", import.meta.url));
const skills = fileURLToPath(new URL("../skills/", import.meta.url));
const directTools = `click close_page drag emulate evaluate_script fill fill_form get_console_message
get_network_request handle_dialog hover lighthouse_audit list_console_messages list_network_requests
list_pages navigate_page new_page performance_analyze_insight performance_start_trace performance_stop_trace
press_key resize_page take_heapsnapshot take_screenshot take_snapshot type_text upload_file wait_for`
  .split(/\s+/).map(name => `browser_${name}`);

function partialPolicy(agents: Array<{ permissions: readonly { action: string }[] }>) {
  return agents.some(agent => agent.permissions.some(({ action }) => {
    const regex = new RegExp(`^${action.replace(/[.+^${}()|[\]\\]/g, "\\$&").replaceAll("*", ".*").replaceAll("?", ".")}$`);
    return directTools.some(tool => regex.test(tool))
      && !(regex.test("browser_dispatch") && directTools.every(tool => regex.test(tool)));
  }));
}

export default Plugin.define({
  id: "@mirsella/browser-mcp",
  async setup(ctx) {
    const { enabled = true, compactTools = true } = ctx.options;
    if (typeof enabled !== "boolean") throw new Error("enabled must be a boolean");
    if (typeof compactTools !== "boolean") throw new Error("compactTools must be a boolean");
    let compact = compactTools;
    let reconnectGeneration = 0;
    let active = enabled;
    await ctx.mcp.transform(editor => {
      const existing = editor.get("browser");
      if (existing?.type === "remote" || (existing && existing.command[0] !== binary)) {
        throw new Error("Browser MCP cannot replace an incompatible mcp.servers.browser configuration");
      }
      active = !(existing?.disabled ?? !enabled);
      editor.set("browser", {
        ...existing,
        type: "local", command: compact ? [binary] : [binary, "--direct-tools"],
        disabled: !active, codemode: false,
        environment: { ...existing?.environment, BROWSER_MCP_RECONNECT_GENERATION: String(reconnectGeneration) },
        timeout: { startup: 30_000, catalog: 30_000, execution: 600_000, ...existing?.timeout },
      });
    });
    if (active) {
      const entries = await readdir(skills, { withFileTypes: true });
      const registered = await Promise.all(entries.filter(entry => entry.isDirectory()).map(async entry => {
        const path = `${skills}${entry.name}/SKILL.md`;
        const text = await readFile(path, "utf8");
        const frontmatter = text.match(/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/);
        if (!frontmatter) throw new Error(`Missing skill frontmatter: ${path}`);
        const meta = parse(frontmatter[1]);
        if (!meta || typeof meta.name !== "string" || typeof meta.description !== "string") throw new Error(`Invalid skill metadata: ${path}`);
        return Skill.Info.make({ id: Skill.ID.make(meta.name), name: Skill.Name.make(meta.name),
          description: meta.description, path: Skill.Info.fields.path.make(path), content: text.slice(frontmatter[0].length) });
      }));
      await ctx.skill.transform(editor => { for (const skill of registered) editor.add(skill); });
    }
    await ctx.tool.hook("execute.before", input => {
      if (!input.tool.startsWith("browser_")) return;
      if (!input.sessionID) throw new Error("Browser tool call has no OpenCode session ID");
      if (!input.input || typeof input.input !== "object" || Array.isArray(input.input)) throw new Error("Browser tool call has invalid arguments");
      input.input = { ...input.input, __opencode_session_id: input.sessionID };
    });
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    let recovering: Promise<void> | undefined;
    let failures = 0;
    // Runners that never list the server cannot be fixed by reloading;
    // stay quiet until it has been seen at least once.
    let seen = false;
    const schedule = () => {
      if (controller.signal.aborted) return;
      timer = setTimeout(() => void recover(), failures ? Math.min(30_000, 1_000 * 2 ** Math.min(failures - 1, 5)) : 5_000);
      timer.unref();
    };
    const recover = (): Promise<void> => {
      if (controller.signal.aborted) return Promise.resolve();
      if (recovering) return recovering;
      if (timer) clearTimeout(timer);
      recovering = (async () => {
        try {
          const server = (await ctx.mcp.list()).data.find(server => server.name === "browser");
          if (!server) throw new Error("Browser MCP status is missing");
          seen = true;
          // A disabled connection is an explicit user choice; only recover failures.
          if (server.status.status === "failed" && !controller.signal.aborted) {
            console.warn("Browser MCP adapter failed; reloading its registration");
            reconnectGeneration++;
            await ctx.mcp.reload();
            const restored = (await ctx.mcp.list()).data.find(server => server.name === "browser");
            if (restored?.status.status !== "connected") throw new Error(`Browser MCP recovery failed: ${JSON.stringify(restored?.status)}`);
          }
          failures = 0;
        } catch (error) {
          if (!controller.signal.aborted) { failures++; if (seen) console.error("Browser MCP recovery failed; will retry", error); }
        } finally { recovering = undefined; schedule(); }
      })();
      return recovering;
    };
    // Recovery must never stall a prompt: a wedged adapter keeps retrying in
    // the background while chat continues within a short deadline.
    const recoverSoon = async () => {
      let deadline: ReturnType<typeof setTimeout> | undefined;
      try {
        await Promise.race([
          recover(),
          new Promise<void>(resolve => { deadline = setTimeout(resolve, 5_000); deadline.unref(); }),
        ]);
      } finally {
        if (deadline) clearTimeout(deadline);
      }
    };
    // Catalog reads wait for plugin bootstrap. Do not await them inside setup.
    const checkPolicy = async () => {
      if (!compactTools || controller.signal.aborted) return;
      const next = !partialPolicy((await ctx.agent.list()).data);
      if (controller.signal.aborted || next === compact) return;
      compact = next;
      if (!compact) console.warn("Browser MCP uses direct tools to preserve per-action permissions");
      await ctx.mcp.reload();
    };
    const policyReady = Promise.resolve().then(checkPolicy);
    void policyReady.catch(error => console.error("Browser MCP permission initialization failed", error));
    await ctx.session.hook("prompt", async () => { await policyReady; await checkPolicy(); await recoverSoon(); });
    const events = async () => {
      for await (const event of ctx.event.subscribe({ signal: controller.signal })) {
        if (controller.signal.aborted) break;
        if (event.type === "agent.updated") void checkPolicy().catch(error => console.error("Browser MCP permission update failed", error));
        if ((event.type === "mcp.status.changed") && event.data.server === "browser") void recover();
        if (event.type !== "session.deleted") continue;
        try { await execute(binary, ["release-session", event.data.sessionID], { signal: controller.signal }); }
        catch (error) { if (!controller.signal.aborted) console.error(`Failed to release browser session ${event.data.sessionID}`, error); }
      }
    };
    void events().catch(error => { if (!controller.signal.aborted) console.error("Browser MCP event subscription failed", error); });
    schedule();
    return () => { controller.abort(); if (timer) clearTimeout(timer); };
  },
});
