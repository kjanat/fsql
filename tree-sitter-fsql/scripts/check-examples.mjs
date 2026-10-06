import { spawnSync } from 'node:child_process';
import { mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const examples = readdirSync(new URL('../examples/', import.meta.url))
	.filter(name => name.endsWith('.fsql'))
	.sort()
	.map(name => `examples/${name}`);

function run(...args) {
	const result = spawnSync('tree-sitter', args, { cwd: root, encoding: 'utf8' });
	if (result.error) throw result.error;
	if (result.status !== 0) {
		throw new Error(result.stdout + result.stderr);
	}
	return result.stdout;
}

run('parse', '--quiet', ...examples);
run('highlight', '--quiet', '--scope', 'source.fsql', ...examples);
run('query', '--quiet', 'queries/indents.scm', ...examples);
const zedQueries = '../../editors/zed/languages/fsql/';
for (const name of readdirSync(new URL(zedQueries, import.meta.url)).filter(name => name.endsWith('.scm'))) {
	run('query', '--quiet', `../editors/zed/languages/fsql/${name}`, ...examples);
}
// Match an incremental reparse to a fresh parse, including scanner-backed
// dollar strings. Both trees must retain the statement after the edit.
const temporary = mkdtempSync(join(tmpdir(), 'fsql-grammar-'));
try {
	const original = join(temporary, 'original.fsql');
	const edited = join(temporary, 'edited.fsql');
	for (
		const [before, after, edit] of [
			['SELECT 1k; SELECT 2;', 'SELECT 2MiB; SELECT 2;', '7 2 2MiB'],
			['SELECT $$old$$; SELECT 2;', 'SELECT $$new text$$; SELECT 2;', '9 3 new text'],
		]
	) {
		writeFileSync(original, before);
		writeFileSync(edited, after);
		const incremental = run('parse', '--no-ranges', original, '--edits', edit);
		const fresh = run('parse', '--no-ranges', edited);
		if (incremental !== fresh) throw new Error(`Incremental parse differs for ${edit}`);
	}
} finally {
	rmSync(temporary, { recursive: true });
}
console.log(`Parsed ${examples.length} .fsql files, validated editor queries and incremental edits.`);
