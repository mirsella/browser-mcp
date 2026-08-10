import type { Plugin, PluginModule } from "@opencode-ai/plugin";
import { fileURLToPath } from "node:url";

const SESSION_KEY = "__opencode_session_id";
const SKILLS_PATH = fileURLToPath(new URL("../skills", import.meta.url));
const MCP_CLI = fileURLToPath(new URL("../vendor/opencode-browser-mcp", import.meta.url));
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

const server: Plugin = async ({ $ }, options) => {
	const { compactTools = true } = (options ?? {}) as BrowserRoutingOptions;
	if (typeof compactTools !== "boolean") throw new Error("compactTools must be a boolean");

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
			config.mcp.browser = {
				type: "local",
				command: compact ? [MCP_CLI] : [MCP_CLI, "--direct-tools"],
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
			(output.args as Record<string, unknown>)[SESSION_KEY] = input.sessionID;
		},
		event: async ({ event }) => {
			if (event.type !== "session.deleted") return;
			const sessionID = event.properties.info.id;
			try {
				const result = await $`${[MCP_CLI, "release-session", sessionID]}`.quiet().nothrow();
				if (result.exitCode !== 0) {
					const stderr = String(result.stderr).trim();
					console.error(
						`Failed to release browser session ${sessionID} with exit code ${result.exitCode}${stderr ? `: ${stderr}` : ""}`,
					);
				}
			} catch (error) {
				console.error(`Failed to release browser session ${sessionID}`, error);
			}
		},
	};
};

export default { id: "@mirsella/opencode-browser-mcp", server } satisfies PluginModule;
