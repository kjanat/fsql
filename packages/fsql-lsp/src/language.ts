import type { CompletionItem, Hover, Position } from 'vscode-languageserver';
import type { TextDocument } from 'vscode-languageserver-textdocument';
import { catalog, tables } from '#catalog';

interface Token {
	text: string;
	start: number;
	end: number;
	opaque: boolean;
	closed: boolean;
}

function scanBlockComment(text: string, start: number): Pick<Token, 'end' | 'closed'> {
	let end = start + 2;
	let depth = 1;
	while (end < text.length && depth) {
		if (text.startsWith('/*', end)) {
			depth++;
			end += 2;
		} else if (text.startsWith('*/', end)) {
			depth--;
			end += 2;
		} else end++;
	}
	return { end, closed: depth === 0 };
}

function scanQuotedText(text: string, start: number): Pick<Token, 'end' | 'closed'> {
	const quote = text.charAt(start);
	let end = start + 1;
	while (end < text.length) {
		if (text.charAt(end++) !== quote) continue;
		if (text.charAt(end) === quote) end++;
		else return { end, closed: true };
	}
	return { end, closed: false };
}

// A lexical helper, not a SQL validator. Offsets are UTF-16, as required by LSP.
function tokens(text: string): Token[] {
	const result: Token[] = [];
	let i = 0;
	while (i < text.length) {
		if (/\s/.test(text.charAt(i))) {
			i++;
			continue;
		}
		const start = i;
		let opaque = false;
		let closed = true;
		const dollar = /^\$(?:[A-Za-z_][A-Za-z_0-9]*)?\$/.exec(text.slice(i))?.[0];
		if (text.startsWith('--', i)) {
			opaque = true;
			while (i < text.length && !/[\r\n]/.test(text.charAt(i))) i++;
			closed = false;
		} else if (text.startsWith('/*', i)) {
			opaque = true;
			const comment = scanBlockComment(text, i);
			i = comment.end;
			closed = comment.closed;
		} else if (dollar) {
			opaque = true;
			const end = text.indexOf(dollar, i + dollar.length);
			closed = end !== -1;
			i = closed ? end + dollar.length : text.length;
		} else if (/['"`]/.test(text.charAt(i))) {
			opaque = true;
			const quoted = scanQuotedText(text, i);
			i = quoted.end;
			closed = quoted.closed;
		} else {
			const word = /^[A-Za-z_][A-Za-z_0-9]*/.exec(text.slice(i));
			i += word?.[0].length ?? 1;
		}
		result.push({ text: text.slice(start, i), start, end: i, opaque, closed });
	}
	return result;
}

export function complete(document: TextDocument, position: Position): CompletionItem[] {
	const offset = document.offsetAt(position);
	const all = tokens(document.getText());
	if (all.some(t => t.opaque && offset > t.start && (offset < t.end || (!t.closed && offset === t.end)))) return [];
	const current = all.find(t => !t.opaque && /^[A-Za-z_]/.test(t.text) && t.start <= offset && offset <= t.end);
	const prefix = current ? document.getText().slice(current.start, offset).toLowerCase() : '';
	const before = all.filter(t => !t.opaque && t.end <= (current?.start ?? offset));
	const last = before.at(-1)?.text.toLowerCase();
	const qualifier = before.at(-2)?.text.toLowerCase();
	const columns = qualifier && Object.hasOwn(tables, qualifier) ? tables[qualifier] : undefined;
	let items = catalog;
	if (last && ['from', 'join', 'update', 'into'].includes(last)) {
		items = items.filter(item => Object.hasOwn(tables, item.label));
	} else if (last === '.' && columns) {
		items = items.filter(item => item.detail === 'fsql column' && columns.includes(item.label));
	}
	return items.filter(item => item.label.toLowerCase().startsWith(prefix)).map(item => ({
		...item,
		textEdit: {
			range: { start: document.positionAt(current?.start ?? offset), end: document.positionAt(current?.end ?? offset) },
			newText: item.label,
		},
	}));
}

export function hover(document: TextDocument, position: Position): Hover | null {
	const offset = document.offsetAt(position);
	const token = tokens(document.getText()).find(t => !t.opaque && t.start <= offset && offset < t.end);
	if (!token) return null;
	const entries = catalog.filter(item => item.label.toLowerCase() === token.text.toLowerCase());
	if (!entries.length) return null;
	return {
		contents: {
			kind: 'plaintext',
			value: entries.map(item => `${item.label}: ${item.documentation ?? item.detail}`).join('\n\n'),
		},
		range: { start: document.positionAt(token.start), end: document.positionAt(token.end) },
	};
}
