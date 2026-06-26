/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! `code agent proxy claude <socket|port>` — runs a standalone Claude →
//! Copilot-CAPI proxy as a plain Node child process.
//!
//! The GitHub OAuth token is acquired exactly like `code agent host`: from the
//! CLI keyring (namespace `agent-host`), falling back to an interactive
//! device-code login. It is handed to the child via an environment variable so
//! it never appears in the process argument list.
//!
//! The proxy itself is the self-contained `claudeProxyMain.js` entrypoint, run
//! directly by Node (it sets `_VSCODE_FILE_ROOT` itself, like
//! `agentHostServerMain.ts`). The bind target is passed as an argument; the
//! token is injected via env so it never appears in argv.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::auth::{Auth, AuthProvider};
use crate::commands::args::AgentProxyClaudeArgs;
use crate::commands::CommandContext;
use crate::log;
use crate::tunnels::paths::SERVER_FOLDER_NAME;
use crate::tunnels::shutdown_signal::ShutdownRequest;
use crate::util::command::new_tokio_command;
use crate::util::errors::{wrap, AnyError};

/// Transpiled entrypoint (relative to `out/`), used in dev mode fallback.
const PROXY_ENTRYPOINT_REL: &str = "vs/platform/agentHost/node/claudeProxyMain.js";
/// Bundled single-file entrypoint filename — placed next to the binary in dist/.
const PROXY_BUNDLE_NAME: &str = "claudeProxyBundle.js";
/// Bundled single-file entrypoint relative to `out/` — used in server layout.
const PROXY_BUNDLE_REL: &str = "vs/platform/agentHost/node/claudeProxyBundle.js";

/// Env var carrying the GitHub OAuth token (kept out of argv).
const GITHUB_TOKEN_ENV: &str = "VSCODE_AGENT_PROXY_GITHUB_TOKEN";

/// How to invoke Node against the compiled proxy entrypoint.
struct NodeLaunch {
	/// Path to the Node executable (a bundled server `node`, or `node` on PATH).
	node: PathBuf,
	/// Path to the compiled `claudeProxyMain.js`.
	entrypoint: PathBuf,
}

pub async fn agent_proxy_claude(
	ctx: CommandContext,
	args: AgentProxyClaudeArgs,
) -> Result<i32, AnyError> {
	// 1. Acquire the GitHub token the same way the agent-host commands do:
	//    reuse a cached keyring credential, otherwise start a device-flow login.
	//    The `agent-host` namespace keeps a single shared credential across all
	//    agent-host-related commands.
	let auth = Auth::with_namespace(&ctx.paths, ctx.log.clone(), Some("agent-host".into()));
	let credential = match auth.get_current_credential() {
		Ok(Some(existing)) => existing,
		_ => auth.login_with_scopes(AuthProvider::Github, None).await?,
	};
	let token = credential.access_token().to_string();

	// 2. Resolve how to launch the Node entrypoint.
	let launch = resolve_node_launch()?;
	if !launch.entrypoint.exists() {
		return Err(wrap(
			format!("Could not find proxy entrypoint at {}", launch.entrypoint.display()),
			"Build the client (`npm run compile`), run `code agent host` once to download the server, or set VSCODE_CLI_OVERRIDE_SERVER_PATH",
		)
		.into());
	}

	// 3. Spawn the proxy. The bind target is a plain argument; the token is
	//    injected via env so it never appears in `ps`.
	let mut cmd = new_tokio_command(&launch.node);
	cmd.arg(&launch.entrypoint);
	cmd.arg(&args.target);
	cmd.env(GITHUB_TOKEN_ENV, &token);
	cmd.stdin(Stdio::null());
	cmd.stdout(Stdio::piped());
	cmd.stderr(Stdio::piped());

	let mut child = cmd
		.spawn()
		.map_err(|e| wrap(e, "failed to spawn claude proxy node process"))?;

	let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
	let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
	let mut shutdown = ShutdownRequest::create_rx([ShutdownRequest::CtrlC]);

	let log: log::Logger = ctx.log.clone();
	loop {
		tokio::select! {
			// Forward the proxy's stdout verbatim — notably the
			// `PROXY_TOKEN:<token>` / `PROXY_READY:<path>` contract line.
			Ok(Some(line)) = stdout.next_line() => {
				println!("{line}");
			}
			Ok(Some(line)) = stderr.next_line() => {
				info!(log, "[claude-proxy] {}", line);
			}
			_ = shutdown.wait() => {
				let _ = child.kill().await;
				break;
			}
			status = child.wait() => {
				return Ok(status.ok().and_then(|s| s.code()).unwrap_or(1));
			}
		}
	}

	Ok(0)
}

