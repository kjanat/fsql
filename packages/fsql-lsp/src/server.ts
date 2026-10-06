#!/usr/bin/env node
import { startServer } from '#connection';
import { createConnection } from 'vscode-languageserver/node';

startServer(createConnection(
	process.stdin,
	process.stdout,
));
