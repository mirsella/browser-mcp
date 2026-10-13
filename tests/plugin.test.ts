import { afterEach, describe, expect, test } from "bun:test";
import BrowserPlugin from "@mirsella/opencode-browser-mcp/server";

const disposals: Array<() => Promise<void>> = [];
afterEach(async () => {
	await Promise.all(disposals.splice(0).map((dispose) => dispose()));
});

async function hooks(options?: { enabled?: boolean; compactTools?: boolean }, mcp?: unknown) {
	const calls: unknown[][] = [];
	const shell = ((strings: TemplateStringsArray, ...values: unknown[]) => {
		calls.push([strings, ...values]);
		const command = {
			quiet: () => command,
			nothrow: async () => ({ exitCode: 0, stdout: "", stderr: "" }),
		};
		return command;
	}) as never;
	const plugin = await BrowserPlugin.server(
		{
			$: shell,
			directory: "/workspace",
			worktree: "/workspace",
			client: {
				mcp: mcp ?? {
					status: async () => ({ data: { browser: { status: "connected" } } }),
				},
			},
		} as never,
		options,
	);
	disposals.push(async () => {
		await plugin.event?.({ event: { type: "server.instance.disposed", properties: {} } } as never);
	});
	return { plugin, calls };
}

function expectRouting(
	config: { mcp: { browser: { command: unknown[] } } },
	directTools: boolean,
) {
	const [executable, ...arguments_] = config.mcp.browser.command;
	expect(String(executable).endsWith("/vendor/browser-mcp")).toBeTrue();
	expect(arguments_).toEqual(directTools ? ["--direct-tools"] : []);
}

function browserEnabled(config: { mcp?: { browser?: { enabled?: unknown } } }): unknown {
	return config.mcp?.browser?.enabled;
}

describe("BrowserRouting", () => {
	test("reconnects a crashed adapter once across concurrent hooks", async () => {
		let state = "failed";
		const connections: unknown[] = [];
		const { plugin } = await hooks({ enabled: true }, {
			status: async () => ({ data: { browser: { status: state } } }),
			connect: async (options: unknown) => {
				connections.push(options);
				await Bun.sleep(10);
				state = "connected";
				return { data: true };
			},
		});
		await plugin.config?.({} as never);
		await Promise.all(Array.from({ length: 4 }, () => plugin["chat.message"]?.({} as never, {} as never)));
		expect(connections).toEqual([
			{ path: { name: "browser" }, query: { directory: "/workspace" }, throwOnError: true },
		]);
		state = "disabled";
		await plugin["chat.message"]?.({} as never, {} as never);
		expect(connections).toHaveLength(1);
	});

	test("retries unsuccessful reconnects and stops on disposal", async () => {
		let state = "failed";
		let connections = 0;
		const { plugin } = await hooks({ enabled: true }, {
			status: async () => ({ data: { browser: { status: state } } }),
			connect: async () => {
				connections++;
				if (connections > 1) state = "connected";
				return { data: true };
			},
		});
		await plugin.config?.({} as never);
		await plugin["chat.message"]?.({} as never, {} as never);
		expect(connections).toBe(1);
		await Bun.sleep(1_100);
		expect(connections).toBe(2);
		await plugin.event?.({ event: { type: "server.instance.disposed", properties: {} } } as never);
		state = "failed";
		await plugin["chat.message"]?.({} as never, {} as never);
		expect(connections).toBe(2);
	});

	test("recovers an adapter enabled at runtime when its tools disappear", async () => {
		let state = "disabled";
		let connections = 0;
		const { plugin } = await hooks({}, {
			status: async () => ({ data: { browser: { status: state } } }),
			connect: async () => {
				connections++;
				state = "connected";
				return { data: true };
			},
		});
		await plugin.config?.({} as never);
		await plugin["chat.message"]?.({} as never, {} as never);
		expect(connections).toBe(0);
		state = "failed";
		await plugin.event?.({ event: { type: "mcp.tools.changed", properties: { server: "browser" } } } as never);
		await plugin["chat.message"]?.({} as never, {} as never);
		expect(connections).toBe(1);
	});

	test("enables compact routing by default and injects the session ID", async () => {
		expect(BrowserPlugin.id).toBe("@mirsella/opencode-browser-mcp");
		const { plugin } = await hooks();
		const config = { skills: { paths: ["/existing"] } } as never;
		await plugin.config?.(config);
		await plugin.config?.(config);
		expectRouting(config, false);
		expect(browserEnabled(config)).toBeTrue();
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

	test("allows explicit disablement and skips the skill", async () => {
		const { plugin } = await hooks({ enabled: false });
		const config = { skills: { paths: ["/existing"] } } as never;
		await plugin.config?.(config);
		expect(browserEnabled(config)).toBeFalse();
		expectRouting(config, false);
		expect(config.skills.paths).toHaveLength(1);
		await expect(
			hooks({ enabled: "true" } as never),
		).rejects.toThrow("enabled must be a boolean");
	});

	test("explicit mcp.browser settings take precedence", async () => {
		const { plugin } = await hooks({ enabled: true });
		const disabled = { mcp: { browser: { enabled: false } }, skills: { paths: [] as string[] } } as never;
		await plugin.config?.(disabled);
		expect(browserEnabled(disabled)).toBeFalse();
		expect((disabled as { skills: { paths: string[] } }).skills.paths).toHaveLength(0);

		const optedIn = { mcp: { browser: { enabled: true } } } as never;
		await plugin.config?.(optedIn);
		expectRouting(optedIn, false);
		expect(browserEnabled(optedIn)).toBeTrue();
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
		const { plugin } = await hooks({ compactTools: false });
		const config = {} as never;
		await plugin.config?.(config);
		expectRouting(config, true);
	});

	test("ignores idle and releases deleted sessions", async () => {
		const { plugin, calls } = await hooks();
		await plugin["tool.execute.before"]?.(
			{ tool: "browser_dispatch", sessionID: "session-a", callID: "call-a" },
			{ args: {} },
		);
		await plugin.event?.({
			event: { type: "session.idle", properties: { sessionID: "session-a" } },
		} as never);
		expect(calls).toHaveLength(0);
		await plugin.event?.({
			event: { type: "session.deleted", properties: { info: { id: "restored-session" } } },
		} as never);
		expect(calls).toHaveLength(1);
		expect(calls[0]?.[1]).toEqual([expect.any(String), "release-session", "restored-session"]);
	});
});
