/**
 * herdr-status — tell Herdr what Pi is doing, over OSC 7501.
 *
 * Herdr's core takes a terminal's agent status only from OSC 7501
 * reports in the terminal's own output stream; Pi never sends them,
 * so a Pi terminal shows no status badge in the desktop app. This
 * extension emits the reports at Pi's lifecycle points:
 *
 *   session_start   -> idle     (Pi is up, waiting for a prompt)
 *   agent_start     -> working  (a prompt run begins; once per prompt)
 *   agent_end       -> done     (the run finished; error if it failed)
 *   session_shutdown-> clear    (forget the status again)
 *
 * Writing escape sequences to stdout from an extension is Pi's own
 * documented pattern (see examples/extensions/notify.ts in the Pi
 * package). Outside Herdr the sequence is inert: terminals that do
 * not know OSC 7501 ignore it.
 *
 * Installed by fleet/install.sh to ~/.pi/agent/extensions/. A Pi
 * that is already running picks it up with /reload.
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

function report(state: string): void {
	process.stdout.write(`\x1b]7501;state=${state}:app=pi\x1b\\`);
}

export default function (pi: ExtensionAPI) {
	pi.on("session_start", async () => {
		report("idle");
	});

	pi.on("agent_start", async () => {
		report("working");
	});

	pi.on("agent_end", async (event) => {
		const messages = (event as { messages?: unknown[] }).messages ?? [];
		const lastAssistant = [...messages]
			.reverse()
			.find(
				(m): m is { stopReason?: string } =>
					typeof m === "object" && m !== null && (m as { role?: string }).role === "assistant",
			);
		report(lastAssistant?.stopReason === "error" ? "error" : "done");
	});

	pi.on("session_shutdown", async () => {
		report("clear");
	});
}
