import { expect, test } from 'bun:test';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { fileURLToPath } from 'node:url';
import {
	CompletionRequest,
	createProtocolConnection,
	DidChangeTextDocumentNotification,
	DidCloseTextDocumentNotification,
	DidOpenTextDocumentNotification,
	ExitNotification,
	HoverRequest,
	InitializedNotification,
	InitializeRequest,
	ShutdownRequest,
	StreamMessageReader,
	StreamMessageWriter,
} from 'vscode-languageserver/node';
import { TextDocument } from 'vscode-languageserver-textdocument';
import { complete, hover } from '#language';

function completions(text: string, offset = text.length) {
	const doc = TextDocument.create('file:///test.fsql', 'fsql', 1, text);
	return complete(doc, doc.positionAt(offset));
}

test('table context, qualified columns, prefix replacement, and case-insensitive hover', () => {
	expect(completions('SELECT * FROM ').map(item => item.label)).toEqual(['files', 'mounts', 'xattrs', 'acls']);
	expect(completions('SELECT mounts.').map(item => item.label)).toContain('mountpoint');
	expect(completions('SELECT mounts.').map(item => item.label)).not.toContain('size');
	const [size] = completions('SELECT sizex', 9);
	expect(size?.label).toBe('size');
	expect(size?.textEdit).toEqual({
		range: { start: { line: 0, character: 7 }, end: { line: 0, character: 12 } },
		newText: 'size',
	});
	const doc = TextDocument.create('file:///test.fsql', 'fsql', 1, 'SELECT HUMAN(size)');
	expect(hover(doc, { line: 0, character: 9 })?.contents).toMatchObject({
		kind: 'plaintext',
		value: expect.stringContaining('human-readable'),
	});
});

test('no suggestions inside comments or quoted text, including unfinished tokens', () => {
	for (
		const text of [
			"SELECT 'si",
			"SELECT 'it''s si",
			'SELECT "si',
			'-- si',
			'/* outer /* inner */ si',
			'SELECT $$si',
			'SELECT $tag$si',
		]
	) {
		expect(completions(text)).toEqual([]);
	}
	expect(completions('-- comment\nSELECT si').map(item => item.label)).toContain('size');
	expect(completions('-- si\nSELECT size', 5)).toEqual([]);
	expect(completions("SELECT '😀', si")[0]?.textEdit).toMatchObject({
		range: { start: { line: 0, character: 13 }, end: { line: 0, character: 15 } },
	});
	const doc = TextDocument.create('file:///test.fsql', 'fsql', 1, "SELECT 'size'");
	expect(hover(doc, { line: 0, character: 10 })).toBeNull();
});

test.each([
	"SELECT 'it''s', si",
	'SELECT "a""b", si',
	'SELECT `a``b`, si',
	'/* outer /* inner */ still outer */ SELECT si',
	'SELECT $$inside -- comment$$, si',
	'SELECT $tag$inside /* comment */$tag$, si',
	'-- comment\r\nSELECT si',
])('completion resumes after closed comments and quoted text: %s', text => {
	const doc = TextDocument.create('file:///test.fsql', 'fsql', 1, text);
	const size = complete(doc, doc.positionAt(text.length)).find(item => item.label === 'size');
	expect(size?.textEdit).toEqual({
		range: { start: doc.positionAt(text.length - 2), end: doc.positionAt(text.length) },
		newText: 'size',
	});
});

test.each([
	[process.execPath, '../src/server.ts', []],
	['node', '../src/server.ts', []],
	['node', '../dist/node/server.mjs', []],
	...(Bun.which('deno')
		? [
			['deno', '../src/server.ts', [
				'run',
				'--allow-env',
				'--allow-read',
				'--node-modules-dir=manual',
				'--no-lock',
			]] as const,
		]
		: []),
])('stdio lifecycle with %s %s', async (runtime, entry, args) => {
	const child = spawn(runtime, [...args, fileURLToPath(new URL(entry, import.meta.url)), '--stdio'], {
		stdio: 'pipe',
	});
	const exited = once(child, 'exit');
	let stderr = '';
	child.stderr.on('data', chunk => {
		stderr += chunk;
	});
	const connection = createProtocolConnection(
		new StreamMessageReader(child.stdout),
		new StreamMessageWriter(child.stdin),
	);
	connection.listen();
	const timer = setTimeout(() => child.kill(), 8000);
	try {
		const init = await connection.sendRequest(InitializeRequest.type, {
			processId: null,
			rootUri: null,
			capabilities: {},
		});
		expect(init.serverInfo?.name).toBe('fsql-lsp');
		expect(init.capabilities.hoverProvider).toBe(true);
		expect(init.capabilities.textDocumentSync).toBe(2);
		await connection.sendNotification(InitializedNotification.type, {});
		const uri = 'file:///smoke.fsql';
		await connection.sendNotification(DidOpenTextDocumentNotification.type, {
			textDocument: { uri, languageId: 'fsql', version: 1, text: 'SELECT si' },
		});
		const completion = await connection.sendRequest(CompletionRequest.type, {
			textDocument: { uri },
			position: { line: 0, character: 9 },
		});
		expect(completion && !Array.isArray(completion) && completion.items.some(item => item.label === 'size')).toBe(true);
		await connection.sendNotification(DidChangeTextDocumentNotification.type, {
			textDocument: { uri, version: 2 },
			contentChanges: [{
				range: { start: { line: 0, character: 7 }, end: { line: 0, character: 9 } },
				text: 'human(size)',
			}],
		});
		const result = await connection.sendRequest(HoverRequest.type, {
			textDocument: { uri },
			position: { line: 0, character: 8 },
		});
		expect(result?.contents).toMatchObject({ value: expect.stringContaining('human-readable') });
		await connection.sendNotification(DidCloseTextDocumentNotification.type, { textDocument: { uri } });
		expect(
			await connection.sendRequest(HoverRequest.type, { textDocument: { uri }, position: { line: 0, character: 8 } }),
		).toBeNull();
		await connection.sendRequest(ShutdownRequest.type);
		await connection.sendNotification(ExitNotification.type);
		expect((await exited)[0]).toBe(0);
		expect(stderr).toBe('');
	} finally {
		clearTimeout(timer);
		connection.dispose();
		if (child.exitCode === null) child.kill();
	}
}, 10000);
