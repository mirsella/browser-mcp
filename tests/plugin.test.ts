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
		headless?: boolean;
		releaseOnIdle?: boolean;
		sessionLayout?: "shared-window" | "window-per-session";
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
	layout?: string,
	headless?: boolean,
) {
	const command: unknown[] = [expect.any(String), "--routing", routing];
	if (layout) command.push("--session-layout", layout);
	if (headless !== undefined) command.push("--headless", String(headless));
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
			["browser_click", "opencode-direct"],
			["browser_d*", "opencode-direct"],
			["browser_*", "opencode"],
			["*", "opencode"],
			["browser_help", "opencode"],
			["browser_nonexistent", "opencode"],
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

	test("configures and validates session window layout", async () => {
		const { plugin } = await hooks([], { sessionLayout: "window-per-session" });
		const config = {} as never;
		await plugin.config?.(config);
		expectRouting(config, "opencode", "window-per-session");
		expect(hooks([], { sessionLayout: "invalid" as never })).rejects.toThrow(
			"sessionLayout must be",
		);
	});

	test("configures and validates headless mode", async () => {
		for (const headless of [false, true]) {
			const { plugin } = await hooks([], { headless });
			const config = {} as never;
			await plugin.config?.(config);
			expectRouting(config, "opencode", undefined, headless);
		}
		expect(hooks([], { headless: "false" as never })).rejects.toThrow("headless must be a boolean");
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
		const failed = Promise.resolve({ exitCode: 1, stdout: "", stderr: "failed" });
		const { plugin, calls } = await hooks([failed, successful]);
		await plugin["tool.execute.before"]?.(
			{ tool: "browser_dispatch", sessionID: "idle", callID: "call-a" },
			{ args: {} },
		);
		for (const sessionID of ["idle", "idle"]) {
			await plugin.event?.({ event: { type: "session.idle", properties: { sessionID } } } as never);
		}
		expect(calls).toHaveLength(2);
	});

	test("does not resurrect a deleted session after a shared release fails", async () => {
		let failRelease: (result: ShellResult) => void = () => {};
		const pending = new Promise<ShellResult>((resolve) => {
			failRelease = resolve;
		});
		const { plugin, calls } = await hooks([pending]);
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
		failRelease({ exitCode: 1, stdout: "", stderr: "failed" });
		await Promise.all([idle, deleted]);
		await plugin.event?.({
			event: { type: "session.idle", properties: { sessionID: "session-a" } },
		} as never);
		expect(calls).toHaveLength(1);
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
