import { expect, test } from "bun:test";
import plugin from "../plugin/browser-routing";
async function setup(options = {}, permissions: object[] = [], existing?: any, catalog?: Promise<any>) {
  const hooks: Record<string, Function> = {};
  let registration = existing;
  const skills: any[] = [];
  let transform: Function;
  const editor = { get: () => existing, set: (_name: string, value: unknown) => { registration = value; } };
  const cleanup = await plugin.setup({ options,
    agent: { list: async () => catalog ? await catalog : ({data:[{permissions}]}) },
    mcp: { transform: async (fn: Function) => { transform = fn; fn(editor); }, reload: async () => transform(editor), list: async () => ({data:[{name:"browser",status:{status:"connected"}}]}) },
    skill: { transform: async (fn: Function) => fn({add: (skill: unknown) => skills.push(skill)}) },
    tool: { hook: async (name: string, fn: Function) => { hooks[name] = fn; } },
    session: { hook: async (name: string, fn: Function) => { hooks[name] = fn; } },
    event: { subscribe: async function* () {} },
  } as any);
  if (!catalog) await hooks.prompt?.();
  return { hooks, registration, skills, cleanup };
}
test("registers native MCP, skill and session routing", async () => {
  const h = await setup();
  try {
    expect(h.registration.type).toBe("local");
    expect(h.registration.codemode).toBe(false);
    expect(h.registration.timeout.execution).toBe(600000);
    expect(h.skills.length).toBeGreaterThan(0);
    const call = {tool:"browser_dispatch",sessionID:"ses_one",input:{name:"click",args:{uid:"1"}}};
    await h.hooks["execute.before"](call);
    expect(call.input).toEqual({name:"click",args:{uid:"1"},__opencode_session_id:"ses_one"});
  } finally { await h.cleanup?.(); }
});
test("preserves individual tool permissions with direct tools", async () => {
  const h = await setup({}, [{action:"browser_click",resource:"*",effect:"deny"}]);
  try { expect(h.registration.command.at(-1)).toBe("--direct-tools"); }
  finally { await h.cleanup?.(); }
});
test("disabled adapters do not register skills", async () => {
  const h = await setup({enabled:false});
  try { expect(h.registration.disabled).toBe(true); expect(h.skills).toEqual([]); }
  finally { await h.cleanup?.(); }
});
test("invalid options and conflicting registration fail explicitly", async () => {
  await expect(setup({compactTools:"yes"})).rejects.toThrow("boolean");
  await expect(setup({}, [], {type:"remote",url:"https://example.test"})).rejects.toThrow("incompatible");
});

test("setup does not block on the agent catalog it helps initialize", async () => {
  const {promise, resolve} = Promise.withResolvers<any>();
  const h = await setup({}, [], undefined, promise);
  try { resolve({data:[{permissions:[]}]}); await h.hooks.prompt(); expect(h.registration.disabled).toBe(false); }
  finally { resolve({data:[]}); await h.cleanup?.(); }
});
