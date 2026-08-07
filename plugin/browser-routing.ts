import type { Plugin } from "@opencode-ai/plugin";
import { fileURLToPath } from "node:url";

const SESSION_KEY = "__opencode_session_id";
const PROJECT_ROOT = fileURLToPath(new URL("..", import.meta.url)).replace(/\/$/, "");
const SKILLS_PATH = `${PROJECT_ROOT}/skills`;
const MCP_CLI = fileURLToPath(new URL("../bin/opencode-browser-mcp.cjs", import.meta.url));

type ConfigWithSkills = {
	agent?: Record<string, { permission?: unknown; tools?: unknown }>;
	permission?: unknown;
	skills?: {
		paths?: string[];
	};
	tools?: unknown;
};

type BrowserRoutingOptions = {
	compactTools?: boolean;
	releaseOnIdle?: boolean;
};

const DIRECT_BROWSER_TOOLS = [
	"browser_click",
	"browser_close_page",
	"browser_drag",
	"browser_emulate",
	"browser_evaluate_script",
	"browser_fill",
	"browser_fill_form",
	"browser_get_console_message",
	"browser_get_network_request",
	"browser_handle_dialog",
	"browser_hover",
	"browser_lighthouse_audit",
	"browser_list_console_messages",
	"browser_list_network_requests",
	"browser_list_pages",
	"browser_navigate_page",
	"browser_new_page",
	"browser_performance_analyze_insight",
	"browser_performance_start_trace",
	"browser_performance_stop_trace",
	"browser_press_key",
	"browser_resize_page",
	"browser_take_heapsnapshot",
	"browser_take_screenshot",
	"browser_take_snapshot",
	"browser_type_text",
	"browser_upload_file",
	"browser_wait_for",
];

function matchesToolPattern(pattern: string, tool: string): boolean {
	const expression = pattern
		.replace(/[.+^${}()|[\]\\]/g, "\\$&")
		.replaceAll("*", ".*")
		.replaceAll("?", ".");
	return new RegExp(`^${expression}$`).test(tool);
}

function hasDirectBrowserPolicy(value: unknown): boolean {
	if (!value || typeof value !== "object" || Array.isArray(value)) return false;
	return Object.keys(value).some((pattern) => {
		const affectsDirectTool = DIRECT_BROWSER_TOOLS.some((tool) =>
			matchesToolPattern(pattern, tool),
		);
		return affectsDirectTool && !matchesToolPattern(pattern, "browser_dispatch");
	});
}

function requiresDirectTools(config: ConfigWithSkills): boolean {
	if (hasDirectBrowserPolicy(config.permission) || hasDirectBrowserPolicy(config.tools)) return true;
	return Object.values(config.agent ?? {}).some(
		(agent) => hasDirectBrowserPolicy(agent.permission) || hasDirectBrowserPolicy(agent.tools),
	);
}

export const BrowserRouting: Plugin = async ({ $ }, options) => {
	const { compactTools = true, releaseOnIdle = true } = (options ?? {}) as BrowserRoutingOptions;
	if (typeof compactTools !== "boolean") {
		throw new Error("compactTools must be a boolean");
	}
	if (typeof releaseOnIdle !== "boolean") {
		throw new Error("releaseOnIdle must be a boolean");
	}
	const browserSessions = new Set<string>();
	const pendingReleases = new Map<string, Promise<void>>();
	const release = (sessionID: string) => {
		const pending = pendingReleases.get(sessionID);
		if (pending) return pending;
		const task = (async () => {
			try {
				const result = await $`${MCP_CLI} --release-session ${sessionID}`.quiet().nothrow();
				if (result.exitCode === 0) return;
				browserSessions.add(sessionID);
				console.error(
					`Failed to release browser session ${sessionID}: ${String(result.stderr).trim()}`,
				);
			} catch (error) {
				browserSessions.add(sessionID);
				console.error(`Failed to release browser session ${sessionID}`, error);
			}
		})();
		pendingReleases.set(sessionID, task);
		void task.finally(() => {
			if (pendingReleases.get(sessionID) === task) pendingReleases.delete(sessionID);
		});
		return task;
	};

	return {
		config: async (config) => {
			const configWithSkills = config as typeof config & ConfigWithSkills;
			const useCompactTools = compactTools && !requiresDirectTools(configWithSkills);
			if (compactTools && !useCompactTools) {
				console.warn(
					"@mirsella/opencode-browser-mcp disabled compact tools to preserve per-action browser permissions",
				);
			}
			configWithSkills.skills ??= {};
			configWithSkills.skills.paths ??= [];
			if (!configWithSkills.skills.paths.includes(SKILLS_PATH)) {
				configWithSkills.skills.paths.push(SKILLS_PATH);
			}

			config.mcp ??= {};
			const existing = config.mcp.browser;
			if (existing && (existing.type !== "local" || existing.command?.[0] !== MCP_CLI)) {
				throw new Error(
					"@mirsella/opencode-browser-mcp cannot replace the existing mcp.browser configuration",
				);
			}
			config.mcp.browser = {
				type: "local",
				command: [
					MCP_CLI,
					"--routing",
					"opencode",
					"--tool-interface",
					useCompactTools ? "progressive" : "direct",
				],
				enabled: true,
				timeout: 600_000,
			};
		},
		"tool.execute.before": async (input, output) => {
			if (!input.tool.startsWith("browser_")) return;
			if (!input.sessionID) {
				throw new Error("browser tool call has no OpenCode session ID");
			}
			if (!output.args || typeof output.args !== "object") {
				throw new Error("browser tool call has invalid arguments");
			}

			await pendingReleases.get(input.sessionID);
			if (input.tool !== "browser_help") browserSessions.add(input.sessionID);
			(output.args as Record<string, unknown>)[SESSION_KEY] = input.sessionID;
		},
		event: async ({ event }) => {
			if (
				releaseOnIdle &&
				event.type === "session.idle" &&
				browserSessions.delete(event.properties.sessionID)
			) {
				await release(event.properties.sessionID);
			}
			if (event.type === "session.deleted") {
				browserSessions.delete(event.properties.info.id);
				await release(event.properties.info.id);
			}
		},
	};
};

export default BrowserRouting;
