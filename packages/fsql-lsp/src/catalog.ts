import type { CompletionItem } from 'vscode-languageserver';
import { CompletionItemKind } from 'vscode-languageserver';

// Built-in schema from crates/fsql/src/column.rs; functions from bind.rs/eval.rs.
export const tables: Record<string, string[]> = {
	files:
		'path name parent ext depth hidden kind size blocks mode perms setuid setgid sticky uid gid user group nlink inode dev atime mtime ctime btime target broken'
			.split(' '),
	mounts: 'mountpoint fstype source options readonly dev mnt_id topology transport media case_sensitive remote'.split(
		' ',
	),
	xattrs: 'path name value size'.split(' '),
	acls: 'path kind tag qualifier perms'.split(' '),
};

const functions: Record<string, string> = {
	now: 'now() — current timestamp.',
	current_timestamp: 'current_timestamp() — current timestamp.',
	lower: 'lower(text) — lowercase text.',
	upper: 'upper(text) — uppercase text.',
	length: 'length(value) — character count for text, byte count for blobs.',
	abs: 'abs(number) — absolute value.',
	basename: 'basename(path) — final path component.',
	dirname: 'dirname(path) — parent directory.',
	extension: 'extension(path) — filename extension.',
	human: 'human(bytes) — human-readable file size.',
	format_size: 'format_size(bytes) — alias for human.',
	oct: 'oct(mode) — octal permission bits, masked to 0o7777.',
	typeof: 'typeof(value) — value type name.',
	coalesce: 'coalesce(value, ...) — first non-NULL value.',
	ifnull: 'ifnull(value, fallback) — fallback when value is NULL.',
	nullif: 'nullif(left, right) — NULL when both values are equal.',
	starts_with: 'starts_with(text, prefix) — test a prefix.',
	ends_with: 'ends_with(text, suffix) — test a suffix.',
	contains: 'contains(text, needle) — test a substring.',
	replace: 'replace(text, from, to) — replace occurrences.',
	substr: 'substr(text, start[, length]) — substring.',
	substring: 'substring(text, start[, length]) — alias for substr.',
	trim: 'trim(text[, characters]) — trim both ends.',
	ltrim: 'ltrim(text[, characters]) — trim the start.',
	rtrim: 'rtrim(text[, characters]) — trim the end.',
	count: 'count(value) / count(*) — count non-NULL values / rows.',
	sum: 'sum(value) — sum non-NULL values.',
	min: 'min(value) — minimum non-NULL value.',
	max: 'max(value) — maximum non-NULL value.',
	avg: 'avg(value) — average non-NULL value.',
	group_concat: 'group_concat(value[, separator]) — concatenate a group.',
	string_agg: 'string_agg(value[, separator]) — alias for group_concat.',
	cast: 'CAST(value AS type) — convert a value.',
	extract: 'EXTRACT(unit FROM value) — extract a timestamp component.',
};

const keywords =
	'SELECT FROM WHERE AS DISTINCT ALL JOIN INNER LEFT RIGHT FULL OUTER CROSS ON USING GROUP BY HAVING ORDER ASC DESC NULLS FIRST LAST LIMIT OFFSET UNION EXCEPT INTERSECT WITH RECURSIVE VALUES INSERT INTO UPDATE SET DELETE RETURNING AND OR XOR NOT IS NULL TRUE FALSE IN EXISTS BETWEEN CASE WHEN THEN ELSE END LIKE ILIKE GLOB REGEXP MATCH INTERVAL'
		.split(' ');

export const catalog: CompletionItem[] = [
	...Object.entries(tables).map(([label, columns]) => ({
		label,
		kind: CompletionItemKind.Class,
		detail: 'fsql table',
		documentation: `${label}: ${columns.join(', ')}.`,
	})),
	...Array.from(new Set(Object.values(tables).flat())).map(label => ({
		label,
		kind: CompletionItemKind.Field,
		detail: 'fsql column',
		documentation: `Column of ${
			Object.entries(tables).filter(([, columns]) => columns.includes(label)).map(([table]) => table).join(', ')
		}.`,
	})),
	...Object.entries(functions).map(([label, documentation]) => ({
		label,
		kind: CompletionItemKind.Function,
		detail: documentation,
		documentation,
	})),
	...keywords.map(label => ({ label, kind: CompletionItemKind.Keyword, detail: 'fsql keyword' })),
];
