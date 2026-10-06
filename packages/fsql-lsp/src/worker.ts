import { BrowserMessageReader, BrowserMessageWriter, createConnection } from 'vscode-languageserver/browser';
import { startServer } from '#connection';

startServer(createConnection(
	new BrowserMessageReader(self),
	new BrowserMessageWriter(self),
));
