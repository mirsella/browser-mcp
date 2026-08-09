#!/usr/bin/env node

const { X_OK } = require("node:constants");
const { accessSync, chmodSync, existsSync } = require("node:fs");
const { join, resolve } = require("node:path");
const { spawn } = require("node:child_process");

if (process.platform !== "linux" || process.arch !== "x64") {
	console.error("opencode-browser-mcp currently supports Linux x64 only");
	process.exit(1);
}

const root = resolve(__dirname, "..");
const candidates = [
	join(root, "target", "release", "opencode-browser-mcp"),
	join(root, "target", "x86_64-unknown-linux-musl", "release", "opencode-browser-mcp"),
	join(root, "vendor", "opencode-browser-mcp"),
];
const binary = candidates.find(existsSync);

if (!binary) {
	console.error("opencode-browser-mcp native binary is missing; run `cargo build --release`");
	process.exit(1);
}

try {
	accessSync(binary, X_OK);
} catch {
	try {
		chmodSync(binary, 0o755);
	} catch (error) {
		console.error(`opencode-browser-mcp native binary is not executable: ${error.message}`);
		process.exit(1);
	}
}

const child = spawn(binary, process.argv.slice(2), {
	stdio: "inherit",
	env: process.env,
});

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
	process.on(signal, () => child.kill(signal));
}

child.on("error", (error) => {
	console.error(`failed to start ${binary}: ${error.message}`);
	process.exit(1);
});
child.on("exit", (code, signal) => {
	if (!signal) process.exit(code ?? 1);
	process.removeAllListeners(signal);
	process.kill(process.pid, signal);
});
