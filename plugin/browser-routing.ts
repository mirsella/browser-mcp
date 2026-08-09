import type { Plugin } from "@opencode-ai/plugin";
import { fileURLToPath } from "node:url";

const SESSION_KEY = "__opencode_session_id";
const SKILLS_PATH = fileURLToPath(new URL("../skills", import.meta.url));
const MCP_CLI = fileURLToPath(new URL("../bin/opencode-browser-mcp.cjs", import.meta.url));
const DISPATCH_TOOL = "browser_dispatch";
const DIRECT_TOOLS = `
	browser_click browser_close_page browser_drag browser_emulate browser_evaluate_script browser_fill browser_fill_form
	browser_get_console_message browser_get_network_request browser_handle_dialog browser_hover browser_lighthouse_audit
	browser_list_console_messages browser_list_network_requests browser_list_pages browser_navigate_page browser_new_page
	browser_performance_analyze_insight browser_performance_start_trace browser_performance_stop_trace browser_press_key
	browser_resize_page browser_take_heapsnapshot browser_take_screenshot browser_take_snapshot browser_type_text
	browser_upload_file browser_wait_for
`
	.trim()
	.split(/\s+/);

type ConfigWithPolicies = {
	agent?: Record<string, { permission?: unknown; tools?: unknown }>;
	permission?: unknown;
	skills?: { paths?: string[] };
	tools?: unknown;
};

type BrowserRoutingOptions = {
	compactTools?: boolean;
	headless?: boolean;
	releaseOnIdle?: boolean;
	sessionLayout?: "shared-window" | "window-per-session";
};

type PendingRelease = {
	deleted: boolean;
	task: Promise<boolean>;
};

function matchesToolPattern(pattern: string, tool: string): boolean {
	const expression = pattern
		.replace(/[.+^${}()|[\]\\]/g, "\\$&")
		.replaceAll("*", ".*")
		.replaceAll("?", ".");
	return new RegExp(`^${expression}$`).test(tool);
}

function hasPartialBrowserPolicy(value: unknown): boolean {
	if (!value || typeof value !== "object" || Array.isArray(value)) return false;
	return Object.keys(value).some((pattern) => {
		const matches = (tool: string) => matchesToolPattern(pattern, tool);
		return DIRECT_TOOLS.some(matches) && !(matches(DISPATCH_TOOL) && DIRECT_TOOLS.every(matches));
	});
}

function requiresDirectTools(config: ConfigWithPolicies): boolean {
	if (hasPartialBrowserPolicy(config.permission) || hasPartialBrowserPolicy(config.tools)) {
		return true;
	}
	return Object.values(config.agent ?? {}).some(
		(agent) => hasPartialBrowserPolicy(agent.permission) || hasPartialBrowserPolicy(agent.tools),
	);
}

export const BrowserRouting: Plugin = async ({ $ }, options) => {
	const { compactTools = true, headless, releaseOnIdle = true, sessionLayout } = (options ?? {}) as BrowserRoutingOptions;
	if (typeof compactTools !== "boolean") throw new Error("compactTools must be a boolean");
	if (headless !== undefined && typeof headless !== "boolean") throw new Error("headless must be a boolean");
	if (typeof releaseOnIdle !== "boolean") throw new Error("releaseOnIdle must be a boolean");
	if (sessionLayout !== undefined && sessionLayout !== "shared-window" && sessionLayout !== "window-per-session")
		throw new Error('sessionLayout must be "shared-window" or "window-per-session"');
	const usedSessions = new Set<string>();
	const pendingReleases = new Map<string, PendingRelease>();
	const release = (sessionID: string, deleted = false) => {
		const pending = pendingReleases.get(sessionID);
		if (pending) {
			pending.deleted ||= deleted;
			return pending;
		}
		const next: PendingRelease = {
			deleted,
			task: (async () => {
				try {
					const result = await $`${MCP_CLI} --release-session ${sessionID}`.quiet().nothrow();
					if (result.exitCode === 0) return true;
					console.error(
						`Failed to release browser session ${sessionID}: ${String(result.stderr).trim()}`,
					);
				} catch (error) {
					console.error(`Failed to release browser session ${sessionID}`, error);
				}
				return false;
			})(),
		};
		pendingReleases.set(sessionID, next);
		void next.task.finally(() => {
			if (pendingReleases.get(sessionID) === next) pendingReleases.delete(sessionID);
		});
		return next;
	};

	return {
		config: async (config) => {
			const extended = config as typeof config & ConfigWithPolicies;
			const compact = compactTools && !requiresDirectTools(extended);
			if (compactTools && !compact) {
				console.warn(
					"@mirsella/opencode-browser-mcp disabled compact tools to preserve per-action browser permissions",
				);
			}
			extended.skills ??= {};
			extended.skills.paths ??= [];
			if (!extended.skills.paths.includes(SKILLS_PATH)) extended.skills.paths.push(SKILLS_PATH);

			config.mcp ??= {};
			const existing = config.mcp.browser;
			if (existing && (existing.type !== "local" || existing.command?.[0] !== MCP_CLI)) {
				throw new Error(
					"@mirsella/opencode-browser-mcp cannot replace the existing mcp.browser configuration",
				);
			}
			const command = [MCP_CLI, "--routing", compact ? "opencode" : "opencode-direct"];
			if (sessionLayout !== undefined) command.push("--session-layout", sessionLayout);
			if (headless !== undefined) command.push("--headless", String(headless));
			config.mcp.browser = {
				type: "local",
				command,
				enabled: true,
				timeout: 600_000,
			};
		},
		"tool.execute.before": async (input, output) => {
			if (!input.tool.startsWith("browser_")) return;
			if (!input.sessionID) throw new Error("browser tool call has no OpenCode session ID");
			if (!output.args || typeof output.args !== "object" || Array.isArray(output.args)) {
				throw new Error("browser tool call has invalid arguments");
			}
			await pendingReleases.get(input.sessionID)?.task;
			if (input.tool !== "browser_help") usedSessions.add(input.sessionID);
			(output.args as Record<string, unknown>)[SESSION_KEY] = input.sessionID;
		},
		event: async ({ event }) => {
			if (releaseOnIdle && event.type === "session.idle" && usedSessions.delete(event.properties.sessionID)) {
				const pending = release(event.properties.sessionID);
				if (!(await pending.task) && !pending.deleted) usedSessions.add(event.properties.sessionID);
			}
			if (event.type === "session.deleted") {
				const id = event.properties.info.id;
				usedSessions.delete(id);
				await release(id, true).task;
			}
		},
	};
};

export default BrowserRouting;
