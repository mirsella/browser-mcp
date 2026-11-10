import { expect, test } from "bun:test";
import { resolve } from "node:path";
import { chmodSync } from "node:fs";

for (const mode of ["exit", "signal"] as const) {
  test(`model adapter drains worker output and propagates ${mode}`, async () => {
    const root = resolve(`.scratch/model-adapter-${mode}-${process.pid}`);
    const binary = `${root}/worker`;
    const marker = `${root}/stopped`;
    await Bun.write(binary, `#!/usr/bin/env bun
if (process.env.MODEL_ADAPTER_TEST === "exit") {
  console.log('{"final":true}');
  process.exit(23);
}
process.on("SIGTERM", () => setTimeout(async () => {
  await Bun.write(${JSON.stringify(marker)}, "stopped");
  console.log('{"final":true}');
  process.exit(0);
}, 30));
console.log('{"ready":true}');
for await (const chunk of Bun.stdin.stream()) {}
`);
    chmodSync(binary, 0o700);
    const adapter = Bun.spawn(["bun", "scripts/browser-model-eval.ts", "mcp",
      `--root=${root}`, `--binary=${binary}`, "--label=after", "--run=after-1"], {
      env: { ...process.env, MODEL_ADAPTER_TEST: mode },
      stdin: "pipe", stdout: "pipe", stderr: "pipe",
    });
    try {
      const reader = adapter.stdout.getReader();
      let output = "";
      if (mode === "signal") {
        const ready = await reader.read();
        output += new TextDecoder().decode(ready.value);
        expect(output).toContain('"ready":true');
        adapter.kill("SIGTERM");
      }
      // Keep host stdin open: worker exit alone must complete the adapter.
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        output += new TextDecoder().decode(value);
      }
      expect(await adapter.exited).toBe(mode === "exit" ? 23 : 0);
      expect(output).toContain('"final":true');
      if (mode === "signal") expect(await Bun.file(marker).text()).toBe("stopped");
    } finally {
      adapter.kill("SIGTERM");
      await adapter.exited;
    }
  }, 5_000);
}

