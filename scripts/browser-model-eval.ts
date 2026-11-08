// Reproducible dummy site and transparent MCP trace recorder for model evaluations.
// Model calls are made by the host/delegation UI, never by this script.
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

const [mode, ...argv] = process.argv.slice(2);
const options = Object.fromEntries(argv.map(value => {
  const separator = value.indexOf("=");
  if (!value.startsWith("--") || separator < 0) throw new Error("Expected --key=value arguments");
  return [value.slice(2, separator), value.slice(separator + 1)];
}));
const root = resolve(options.root ?? ".scratch/model-eval");
mkdirSync(root, { recursive: true, mode: 0o700 });
const save = (name: string, value: unknown) => writeFileSync(`${root}/${name}`, JSON.stringify(value, null, 2));

function fixture(run: string) {
  if (!/^(before|after)-[12]$/.test(run)) throw new Error("Unknown evaluation run");
  const second = run.endsWith("2");
  return { name: second ? "Indigo Hinge" : "Saffron Relay", sku: second ? "IH-274" : "SR-318",
    cents: second ? 1840 : 3700, stock: second ? 14 : 17, warranty: second ? 24 : 30,
    quantity: second ? 4 : 3, total: second ? 7224 : 10590 };
}

if (mode === "site") {
  const states = new Map<string, { attempts: unknown[]; orders: unknown[]; validAttempts: number }>();
  const server = Bun.serve({ hostname: "127.0.0.1", port: Number(options.port ?? 19340),
    async fetch(request) {
      const url = new URL(request.url);
      if (url.pathname === "/health") return Response.json({ ok: true });
      const match = url.pathname.match(/^\/run\/((?:before|after)-[12])(?:\/(details|order|score))?$/);
      if (!match) return new Response("Not found", { status: 404 });
      const [, run, action] = match;
      const product = fixture(run);
      let current = states.get(run);
      if (!current) {
        current = { attempts: [], orders: [], validAttempts: 0 };
        states.set(run, current);
      }
      if (action === "score") return Response.json({ run, expected: product, ...current });
      if (action === "order") {
        if (request.method !== "POST") return new Response("POST required", { status: 405 });
        const data = await request.json() as Record<string, unknown>;
        const respond = (body: unknown, status = 200) => {
          save(`${run}-state.json`, current);
          return Response.json(body, { status });
        };
        current.attempts.push(data);
        if (data.sku !== product.sku || Number(data.quantity) !== product.quantity ||
            data.coupon !== "ORBIT10" || data.shipping !== "express" || data.name !== "Ada Example") {
          return respond({ error: "Order fields do not match the requested fixture order." }, 400);
        }
        current.validAttempts++;
        if (current.validAttempts === 1) {
          return respond({ error: "Temporary checkout failure. Nothing was ordered. Please retry." }, 503);
        }
        const receipt = `ORDER-${run.toUpperCase()}-${String(current.orders.length + 1).padStart(3, "0")}`;
        const order = { ...data, total: product.total / 100, receipt };
        current.orders.push(order);
        return respond(order);
      }
      const html = action === "details"
        ? `<!doctype html><meta charset="utf-8"><title>${product.name} details</title><h1>${product.name}</h1><p>SKU: ${product.sku}</p><p>Warranty: ${product.warranty} months</p><p>Stock: ${product.stock}</p><p>Unit price: €${(product.cents / 100).toFixed(2)}</p><p>Return to the shop tab to order.</p>`
        : shop(run, product);
      return new Response(html, { headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" } });
    }
  });
  save("site.json", { pid: process.pid, url: server.url.href });
  console.log(`Fixture ready: ${server.url}`);
  for (const signal of ["SIGTERM", "SIGINT"] as const) process.on(signal, () => { server.stop(true); process.exit(0); });
} else if (mode === "mcp") {
  if (!options.binary || !options.run || !/^(before|after)$/.test(options.label ?? "")) throw new Error("mcp requires binary, label, and run");
  fixture(options.run);
  const label = options.label;
  if (!options.run.startsWith(`${label}-`)) throw new Error("MCP run does not match its build label");
  const trace = `${root}/${label}-trace.jsonl`;
  const record = (event: Record<string, unknown>) => appendFileSync(trace, JSON.stringify({ time: Date.now(), ...event }) + "\n");
  const child = Bun.spawn([resolve(options.binary), "--profile", `${root}/${label}-profile`, "--headless",
    "--browser-executable", options.browser ?? "/usr/bin/chromium", "--cdp-port", label === "before" ? "19341" : "19342",
    "--operation-seconds", "45", "--idle-minutes", "5"], { stdin: "pipe", stdout: "pipe", stderr: "pipe" });
  record({ kind: "launch", label, bridgePid: process.pid, childPid: child.pid, binary: resolve(options.binary) });
  const pending = new Map<unknown, { at: number; method: string; action?: string; session?: string }>();
  async function forward(stream: ReadableStream<Uint8Array>, output: (line: string) => void) {
    let rest = "";
    for await (const chunk of stream.pipeThrough(new TextDecoderStream())) {
      rest += chunk;
      let end: number;
      while ((end = rest.indexOf("\n")) >= 0) { output(rest.slice(0, end)); rest = rest.slice(end + 1); }
    }
    if (rest) output(rest);
  }
  const input = forward(Bun.stdin.stream(), line => {
    const message = JSON.parse(line);
    if (message.id !== undefined) {
      const params = message.params ?? {};
      const args = params.arguments ?? {};
      const session = params._meta?.["ai.opencode/sessionID"] ?? args.__opencode_session_id;
      if (message.method === "tools/call") {
        if (typeof session !== "string" || !session) throw new Error("Trusted host session identity is missing");
        if (args.__opencode_session_id && args.__opencode_session_id !== session) throw new Error("Conflicting host session identities");
      }
      pending.set(message.id, { at: Date.now(), method: message.method, action: params.name === "dispatch" ? args.action : params.name,
        session });
      record({ kind: "request", message });
    }
    child.stdin.write(JSON.stringify(message) + "\n");
    child.stdin.flush();
  });
  const output = forward(child.stdout, line => {
    const message = JSON.parse(line);
    const call = pending.get(message.id);
    pending.delete(message.id);
    record({ kind: "response", ...call, durationMs: call ? Date.now() - call.at : undefined,
      wireBytes: Buffer.byteLength(line), message });
    process.stdout.write(line + "\n");
  });
  const stderr = forward(child.stderr, line => appendFileSync(`${root}/${label}-stderr.log`, line + "\n"));
  const finished = Promise.all([output, stderr, child.exited]);
  let terminating = false;
  process.on("SIGTERM", () => {
    terminating = true;
    child.kill("SIGTERM");
  });
  try {
    // A dead worker must close the adapter even while the host keeps stdin open.
    await Promise.race([finished, input.then(() => { child.stdin.end(); })]);
    const [, , exitCode] = await finished;
    process.exit(terminating ? 0 : exitCode);
  } catch (error) {
    child.kill("SIGTERM");
    await Promise.allSettled([output, stderr, child.exited]);
    throw error;
  }
} else if (mode === "config") {
  const condition = options.condition;
  if (condition !== "before" && condition !== "after") throw new Error("config requires --condition=before|after");
  const servers: Record<string, unknown> = { browser: { disabled: true }, "context7-broker": { disabled: true }, composio: { disabled: true } };
  for (const label of ["before", "after"]) for (const seed of [1, 2]) {
    servers[`browser_trial_${label}_${seed}`] = { type: "local", codemode: false, disabled: label !== condition,
      command: ["bun", resolve(process.argv[1]), "mcp", `--root=${root}`, `--binary=${resolve(options[label] ?? `${root}/bin/${label}`)}`,
        `--label=${label}`, `--run=${label}-${seed}`], timeout: { startup: 30_000, catalog: 30_000, execution: 60_000 } };
  }
  console.log(JSON.stringify({ $schema: "https://opencode.ai/v2/config.json", mcp: { servers } }, null, 2));
} else if (mode === "prompt") {
  if (!options.run || !options.skill) throw new Error("prompt requires --run and --skill");
  const product = fixture(options.run);
  const server = `browser_trial_${options.run.replace("-", "_")}`;
  console.log(`You are a browser-MCP usability test subject. Do the task yourself; do not delegate or implement anything. Use ONLY the MCP server "${server}" and its help/dispatch tools for site interaction. The host may expose these as ${server}_help/${server}_dispatch or tools["${server}"].help/dispatch; either spelling/wrapper is permitted. Discover them using the tool catalog if needed. Do not use other browser servers, shell/files, web/fetch tools, evaluate_script, CDP, DOM changes, or source inspection. Do not edit files. If this server is missing or broken, stop and report it without fallback.

Open http://127.0.0.1:${options.port ?? "19340"}/run/${options.run}. Find ${product.name}. Read its SKU, unit price, and stock. Open its Details link in its new tab, read the warranty length, and return to the shop (closing the details tab is fine). Order ${product.quantity} units as customer Ada Example with coupon ORBIT10 and Express shipping. The dummy checkout intentionally fails the first valid submission without placing an order: inspect the page, retry once, and verify one confirmed order. Read the receipt and final total. Use UI controls and fresh snapshot UIDs. Limit: 50 browser calls.

Return ONLY JSON: {run,completed,sku,unitPrice,stock,warrantyMonths,receipt,total,blockers}. Prices are numeric euros. Do not guess or claim completion without UI confirmation.

In this installed-version skill, browser_help/browser_dispatch refer to this condition's server:
${readFileSync(options.skill, "utf8")}`);
} else if (mode === "score") {
  const runs = ["before-1", "before-2", "after-1", "after-2"];
  const answers = existsSync(`${root}/answers.json`) ? JSON.parse(readFileSync(`${root}/answers.json`, "utf8")) : [];
  const traces = new Map(["before", "after"].map(label => {
    const path = `${root}/${label}-trace.jsonl`;
    const trace = existsSync(path) ? readFileSync(path, "utf8").split("\n").filter(Boolean).map(line => JSON.parse(line)) : [];
    return [label, trace] as const;
  }));
  const scores = runs.map(run => {
    const expected = fixture(run);
    const actual = existsSync(`${root}/${run}-state.json`)
      ? JSON.parse(readFileSync(`${root}/${run}-state.json`, "utf8")) : { attempts: [], orders: [] };
    const answer = answers.find((item: { run: string }) => item.run === run);
    const order = actual.orders[0];
    const correctOrder = actual.orders.length === 1 && actual.attempts.length >= 2 && order?.sku === expected.sku &&
      Number(order?.quantity) === expected.quantity && order?.coupon === "ORBIT10" && order?.shipping === "express" &&
      order?.name === "Ada Example" && Math.round(order?.total * 100) === expected.total;
    const correctFacts = answer?.answer?.sku === expected.sku && answer?.answer?.stock === expected.stock &&
      answer?.answer?.warrantyMonths === expected.warranty && Math.round(answer?.answer?.unitPrice * 100) === expected.cents;
    const confirmed = answer?.answer?.completed === true && !!order && answer?.answer?.receipt === order.receipt &&
      Math.round(answer?.answer?.total * 100) === expected.total;
    const label = run.split("-")[0];
    const trace = traces.get(label)!;
    const navigation = trace.find(row => row.kind === "request" && row.message?.params?.arguments?.arguments?.url?.endsWith(`/run/${run}`));
    const session = navigation?.message.params._meta?.["ai.opencode/sessionID"] ?? navigation?.message.params.arguments.__opencode_session_id;
    const calls = session ? trace.filter(row => row.kind === "response" && row.method === "tools/call" && row.session === session) : [];
    const metrics = { toolErrors: 0, responseBytes: 0, textBytes: 0, structuredBytes: 0, imageBase64Bytes: 0 };
    for (const call of calls) {
      const { result, error } = call.message;
      if (error || result?.isError) metrics.toolErrors++;
      metrics.responseBytes += call.wireBytes;
      if (result?.structuredContent) metrics.structuredBytes += Buffer.byteLength(JSON.stringify(result.structuredContent));
      for (const block of result?.content ?? []) {
        if (block.type === "text") metrics.textBytes += Buffer.byteLength(block.text ?? "");
        if (block.type === "image") metrics.imageBase64Bytes += Buffer.byteLength(block.data ?? "");
      }
    }
    return { run, passed: correctOrder && correctFacts && confirmed, correctOrder, correctFacts, confirmed,
      attempts: actual.attempts.length, orders: actual.orders.length, total: order?.total,
      session, calls: calls.length, ...metrics,
      elapsedSeconds: answer?.elapsedSeconds, answer: answer?.answer, thread: answer?.thread };
  });
  save("scores.json", scores);
  console.log(JSON.stringify(scores, null, 2));
} else {
  throw new Error("Usage: bun scripts/browser-model-eval.ts site|mcp|config|prompt|score [--key=value]");
}

function shop(run: string, product: ReturnType<typeof fixture>) {
  // Long enough to expose snapshot truncation and unnecessary full-tree output.
  const products = Array.from({ length: 220 }, (_, index) => ({ name: `Workshop part ${String(index + 1).padStart(3, "0")}`,
    sku: `WP-${index + 1}`, cents: 1200 + index, stock: 5 + index % 20 }));
  products.splice(189, 0, product);
  return `<!doctype html><html lang="en"><meta charset="utf-8"><title>Orbit Parts Shop</title>
<style>body{font:16px system-ui;max-width:1100px;margin:30px auto}label{display:block;margin:14px 0}table{width:100%;text-align:left}td,th{padding:5px}button,input,select{font:inherit}#feedback{font-weight:bold}</style>
<h1>Orbit Parts Shop</h1><p>Demo only. No money, accounts, or external requests.</p>
<label>Filter catalog <input id="filter" type="search"></label><p id="count" aria-live="polite"></p>
<table><thead><tr><th>Product</th><th>SKU</th><th>Unit price</th><th>In stock</th><th>Details</th><th>Order</th></tr></thead><tbody id="rows"></tbody></table>
<section id="cart" hidden><h2 id="cartTitle">Checkout</h2><form id="order">
<label>Customer name <input id="name" required></label><label>Quantity <input id="quantity" type="number" min="1" value="1" required></label>
<label>Discount code <input id="coupon"></label><label>Shipping <select id="shipping"><option value="standard">Standard — €0.00</option><option value="express">Express — €6.00</option></select></label>
<p id="total"></p><button id="submit" type="submit">Place order</button></form><p id="feedback" role="status"></p></section>
<script>
const products=${JSON.stringify(products)};let selected;let submitting=false;
const money=cents=>'€'+(cents/100).toFixed(2);const el=id=>document.getElementById(id);
function render(){const found=products.filter(p=>p.name.toLowerCase().includes(el('filter').value.toLowerCase()));el('count').textContent=found.length+' products';el('rows').replaceChildren();for(const p of found){const row=document.createElement('tr');for(const value of [p.name,p.sku,money(p.cents),p.stock]){const cell=document.createElement('td');cell.textContent=value;row.append(cell)}const details=document.createElement('td');const link=document.createElement('a');link.textContent='Details '+p.name;link.href='/run/${run}/details';link.target='_blank';details.append(link);row.append(details);const action=document.createElement('td');const button=document.createElement('button');button.textContent='Add '+p.name;button.onclick=()=>{selected=p;el('cart').hidden=false;el('cartTitle').textContent='Checkout: '+p.name;update()};action.append(button);row.append(action);el('rows').append(row)}}
function update(){if(!selected)return;const base=selected.cents*Number(el('quantity').value);const subtotal=el('coupon').value==='ORBIT10'?Math.round(base*0.9):base;el('total').textContent='Order total: '+money(subtotal+(el('shipping').value==='express'?600:0))}
el('filter').oninput=render;for(const id of ['quantity','coupon','shipping'])el(id).oninput=update;
el('order').onsubmit=async event=>{event.preventDefault();if(submitting)return;submitting=true;el('submit').disabled=true;try{const response=await fetch('/run/${run}/order',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({sku:selected.sku,name:el('name').value,quantity:Number(el('quantity').value),coupon:el('coupon').value,shipping:el('shipping').value})});const data=await response.json();if(!response.ok){el('feedback').textContent=data.error;el('submit').disabled=false}else{el('feedback').textContent='Order confirmed: '+data.receipt+'. Total: '+money(Math.round(data.total*100));const heading=document.createElement('h2');heading.textContent='Order confirmed';el('cart').append(heading)}}finally{submitting=false}};render();
</script></html>`;
}
