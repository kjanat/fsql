#!/usr/bin/env node
import { createConnection } from 'vscode-languageserver/node';
import { startServer } from '#connection';

startServer(createConnection(
	process.stdin,
	process.stdout,
));
