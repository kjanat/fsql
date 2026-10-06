import type { Connection } from 'vscode-languageserver';
import { TextDocumentSyncKind, TextDocuments } from 'vscode-languageserver';
import { TextDocument } from 'vscode-languageserver-textdocument';
import { complete, hover } from '#language';
import pkg from '#pkg' with { type: 'json' };

export function startServer(connection: Connection): void {
	const documents = new TextDocuments(TextDocument);

	connection.onInitialize(() => ({
		serverInfo: { name: pkg.name.replace(/^@[^/]+\//, ''), version: pkg.version },
		capabilities: {
			positionEncoding: 'utf-16',
			textDocumentSync: TextDocumentSyncKind.Incremental,
			completionProvider: { triggerCharacters: ['.'] },
			hoverProvider: true,
		},
	}));
	connection.onCompletion(params => {
		const document = documents.get(params.textDocument.uri);
		return document ? { isIncomplete: true, items: complete(document, params.position) } : [];
	});
	connection.onHover(params => {
		const document = documents.get(params.textDocument.uri);
		return document ? hover(document, params.position) : null;
	});
	documents.listen(connection);
	connection.listen();
}
