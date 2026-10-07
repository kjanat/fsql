/** @param {string} word */
export function make_keyword(word) {
  let pattern = '';
  for (let i = 0; i < word.length; i++) {
    const character = word.charAt(i);
    pattern += `[${character.toLowerCase()}${character.toUpperCase()}]`;
  }
  return new RegExp(pattern);
}

/** @param {RuleOrLiteral} node */
export function optional_parenthesis(node) {
  return prec.right(choice(node, wrapped_in_parenthesis(node)));
}

/** @param {RuleOrLiteral} [node] */
export function wrapped_in_parenthesis(node) {
  return node ? seq('(', node, ')') : seq('(', ')');
}

/**
 * Creates a comma-separated list of nodes.
 * @param {RuleOrLiteral} field - The field to repeat.
 * @param {boolean} [requireFirst=false] - Whether the first field is required.
 * @returns {ChoiceRule | SeqRule} The comma-separated list.
 */
export function comma_list(field, requireFirst = false) {
  const sequence = seq(field, repeat(seq(',', field)));

  return requireFirst ? sequence : optional(sequence);
}

/**
 * @param {RuleOrLiteral} field
 * @param {boolean} [requireFirst=false]
 */
export function paren_list(field, requireFirst = false) {
  return wrapped_in_parenthesis(comma_list(field, requireFirst));
}
