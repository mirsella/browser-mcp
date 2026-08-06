import { describe, expect, test } from "bun:test";
import { BrowserRouting } from "../plugin/browser-routing";

type ShellResult = {
	exitCode: number;
	stdout: string;
	stderr: string;
};

const successful = Promise.resolve<ShellResult>({ exitCode: 0, stdout: "", stderr: "" });

function shell(results: Array<Promise<ShellResult>>, calls: unknown[][]) {
	return ((strings: TemplateStringsArray, ...values: unknown[]) => {
		calls.push([strings, ...values]);
		const result = results.shift() ?? successful;
		const command = {
			quiet: () => command,
			nothrow: () => result,
		};
		return command;
	}) as never;
}

async function hooks(
	results: Array<Promise<ShellResult>> = [],
	options?: { releaseOnIdle?: boolean },
) {
	const calls: unknown[][] = [];
	const plugin = await BrowserRouting(
		{
			$: shell(results, calls),
			directory: "/workspace",
			worktree: "/workspace",
		} as never,
		options,
	);
	return { plugin, calls };
}

describe("BrowserRouting", () => {
	test("registers OpenCode routing and injects the session ID", async () => {
		const { plugin } = await hooks();
		const config = {} as never;
		await plugin.config?.(config);
		expect(config).toMatchObject({
			mcp: {
				browser: {
					type: "local",
					command: [expect.any(String), "--routing", "opencode"],
					enabled: true,
				},
			},
		});

		const output = { args: { url: "https://example.com" } };
		await plugin["tool.execute.before"]?.(
			{ tool: "browser_navigate_page", sessionID: "session-a", callID: "call-a" },
			output,
		);
		expect(output.args).toEqual({
			url: "https://example.com",
			__opencode_session_id: "session-a",
		});
	});

	test("can retain tabs between idle turns but still cleans deleted sessions", async () => {
		const { plugin, calls } = await hooks([], { releaseOnIdle: false });
		await plugin["tool.execute.before"]?.(
			{ tool: "browser_list_pages", sessionID: "session-a", callID: "call-a" },
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

	test("waits for idle cleanup before routing the next turn", async () => {
		let completeFirst: (result: ShellResult) => void = () => {};
		const first = new Promise<ShellResult>((resolve) => {
			completeFirst = resolve;
		});
		const { plugin, calls } = await hooks([first, successful]);
		const input = { tool: "browser_list_pages", sessionID: "session-a", callID: "call-a" };

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
