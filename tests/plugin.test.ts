import { describe, expect, test } from "bun:test";
import BrowserPlugin from "@mirsella/opencode-browser-mcp/server";

type ShellResult = { exitCode: number; stdout: string; stderr: string };
const successful = Promise.resolve<ShellResult>({ exitCode: 0, stdout: "", stderr: "" });

function shell(results: Array<Promise<ShellResult>>, calls: unknown[][]) {
	return ((strings: TemplateStringsArray, ...values: unknown[]) => {
		calls.push([strings, ...values]);
		const result = results.shift() ?? successful;
		const command = { quiet: () => command, nothrow: () => result };
		return command;
	}) as never;
}

async function hooks(
	results: Array<Promise<ShellResult>> = [],
	options?: {
		compactTools?: boolean;
		releaseOnIdle?: boolean;
	},
) {
	const calls: unknown[][] = [];
	const plugin = await BrowserPlugin.server(
		{ $: shell(results, calls), directory: "/workspace", worktree: "/workspace" } as never,
		options,
	);
	return { plugin, calls };
}

function expectRouting(
	config: { mcp: { browser: { command: unknown[] } } },
	directTools: boolean,
) {
	const [executable, ...arguments_] = config.mcp.browser.command;
	expect(String(executable).endsWith("/vendor/opencode-browser-mcp")).toBeTrue();
	expect(arguments_).toEqual(directTools ? ["--direct-tools"] : []);
}

describe("BrowserRouting", () => {
	test("exports the module, registers compact routing, and injects the session ID", async () => {
		expect(BrowserPlugin.id).toBe("@mirsella/opencode-browser-mcp");
		const { plugin } = await hooks();
		const config = { skills: { paths: ["/existing"] } } as never;
		await plugin.config?.(config);
		await plugin.config?.(config);
		expectRouting(config, false);
		expect(config.skills.paths).toHaveLength(2);

		const output = { args: { action: "navigate_page", arguments: { url: "https://example.com" } } };
		await plugin["tool.execute.before"]?.(
			{ tool: "browser_dispatch", sessionID: "session-a", callID: "call-a" },
			output,
		);
		expect(output.args).toEqual({
			action: "navigate_page",
			arguments: { url: "https://example.com" },
			__opencode_session_id: "session-a",
		});
		expect(
			plugin["tool.execute.before"]?.(
				{ tool: "browser_dispatch", sessionID: "session-a", callID: "call-b" },
				{ args: [] },
			),
		).rejects.toThrow("invalid arguments");
	});

	test("uses direct tools for partial browser policies", async () => {
		for (const [pattern, directTools] of [
			["*_evaluate_scrip?", true],
			["browser_d*", true],
			["browser_*", false],
			["read_*", false],
		]) {
			const { plugin } = await hooks();
			const config = { permission: { [pattern]: "deny" } } as never;
			await plugin.config?.(config);
			expectRouting(config, directTools);
		}
		const { plugin } = await hooks([], { compactTools: false });
		const config = {} as never;
		await plugin.config?.(config);
		expectRouting(config, true);
	});

	test("cleans deleted sessions restored from an earlier plugin process", async () => {
		const { plugin, calls } = await hooks();
		await plugin.event?.({
			event: { type: "session.deleted", properties: { info: { id: "session-a" } } },
		} as never);
		expect(calls).toHaveLength(1);
		expect(calls[0]?.[1]).toEqual([
			expect.any(String),
			"release-session",
			"session-a",
		]);
	});

	test("does not treat compact help as a used browser session", async () => {
		const { plugin, calls } = await hooks();
		await plugin["tool.execute.before"]?.(
			{ tool: "browser_help", sessionID: "session-a", callID: "call-a" },
			{ args: {} },
		);
		await plugin.event?.({
			event: { type: "session.idle", properties: { sessionID: "session-a" } },
		} as never);
		expect(calls).toHaveLength(0);
	});

	test("can retain tabs between turns but still cleans deleted sessions", async () => {
		const { plugin, calls } = await hooks([], { releaseOnIdle: false });
		await plugin["tool.execute.before"]?.(
			{ tool: "browser_dispatch", sessionID: "session-a", callID: "call-a" },
			{ args: {} },
		);
		await plugin.event?.({
			event: { type: "session.idle", properties: { sessionID: "session-a" } },
		} as never);
		expect(calls).toHaveLength(0);
		await plugin.event?.({
			event: { type: "session.deleted", properties: { info: { id: "session-a" } } },
		} as never);
		expect(calls).toHaveLength(1);
	});

	test("retries failed idle cleanup", async () => {
		const failed = Promise.resolve<ShellResult>({ exitCode: 1, stdout: "", stderr: "broker unavailable" });
		const { plugin, calls } = await hooks([failed, successful]);
		await plugin["tool.execute.before"]?.(
			{ tool: "browser_dispatch", sessionID: "session-a", callID: "call-a" },
			{ args: {} },
		);
		const idle = { event: { type: "session.idle", properties: { sessionID: "session-a" } } } as never;
		await plugin.event?.(idle);
		await plugin.event?.(idle);
		expect(calls).toHaveLength(2);
	});

	test("retries a failed pending release when the session is deleted", async () => {
		let failFirst: (result: ShellResult) => void = () => {};
		const first = new Promise<ShellResult>((resolve) => {
			failFirst = resolve;
		});
		const { plugin, calls } = await hooks([first, successful]);
		await plugin["tool.execute.before"]?.(
			{ tool: "browser_dispatch", sessionID: "session-a", callID: "call-a" },
			{ args: {} },
		);
		const idle = plugin.event?.({
			event: { type: "session.idle", properties: { sessionID: "session-a" } },
		} as never);
		await Promise.resolve();
		const deleted = plugin.event?.({
			event: { type: "session.deleted", properties: { info: { id: "session-a" } } },
		} as never);
		expect(calls).toHaveLength(1);
		failFirst({ exitCode: 1, stdout: "", stderr: "broker unavailable" });
		await Promise.all([idle, deleted]);
		expect(calls).toHaveLength(2);
	});

	test("waits for idle cleanup before routing the next turn", async () => {
		let completeFirst: (result: ShellResult) => void = () => {};
		const first = new Promise<ShellResult>((resolve) => {
			completeFirst = resolve;
		});
		const { plugin, calls } = await hooks([first, successful]);
		const input = { tool: "browser_dispatch", sessionID: "session-a", callID: "call-a" };
		await plugin["tool.execute.before"]?.(input, { args: {} });
		const firstIdle = plugin.event?.({
			event: { type: "session.idle", properties: { sessionID: "session-a" } },
		} as never);
		const nextCall = plugin["tool.execute.before"]?.(input, { args: {} });
		await Promise.resolve();
		expect(calls).toHaveLength(1);
		completeFirst({ exitCode: 0, stdout: "", stderr: "" });
		await Promise.all([firstIdle, nextCall]);
		await plugin.event?.({
			event: { type: "session.idle", properties: { sessionID: "session-a" } },
		} as never);
		expect(calls).toHaveLength(2);
	});
});
