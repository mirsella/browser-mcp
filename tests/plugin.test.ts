import { describe, expect, test } from "bun:test";
import BrowserPlugin from "@mirsella/opencode-browser-mcp/server";

async function hooks(options?: { enabled?: boolean; compactTools?: boolean }) {
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
		{ $: shell, directory: "/workspace", worktree: "/workspace" } as never,
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

function browserEnabled(config: { mcp?: { browser?: { enabled?: unknown } } }): unknown {
	return config.mcp?.browser?.enabled;
}

describe("BrowserRouting", () => {
	test("exports the module, registers compact routing, and injects the session ID", async () => {
		expect(BrowserPlugin.id).toBe("@mirsella/opencode-browser-mcp");
		const { plugin } = await hooks({ enabled: true });
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

	test("is disabled by default and skips the skill", async () => {
		const { plugin } = await hooks();
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
