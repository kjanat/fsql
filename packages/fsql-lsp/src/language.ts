import { catalog, tables } from '#catalog';
import type { CompletionItem, Hover, Position } from 'vscode-languageserver';
import { TextDocument } from 'vscode-languageserver-textdocument';

interface Token {
	text: string;
	start: number;
	end: number;
	opaque: boolean;
	closed: boolean;
}

// A lexical helper, not a SQL validator. Offsets are UTF-16, as required by LSP.
function tokens(text: string): Token[] {
	const result: Token[] = [];
	let i = 0;
	while (i < text.length) {
		if (/\s/.test(text[i]!)) {
			i++;
			continue;
		}
		const start = i;
		let opaque = false;
		let closed = true;
		const dollar = /^\$(?:[A-Za-z_][A-Za-z_0-9]*)?\$/.exec(text.slice(i))?.[0];
		if (text.startsWith('--', i)) {
			opaque = true;
			while (i < text.length && !/[\r\n]/.test(text[i]!)) i++;
			closed = false;
		} else if (text.startsWith('/*', i)) {
			opaque = true;
			i += 2;
			let depth = 1;
			while (i < text.length && depth) {
				if (text.startsWith('/*', i)) {
					depth++;
					i += 2;
				} else if (text.startsWith('*/', i)) {
					depth--;
					i += 2;
				} else i++;
			}
			closed = depth === 0;
		} else if (dollar) {
			opaque = true;
			const end = text.indexOf(dollar, i + dollar.length);
			closed = end !== -1;
			i = closed ? end + dollar.length : text.length;
		} else if (/['"`]/.test(text[i]!)) {
			opaque = true;
			closed = false;
			const quote = text[i++];
			while (i < text.length) {
				if (text[i++] === quote) {
					if (text[i] === quote) i++;
					else {
						closed = true;
						break;
					}
				}
			}
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
	let items = catalog;
	if (last && ['from', 'join', 'update', 'into'].includes(last)) {
		items = items.filter(item => Object.hasOwn(tables, item.label));
	} else if (last === '.' && qualifier && Object.hasOwn(tables, qualifier)) {
		items = items.filter(item => item.detail === 'fsql column' && tables[qualifier]!.includes(item.label));
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