/// Resolve the Node executable + proxy entrypoint path.
///
/// Priority order:
/// 1. **Dev** (`VSCODE_DEV` set): locate the repo's `out/` relative to THIS
///    binary (`<repo>/cli/target/<profile>/code[.exe]`); use `node` from PATH.
/// 2. **Direct entrypoint** (`VSCODE_PROXY_ENTRYPOINT`): explicit path to any
///    `claudeProxy*.js`; use `node` from PATH.
/// 3. **Exe-adjacent bundle**: `claudeProxyBundle.js` next to THIS binary (the
///    standard `dist/` layout produced by the build scripts); use `node` from PATH.
/// 4. **Server override** (`VSCODE_CLI_OVERRIDE_SERVER_PATH` → derive server dir).
/// 5. **Fallback**: conventionally-downloaded server layout.
fn resolve_node_launch() -> Result<NodeLaunch, AnyError> {
	let exe = std::env::current_exe()
		.map_err(|e| wrap(e, "could not determine current executable path"))?;

	if std::env::var("VSCODE_DEV").is_ok() {
		// code[.exe] -> <profile> -> target -> cli -> <repo>
		let repo_root = exe
			.ancestors()
			.nth(4)
			.ok_or_else(|| {
				wrap(
					format!("unexpected executable layout: {}", exe.display()),
					"cannot locate repo root from executable path",
				)
			})?
			.to_path_buf();
		// Prefer the pre-built bundle (single file, no out/ tree needed); fall
		// back to the transpiled entrypoint for mid-iteration convenience.
		let bundle = repo_root.join("out").join(PROXY_BUNDLE_REL);
		let entrypoint = if bundle.exists() {
			bundle
		} else {
			repo_root.join("out").join(PROXY_ENTRYPOINT_REL)
		};
		return Ok(NodeLaunch {
			node: PathBuf::from("node"),
			entrypoint,
		});
	}

	if let Ok(entrypoint) = std::env::var("VSCODE_PROXY_ENTRYPOINT") {
		return Ok(NodeLaunch {
			node: PathBuf::from("node"),
			entrypoint: PathBuf::from(entrypoint),
		});
	}

	// Bundle placed next to the binary (dist/ layout: `code[.exe]` + `claudeProxyBundle.js`).
	if let Some(exe_dir) = exe.parent() {
		let bundle = exe_dir.join(PROXY_BUNDLE_NAME);
		if bundle.exists() {
			return Ok(NodeLaunch {
				node: PathBuf::from("node"),
				entrypoint: bundle,
			});
		}
	}

	if let Ok(server_entry) = std::env::var("VSCODE_CLI_OVERRIDE_SERVER_PATH") {
		// server_entry = <server_dir>/bin/code-server[.cmd]
		let entry = PathBuf::from(server_entry);
		// bin/ -> server_dir
		let server = entry
			.parent()
			.and_then(|p| p.parent())
			.ok_or_else(|| wrap("invalid override path", "VSCODE_CLI_OVERRIDE_SERVER_PATH"))?
			.to_path_buf();
		return Ok(NodeLaunch {
			node: server.join(node_binary_name()),
			entrypoint: server.join("out").join(PROXY_BUNDLE_REL),
		});
	}

	// Fallback: the conventionally-downloaded server layout.
	let server = PathBuf::from(SERVER_FOLDER_NAME);
	Ok(NodeLaunch {
		node: server.join(node_binary_name()),
		entrypoint: server.join("out").join(PROXY_BUNDLE_REL),
	})
}

fn node_binary_name() -> &'static str {
	if cfg!(windows) {
		"node.exe"
	} else {
		"node"
	}
}
