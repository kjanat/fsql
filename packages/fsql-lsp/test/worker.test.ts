import { expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import type { CompletionList, Hover, InitializeResult } from 'vscode-languageserver';

test('built worker speaks LSP over postMessage without external imports', async () => {
	const url = new URL('../dist/browser/worker.mjs', import.meta.url);
	const bundle = readFileSync(url, 'utf8');
	expect(bundle).not.toMatch(/^import\s/m);
	expect(bundle).not.toContain('node:');
	const worker = new Worker(url, { type: 'module' });
	let nextId = 0;
	const pending = new Map<number, (message: { result?: unknown; error?: unknown }) => void>();
	worker.onmessage = event => pending.get(event.data.id)?.(event.data);
	function request<T>(method: string, params?: unknown): Promise<T> {
		return new Promise((resolve, reject) => {
			const id = ++nextId;
			const timer = setTimeout(() => {
				pending.delete(id);
				reject(new Error(`${method} timed out`));
			}, 3000);
			pending.set(id, message => {
				clearTimeout(timer);
				pending.delete(id);
				if (message.error) reject(new Error(JSON.stringify(message.error)));
				else resolve(message.result as T);
			});
			worker.postMessage({ jsonrpc: '2.0', id, method, params });
		});
	}
	function notify(method: string, params?: unknown) {
		worker.postMessage({ jsonrpc: '2.0', method, params });
	}
	try {
		const init = await request<InitializeResult>('initialize', { processId: null, rootUri: null, capabilities: {} });
		expect(init.serverInfo?.name).toBe('fsql-lsp');
		notify('initialized', {});
		const uri = 'file:///browser.fsql';
		notify('textDocument/didOpen', { textDocument: { uri, languageId: 'fsql', version: 1, text: 'SELECT si' } });
		const completion = await request<CompletionList>('textDocument/completion', {
			textDocument: { uri },
			position: { line: 0, character: 9 },
		});
		expect(completion.items.some(item => item.label === 'size')).toBe(true);
		notify('textDocument/didChange', {
			textDocument: { uri, version: 2 },
			contentChanges: [{ text: 'SELECT human(size)' }],
		});
		const hover = await request<Hover>('textDocument/hover', {
			textDocument: { uri },
			position: { line: 0, character: 9 },
		});
		expect(hover.contents).toMatchObject({ value: expect.stringContaining('human-readable') });
		await request('shutdown');
		notify('exit');
	} finally {
		worker.terminate();
	}
});
