"""Headless, local browser-use 1.0.0 MCP fixture adapter. Install in a task venv."""
import argparse
import asyncio
import os
import signal
from pathlib import Path
from importlib.metadata import version

parser = argparse.ArgumentParser()
parser.add_argument("--profile", type=Path, required=True)
parser.add_argument("--ipc", type=Path, required=True)
parser.add_argument("--browser", default="/usr/bin/chromium")
args = parser.parse_args()
if version("browser-use") != "1.0.0":
    raise RuntimeError("This evaluation requires browser-use==1.0.0")
for key in list(os.environ):
    if key.startswith(("BU_", "BH_")) or key in ("BROWSER_USE_API_KEY", "DISPLAY", "WAYLAND_DISPLAY"):
        del os.environ[key]
args.ipc.mkdir(parents=True, exist_ok=True, mode=0o700)
os.environ.update(BH_RUNTIME_DIR=str(args.ipc.resolve()), BH_TMP_DIR=str(args.ipc.resolve()),
                  ANONYMIZED_TELEMETRY="false", DO_NOT_TRACK="1")

from browser_use.mcp.cli_mcp import CLIMCPServer


async def main():
    server = CLIMCPServer(own_browser=True, headless=True,
                         executable_path=args.browser, user_data_dir=str(args.profile.resolve()))
    task = asyncio.current_task()
    if task is None:
        raise RuntimeError("MCP server has no lifecycle task")
    asyncio.get_running_loop().add_signal_handler(signal.SIGTERM, task.cancel)
    try:
        await server.run()
    except asyncio.CancelledError:
        pass
    finally:
        await server._browser.stop()


asyncio.run(main())
