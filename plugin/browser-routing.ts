import type { Plugin } from "@opencode-ai/plugin";
import { fileURLToPath } from "node:url";

const SESSION_KEY = "__opencode_session_id";
const PROJECT_ROOT = fileURLToPath(new URL("..", import.meta.url)).replace(/\/$/, "");
const SKILLS_PATH = `${PROJECT_ROOT}/skills`;
const MCP_CLI = fileURLToPath(new URL("../bin/opencode-browser-mcp.cjs", import.meta.url));

type ConfigWithSkills = {
	skills?: {
		paths?: string[];
	};
};

export const BrowserRouting: Plugin = async () => ({
	config: async (config) => {
		const configWithSkills = config as typeof config & ConfigWithSkills;
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
			command: [MCP_CLI],
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

		(output.args as Record<string, unknown>)[SESSION_KEY] = input.sessionID;
	},
});

export default BrowserRouting;
