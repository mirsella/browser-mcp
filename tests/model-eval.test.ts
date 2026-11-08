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
    const pending = await (await fetch(`${url}run/after-2/score`)).json();
    expect(pending.orders).toHaveLength(0);
    const retry = await post(order);
    expect(retry.status).toBe(200);
    expect(await retry.json()).toMatchObject({ total: 72.24, receipt: "ORDER-AFTER-2-001" });
    const final = await (await fetch(`${url}run/after-2/score`)).json();
    expect(final.orders).toHaveLength(1);
    expect(final.attempts).toHaveLength(3);
    expect(final.validAttempts).toBe(2);
    const answer = { completed: true, sku: "IH-274", unitPrice: 18.4, stock: 14,
      warrantyMonths: 24, total: 72.24, receipt: "ORDER-AFTER-2-001" };
    const trace = [
      { kind: "request", message: { params: { _meta: { "ai.opencode/sessionID": "fixture-session" },
        arguments: { arguments: { url: "http://localhost/run/after-2" } } } } },
      { kind: "response", method: "tools/call", session: "fixture-session", wireBytes: 99,
        message: { result: { isError: true, structuredContent: { ok: true },
          content: [{ type: "text", text: "é" }, { type: "image", data: "abcd" }] } } },
    ];
    await Bun.write(`${root}/after-trace.jsonl`, trace.map(row => JSON.stringify(row)).join("\n"));
    const score = async (reported = answer) => {
      await Bun.write(`${root}/answers.json`, JSON.stringify([{ run: "after-2", answer: reported, thread: "fixture-thread" }]));
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