test("model fixture rejects wrong orders, fails the first valid attempt, and records the retry", async () => {
  const root = resolve(`.scratch/model-eval-selftest-${process.pid}`);
  const child = Bun.spawn(["bun", "scripts/browser-model-eval.ts", "site", "--port=0",
    `--root=${root}`], { stdout: "pipe", stderr: "pipe" });
  try {
    const reader = child.stdout.getReader();
    const { value } = await reader.read();
    reader.releaseLock();
    const url = new TextDecoder().decode(value).match(/Fixture ready: (http:\/\/[^\s]+)/)?.[1];
    if (!url) throw new Error("fixture did not announce its listening URL");
    const page = await (await fetch(`${url}run/after-2`)).text();
    expect(page).toContain("Indigo Hinge");
    expect(page).toContain("Filter catalog");
    const post = (data: unknown) => fetch(`${url}run/after-2/order`, {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(data),
    });
    const order = { sku: "IH-274", quantity: 4, coupon: "ORBIT10", shipping: "express", name: "Ada Example" };
    expect((await post({ ...order, quantity: 1 })).status).toBe(400);
    expect((await post(order)).status).toBe(503);
    expect((await fetch(`${url}run/after-2/score`)).status).toBe(404);
    const pending = await Bun.file(`${root}/after-2-state.json`).json();
    expect(pending.orders).toHaveLength(0);
    const retry = await post(order);
    expect(retry.status).toBe(200);
    expect(await retry.json()).toMatchObject({ total: 72.24, receipt: "ORDER-AFTER-2-001" });
    const final = await Bun.file(`${root}/after-2-state.json`).json();
    expect(final.orders).toHaveLength(1);
    expect(final.attempts).toHaveLength(3);
    expect(final.validAttempts).toBe(2);
    const answer = { completed: true, sku: "IH-274", unitPrice: 18.4, stock: 14,
      warrantyMonths: 24, total: 72.24, receipt: "ORDER-AFTER-2-001" };
    const trace = [
      { kind: "request", run: "after-2", message: { method: "tools/call", params: { name: "dispatch" } } },
      { kind: "response", run: "after-2", method: "tools/call", wireBytes: 99,
        message: { result: { isError: true, structuredContent: { ok: true },
          content: [{ type: "text", text: "é" }, { type: "image", data: "abcd" }] } } },
    ];
    await Bun.write(`${root}/after-trace.jsonl`, trace.map(row => JSON.stringify(row)).join("\n"));
    const score = async (reported = answer) => {
      await Bun.write(`${root}/answers.json`, JSON.stringify([{ run: "after-2", valid: true, elapsedSeconds: 1, answer: reported, thread: "fixture-thread" }]));
      const scoring = Bun.spawn(["bun", "scripts/browser-model-eval.ts", "score", `--root=${root}`], { stdout: "pipe", stderr: "pipe" });
      const output = await new Response(scoring.stdout).json();
      expect(await scoring.exited).toBe(0);
      return output;
    };
    const correct = await score();
    expect(correct.find((row: { run: string }) => row.run === "after-2")).toMatchObject({
      passed: true, calls: 1, toolErrors: 1, responseBytes: 99, textBytes: 2,
      imageBase64Bytes: 4, structuredBytes: 11, thread: "fixture-thread",
    });
    expect(correct.find((row: { run: string }) => row.run === "before-1").passed).toBe(false);
    const wrongFacts = await score({ ...answer, warrantyMonths: 30 });
    expect(wrongFacts.find((row: { run: string }) => row.run === "after-2")).toMatchObject({ passed: false, correctOrder: true, correctFacts: false });
    expect((await post(order)).status).toBe(200);
    const duplicate = await score();
    expect(duplicate.find((row: { run: string }) => row.run === "after-2")).toMatchObject({ passed: false, correctOrder: false });
  } finally {
    child.kill("SIGTERM");
    await child.exited;
  }
}, 10_000);

