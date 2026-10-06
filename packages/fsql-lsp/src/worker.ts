import { startServer } from '#connection';
import { BrowserMessageReader, BrowserMessageWriter, createConnection } from 'vscode-languageserver/browser';

startServer(createConnection(
	new BrowserMessageReader(self),
	new BrowserMessageWriter(self),
));
