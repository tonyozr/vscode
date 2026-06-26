/*---------------------------------------------------------------------------------------------
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

// Standalone Claude → Copilot-CAPI proxy. A pure-node process (no Electron,
// no secret storage) that exposes the Anthropic Messages API on a local
// endpoint and forwards to CAPI, mirroring the in-agent-host
// `ClaudeProxyService` but without any session/WebSocket machinery.
//
// This is a self-contained entrypoint run directly as
// `node out/vs/platform/agentHost/node/claudeProxyMain.js <target>` by the CLI
// (`code agent proxy claude`). It does NOT go through `bootstrap-fork`.
//   - argv[2] (or VSCODE_AGENT_PROXY_CLAUDE_TARGET): unix socket path, or a
//     numeric TCP port.
//   - VSCODE_AGENT_PROXY_GITHUB_TOKEN: the GitHub OAuth token, injected via env
//     so it never shows up in `ps`.
//
// The CLI obtains the GitHub token the same way `code agent host` does — from
// the CLI's keyring, falling back to an interactive device-code login.
//
// Bind target:
//   - A numeric argument binds `127.0.0.1:<port>` (TCP). Bearer auth is
//     REQUIRED: a fresh nonce is minted at startup and printed to stdout as
//     `PROXY_TOKEN:<nonce>`. Clients send `Authorization: Bearer <nonce>`.
//   - Any other argument is treated as a unix domain socket path. Access is
//     gated by filesystem permissions, so bearer auth is DISABLED and no token
//     is printed.

import { fileURLToPath } from 'url';

// This standalone process is launched directly by the CLI (`code agent proxy
// claude`), NOT via `bootstrap-fork`/`bootstrap-esm`, so — like
// `agentHostServerMain.ts` — we must set `_VSCODE_FILE_ROOT` ourselves so
// `FileAccess` can resolve module paths. This file lives at
// out/vs/platform/agentHost/node/ — the root is `out/`.
globalThis._VSCODE_FILE_ROOT = fileURLToPath(new URL('../../../..', import.meta.url));

import * as fs from 'fs';
import { DisposableStore } from '../../../base/common/lifecycle.js';
import { ConsoleLogger, ILogService, LogLevel } from '../../log/common/log.js';
import { LogService } from '../../log/common/logService.js';
import product from '../../product/common/product.js';
import { IProductService } from '../../product/common/productService.js';
import { CopilotApiService } from './shared/copilotApiService.js';
import { ClaudeProxyService, IClaudeProxyOptions } from './claude/claudeProxyService.js';
import { ProxyBindTarget } from './shared/loopbackProxyServer.js';

/** Env var carrying the injected GitHub OAuth token (kept out of argv). */
const GITHUB_TOKEN_ENV = 'VSCODE_AGENT_PROXY_GITHUB_TOKEN';
/** Env var carrying the bind target (unix socket path or numeric TCP port). */
const TARGET_ENV = 'VSCODE_AGENT_PROXY_CLAUDE_TARGET';

/** Log to stderr so stdout stays reserved for the `PROXY_TOKEN:` contract line. */
function logStderr(msg: string): void {
	process.stderr.write(`[ClaudeProxy] ${msg}\n`);
}

/**
 * Resolve the bind target + auth policy from the single CLI argument.
 * A purely-numeric argument is a TCP port; anything else is a socket path.
 */
function resolveOptions(target: string): IClaudeProxyOptions {
	if (/^\d+$/.test(target)) {
		const port = parseInt(target, 10);
		const bindTarget: ProxyBindTarget = { kind: 'tcp', host: '127.0.0.1', port };
		return { bindTarget, requireAuth: true, allowNonceOnlyAuth: true };
	}
	const bindTarget: ProxyBindTarget = { kind: 'socket', path: target };
	return { bindTarget, requireAuth: false };
}

async function main(): Promise<void> {
	// Prefer the env var (fork-launch convention); fall back to argv[2] for
	// direct `node claudeProxyMain.js <target>` invocation.
	const target = process.env[TARGET_ENV] || process.argv[2];
	if (!target) {
		logStderr(`Error: missing bind target. Set ${TARGET_ENV} or pass <unixSocketPath | tcpPort> as an argument.`);
		process.exit(1);
	}

	const options = resolveOptions(target);

	// A stale socket file from a previous crash blocks `listen()` with
	// EADDRINUSE; remove it before binding. Best-effort — a genuinely
	// in-use socket re-creates the error below, which we surface.
	if (options.bindTarget?.kind === 'socket') {
		try {
			fs.unlinkSync(options.bindTarget.path);
		} catch {
			// no stale file, or not removable — let listen() report any real conflict
		}
	}

	const githubToken = process.env[GITHUB_TOKEN_ENV] ?? '';
	if (!githubToken) {
		// The proxy can still bind, but every upstream request will 401 from
		// CAPI. Warn loudly rather than silently serving a dead proxy.
		logStderr(`Warning: ${GITHUB_TOKEN_ENV} is empty; upstream CAPI requests will fail authentication.`);
	}

	const disposables = new DisposableStore();
	const productService: IProductService = { _serviceBrand: undefined, ...product };
	// Error level: suppress warn/info noise (e.g. getDevDeviceId missing native
	// addon) while keeping fatal errors on stderr. Stdout stays clean for the
	// PROXY_TOKEN:/PROXY_READY: contract.
	const logService: ILogService = disposables.add(new LogService(disposables.add(new ConsoleLogger(LogLevel.Error, /*useColors*/ false))));
	const copilotApiService = new CopilotApiService(undefined, logService, productService);
	const proxy = disposables.add(new ClaudeProxyService(logService, copilotApiService, options));

	const handle = await proxy.start(githubToken);
	logStderr(`listening on ${handle.baseUrl}`);

	// Machine-parseable startup contract on stdout (mirrors the agent host
	// server's `READY:<port>`). Only TCP mode requires/prints a token; the
	// unix socket relies on filesystem permissions.
	if (options.bindTarget?.kind === 'tcp') {
		process.stdout.write(`PROXY_TOKEN:${handle.nonce}\n`);
	} else {
		process.stdout.write(`PROXY_READY:${options.bindTarget?.path}\n`);
	}

	let shuttingDown = false;
	const shutdown = () => {
		if (shuttingDown) {
			return;
		}
		shuttingDown = true;
		logStderr('shutting down...');
		disposables.dispose();
		if (options.bindTarget?.kind === 'socket') {
			try {
				fs.unlinkSync(options.bindTarget.path);
			} catch {
				// already gone
			}
		}
		process.exit(0);
	};

	// The listening HTTP server keeps the event loop alive; we exit only on a
	// termination signal (the CLI parent kills this child on its own shutdown).
	// We deliberately do NOT key liveness off stdin: the CLI launches us with
	// stdin closed, which would fire an immediate 'end' and shut us down.
	process.on('SIGTERM', shutdown);
	process.on('SIGINT', shutdown);
}

main().catch(err => {
	logStderr(`fatal: ${err instanceof Error ? err.stack ?? err.message : String(err)}`);
	process.exit(1);
});
