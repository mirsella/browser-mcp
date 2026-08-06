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

type BrowserRoutingOptions = {
	releaseOnIdle?: boolean;
};

export const BrowserRouting: Plugin = async ({ $ }, options) => {
	const { releaseOnIdle = true } = (options ?? {}) as BrowserRoutingOptions;
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
				command: [MCP_CLI, "--routing", "opencode"],
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
			browserSessions.add(input.sessionID);
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
