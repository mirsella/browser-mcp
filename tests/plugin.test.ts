import { describe, expect, test } from "bun:test";
import { BrowserRouting } from "../plugin/browser-routing";

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
		socket?: string;
	},
) {
	const calls: unknown[][] = [];
	const plugin = await BrowserRouting(
		{ $: shell(results, calls), directory: "/workspace", worktree: "/workspace" } as never,
		options,
	);
	return { plugin, calls };
}

function expectRouting(
	config: { mcp: { browser: { command: unknown[] } } },
	routing: string,
	socket?: string,
) {
	const command: unknown[] = [expect.any(String)];
	if (socket) command.push("--socket", socket);
	command.push("--routing", routing);
	expect(config.mcp.browser.command).toEqual(command);
}

describe("BrowserRouting", () => {
	test("registers compact routing and injects the session ID", async () => {
		const { plugin } = await hooks();
		const config = { skills: { paths: ["/existing"] } } as never;
		await plugin.config?.(config);
		await plugin.config?.(config);
		expectRouting(config, "opencode");
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
		for (const [pattern, routing] of [
			["*_evaluate_scrip?", "opencode-direct"],
			["browser_d*", "opencode-direct"],
			["browser_*", "opencode"],
			["read_*", "opencode"],
		]) {
			const { plugin } = await hooks();
			const config = { permission: { [pattern]: "deny" } } as never;
			await plugin.config?.(config);
			expectRouting(config, routing);
		}
		const { plugin } = await hooks([], { compactTools: false });
		const config = {} as never;
		await plugin.config?.(config);
		expectRouting(config, "opencode-direct");
	});

	test("configures and validates the broker socket", async () => {
		const socket = "/run/user/1000/browser.sock";
		const { plugin, calls } = await hooks([], { socket });
		const config = {} as never;
		await plugin.config?.(config);
		expectRouting(config, "opencode", socket);
		await plugin.event?.({
			event: { type: "session.deleted", properties: { info: { id: "session-a" } } },
		} as never);
		expect(calls[0]?.[1]).toEqual([
			expect.any(String),
			"--socket",
			socket,
			"release-session",
			"session-a",
		]);
		expect(hooks([], { socket: "" })).rejects.toThrow("socket must be a non-empty string");
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
