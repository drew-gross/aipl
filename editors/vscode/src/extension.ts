// The AIPL extension's whole job is to start the language server and get out
// of the way: every feature it provides — go-to-definition, hover, the
// outline, formatting, diagnostics — is answered by `aipl lsp`, so that what
// the editor says about a program is what the compiler says about it.
//
// The one decision made here is *which* `aipl` to run. See `discoverServer`.

import { execFile } from 'child_process';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';
import {
	LanguageClient,
	LanguageClientOptions,
	ServerOptions,
	TransportKind,
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
	const { command, tried } = await discoverServer();

	const serverOptions: ServerOptions = {
		command,
		args: ['lsp'],
		transport: TransportKind.stdio,
	};
	const clientOptions: LanguageClientOptions = {
		documentSelector: [{ scheme: 'file', language: 'aipl' }],
		// The server reads imported files from disk itself, so it does not need
		// to be told about changes to files nobody has open.
		outputChannelName: 'AIPL',
	};

	client = new LanguageClient('aipl', 'AIPL', serverOptions, clientOptions);
	context.subscriptions.push(client);

	try {
		await client.start();
	} catch (error) {
		// The overwhelmingly likely cause is that there is no compiler at
		// `command`. Listing everywhere it looked is what makes that
		// actionable: `spawn aipl ENOENT` on its own does not say that the
		// search had already been through three other paths.
		const places = tried.map((place) => `\n  ${place}`).join('');
		void vscode.window.showErrorMessage(
			`AIPL: could not start the language server (\`${command} lsp\`: ${error}). ` +
				`Looked for a compiler in:${places}\n` +
				'Set `aipl.serverPath` to the one to use — `${workspaceFolder}` is expanded.',
		);
	}
}

export function deactivate(): Thenable<void> | undefined {
	return client?.stop();
}

// Where the chosen compiler came from, and everywhere that was looked, each
// with why it was passed over — which is what an error message needs, since
// the useful part of a failure is the search that came before it.
interface Discovery {
	command: string;
	tried: string[];
}

// Where a compiler sits, relative to a workspace folder, in the order a
// project means them.
const CANDIDATE_DIRECTORIES: string[][] = [
	// Beside the source: a project carries the compiler that builds it
	// (DESIGN_PRINCIPLES.md §4), and the editor should report what *that*
	// compiler reports rather than whatever version is on the machine.
	[],
	// Working on the compiler itself, where the compiler a checkout contains
	// is the one it just built. `cargo build` makes the debug one, which is
	// what the repository's own instructions use, so it is looked for first.
	['target', 'debug'],
	['target', 'release'],
];

// Which `aipl` to run: `aipl.serverPath` if set, then the first candidate
// above that can actually serve, then `aipl` on `PATH` for a loose file opened
// outside any project.
//
// "Can actually serve" is asked rather than assumed, because a checkout holds
// more than one compiler and they are not the same age: a `target/release`
// binary from last month sits beside the `target/debug` one built five minutes
// ago, and nothing about either path says which of them knows the `lsp`
// subcommand. Guessing from the path picks the stale one about as often as not,
// and the failure that follows looks nothing like its cause.
//
// A multi-root workspace gets one server, from the first folder that carries a
// usable compiler. Two projects on two compiler versions is the case that
// deserves a client per folder; until then, `aipl.serverPath` is the override.
async function discoverServer(): Promise<Discovery> {
	const configured = vscode.workspace.getConfiguration('aipl').get<string>('serverPath');
	if (configured) {
		// An explicit setting is a decision, not a hint: if it names nothing
		// usable, that is what the error should be about, rather than quietly
		// starting some other compiler.
		const expanded = expandVariables(configured);
		return { command: expanded, tried: [`${expanded} — from aipl.serverPath`] };
	}

	const tried: string[] = [];
	for (const folder of vscode.workspace.workspaceFolders ?? []) {
		for (const directory of CANDIDATE_DIRECTORIES) {
			const candidate = path.join(folder.uri.fsPath, ...directory, executableName());
			if (!isExecutableFile(candidate)) {
				tried.push(`${candidate} — not there`);
				continue;
			}
			if (!(await servesLsp(candidate))) {
				tried.push(`${candidate} — too old: no \`lsp\` subcommand`);
				continue;
			}
			tried.push(candidate);
			return { command: candidate, tried };
		}
	}
	// Nothing in the workspace, so fall through to `PATH` and let the spawn
	// decide. The list above is what the error message will explain.
	tried.push(`${executableName()} — on PATH`);
	return { command: executableName(), tried };
}

// Whether the compiler at `candidate` has an `lsp` subcommand, asked by
// reading its usage. One process at activation, and the answer is what keeps a
// stale binary from being spawned as a server that immediately exits.
function servesLsp(candidate: string): Promise<boolean> {
	return new Promise((resolve) => {
		execFile(candidate, ['--help'], { timeout: 30_000 }, (error, stdout) => {
			resolve(!error && /\blsp\b/.test(stdout));
		});
	});
}

// VS Code expands `${workspaceFolder}` in a task or a launch configuration,
// but not in an arbitrary setting read through `getConfiguration`, so a
// perfectly reasonable `${workspaceFolder}/target/debug/aipl` would otherwise
// be passed to `spawn` literally.
function expandVariables(value: string): string {
	const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
	return value
		.replace(/\$\{workspaceFolder\}/g, folder ?? '')
		.replace(/\$\{userHome\}/g, os.homedir());
}

function executableName(): string {
	return process.platform === 'win32' ? 'aipl.exe' : 'aipl';
}

function isExecutableFile(candidate: string): boolean {
	try {
		if (!fs.statSync(candidate).isFile()) {
			return false;
		}
		// Windows has no execute bit; being a file there is as much as can be
		// checked, and spawning it is what finds out.
		if (process.platform === 'win32') {
			return true;
		}
		fs.accessSync(candidate, fs.constants.X_OK);
		return true;
	} catch {
		return false;
	}
}
