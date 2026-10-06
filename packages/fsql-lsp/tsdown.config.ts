import { defineConfig } from 'tsdown';

export default defineConfig([
	{
		entry: ['src/server.ts'],
		outDir: 'dist/node',
		platform: 'node',
		target: 'node22',
		format: 'esm',
		outExtensions: () => ({ js: '.mjs' }),
		sourcemap: true,
		clean: true,
		dts: false,
	},
	{
		entry: ['src/worker.ts'],
		outDir: 'dist/browser',
		platform: 'browser',
		target: 'es2022',
		format: 'esm',
		outExtensions: () => ({ js: '.mjs' }),
		deps: { alwaysBundle: [/^vscode-/] },
		sourcemap: true,
		clean: true,
		dts: false,
	},
]);