test("code trials support named conditions and audit scores without dispatch or session metadata", async () => {
  const root = resolve(`.scratch/model-code-selftest-${process.pid}`);
  const run = "browser-use-1";
  const invoke = async (...args: string[]) => {
    const child = Bun.spawn(["bun", "scripts/browser-model-eval.ts", ...args, `--root=${root}`], { stdout: "pipe", stderr: "pipe" });
    const output = await new Response(child.stdout).text();
    expect(await child.exited).toBe(0);
    return output;
  };
  const prompt = await invoke("prompt", `--run=${run}`);
  expect(prompt).toContain("browser_trial_browser_use_1");
  expect(prompt).toContain("JavaScript, DOM operations");
  expect(prompt).not.toContain("web/fetch tools, evaluate_script");
  const config = JSON.parse(await invoke("config", "--condition=browser-use", "--labels=browser-use", "--seed=1", "--command=/example/argv.json", "--backend=browser-use"));
  expect(config.mcp.servers.browser_trial_browser_use_1.command).toContain("--backend=browser-use");
  const missingSeed = Bun.spawn(["bun", "scripts/browser-model-eval.ts", "config", `--root=${root}`, "--condition=browser-use", "--labels=browser-use", "--command=/example/argv.json"], { stdout: "pipe", stderr: "pipe" });
  expect(await missingSeed.exited).not.toBe(0);
  expect(await new Response(missingSeed.stderr).text()).toContain("require one --seed");
  const order = { sku: "SR-318", quantity: 3, coupon: "ORBIT10", shipping: "express", name: "Ada Example", total: 105.90, receipt: "ORDER-BROWSER-USE-1-001" };
  await Bun.write(`${root}/${run}-state.json`, JSON.stringify({ attempts: [{}, {}], orders: [order] }));
  await Bun.write(`${root}/browser-use-trace.jsonl`, [
    { kind: "request", run, wireBytes: 22, message: { method: "tools/call", params: { name: "browser_exec", arguments: { code: "print('observed')" } } } },
    { kind: "response", run, method: "initialize", wireBytes: 250, message: { result: {} } },
    { kind: "response", run, method: "tools/list", wireBytes: 100, message: { result: {} } },
    { kind: "response", run, method: "tools/call", wireBytes: 80, message: { result: { content: [{ type: "text", text: "observed" }] } } },
    { kind: "response", run: "browser-use-2", method: "tools/call", wireBytes: 999, message: { result: {} } },
  ].map(row => JSON.stringify(row)).join("\n"));
  const answer = { run, answer: { ...order, completed: true, unitPrice: 37, stock: 17, warrantyMonths: 30 } };
  const score = async (valid?: boolean, elapsedSeconds = 1) => {
    await Bun.write(`${root}/answers.json`, JSON.stringify([{ ...answer, valid, elapsedSeconds }]));
    return JSON.parse(await invoke("score", `--runs=${run}`))[0];
  };
  expect(await score()).toMatchObject({ passed: false, correctOrder: true, correctFacts: true, trialValid: false });
  expect(await score(true)).toMatchObject({ passed: true, calls: 1, responseBytes: 80, requestBytes: 22, initializationBytes: 250, catalogBytes: 100 });
  expect(await score(false)).toMatchObject({ passed: false, trialValid: false });
  expect(await score(true, 601)).toMatchObject({ passed: false, withinBudget: false });
  expect(await score(true, -1)).toMatchObject({ passed: false, withinBudget: false });
  const tracePath = `${root}/browser-use-trace.jsonl`;
  const pending = JSON.stringify({ kind: "request", run, message: { method: "tools/call" } });
  await Bun.write(tracePath, (await Bun.file(tracePath).text()) + "\n" + pending);
  expect(await score(true)).toMatchObject({ passed: false, calls: 2, completeTrace: false, withinBudget: true });
  await Bun.write(tracePath, (await Bun.file(tracePath).text()) + "\n" + Array(49).fill(pending).join("\n"));
  expect(await score(true)).toMatchObject({ passed: false, calls: 51, responses: 1, completeTrace: false, withinBudget: false });
});

test("external MCP command preserves code calls without an OpenCode session identity", async () => {
  const root = resolve(`.scratch/model-external-${process.pid}`);
  const worker = `${root}/worker.ts`;
  const command = `${root}/command.json`;
  await Bun.write(worker, `import { createInterface } from "node:readline";
for await (const line of createInterface({ input: process.stdin })) {
 const request = JSON.parse(line);
 console.log(JSON.stringify({ jsonrpc: "2.0", id: request.id, result: { content: [{ type: "text", text: request.params.arguments.code }] } }));
}`);
  await Bun.write(command, JSON.stringify([process.execPath, worker]));
  const request = { jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "browser_exec", arguments: { code: "print('observed')" } } };
  const adapter = Bun.spawn(["bun", "scripts/browser-model-eval.ts", "mcp", `--root=${root}`, "--label=browser-use", "--run=browser-use-1", `--command=${command}`, "--backend=browser-use"], { stdin: "pipe", stdout: "pipe", stderr: "pipe" });
  try {
    adapter.stdin.write(JSON.stringify(request) + "\n");
    adapter.stdin.end();
    const output = JSON.parse(await new Response(adapter.stdout).text());
    expect(await adapter.exited).toBe(0);
    expect(output.result.content[0].text).toBe(request.params.arguments.code);
    const trace = (await Bun.file(`${root}/browser-use-trace.jsonl`).text()).trim().split("\n").map(row => JSON.parse(row));
    expect(trace.find(row => row.kind === "request").message).toEqual(request);
    expect(trace.find(row => row.kind === "response")).toMatchObject({ run: "browser-use-1", method: "tools/call", action: "browser_exec" });
  } finally {
    adapter.kill("SIGTERM");
    await adapter.exited;
  }
}, 5_000);
