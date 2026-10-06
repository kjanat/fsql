/**
 * @file Permissive Tree-sitter grammar for fsql query files.
 * @author Kaj Kowalski <info@kajkowalski.nl>
 * @license MIT
 *
 * Adapted from Derek Stride (@DerekStride)'s tree-sitter-sql; see UPSTREAM.md.
 * The Rust planner and evaluator define which queries fsql can execute.
 */
/// <reference types="tree-sitter-cli/dsl" resolution-mode="require" />

import column_list_rules from '#grammar/column-lists';
import expression_rules from '#grammar/expressions';
import { make_keyword, optional_parenthesis } from '#grammar/helpers';
import keyword_rules from '#grammar/keywords';
import statement_rules from '#grammar/statements/index';
import select_rules from '#grammar/statements/select';
import transaction_rules from '#grammar/transactions';
import type_rules from '#grammar/types';

export default grammar({
	name: 'fsql',

	extras: $ => [
		/\s\n/,
		/\s/,
		$.comment,
		$.marginalia,
	],

	externals: $ => [
		$._dollar_quoted_string_start_tag,
		$._dollar_quoted_string_end_tag,
		$._dollar_quoted_string,
	],

	conflicts: $ => [
		[$.object_reference, $._qualified_field],
		[$.field, $._qualified_field],
		[$.object_reference],
		[$.between_expression, $.binary_expression],
	],

	precedences: () => [
		[
			'binary_is',
			'unary_not',
			'binary_exp',
			'binary_times',
			'binary_plus',
			'unary_other',
			'binary_other',
			'binary_in',
			'binary_compare',
			'binary_relation',
			'pattern_matching',
			'between',
			'clause_connective',
			'clause_disjunctive',
		],
	],

	word: $ => $._identifier,

	rules: {
		program: $ =>
			seq(
				repeat(choice(';', seq($.statement, ';'))),
				optional($.statement),
			),

		comment: () => /--.*/,
		// https://stackoverflow.com/questions/13014947/regex-to-match-a-c-style-multiline-comment
		marginalia: () => /\/\*[^*]*\*+(?:[^/*][^*]*\*+)*\//,

		...keyword_rules,
		...type_rules,
		...column_list_rules,
		...expression_rules,
		...transaction_rules,
		...statement_rules,

		// fsql additions. This intentionally remains a permissive SQL grammar;
		// execution support and mutation safety are checked by the Rust planner.
		keyword_glob: () => token(prec(1, make_keyword('glob'))),
		keyword_xor: () => token(prec(1, make_keyword('xor'))),

		// Keep the upstream expression precedence, including the new byte token.
		literal: $ =>
			prec(
				2,
				choice(
					$._integer,
					$._decimal_number,
					$._byte_size,
					$._literal_string,
					$._bit_string,
					$._string_casting,
					$.keyword_true,
					$.keyword_false,
					$.keyword_null,
				),
			),

		// Query files contain reads and mutations; DDL/procedure modules remain
		// vendored but are not reachable from this language's entry point.
		statement: $ => choice($._dml_read, $._dml_write),
		_dml_write: $ =>
			seq(
				optional($._cte),
				choice(
					$._delete_statement,
					$._insert_statement,
					$._update_statement,
				),
			),
		_dml_read: $ =>
			seq(
				optional(optional_parenthesis($._cte)),
				optional_parenthesis(choice($._select_statement, $.set_operation, $.values)),
			),

		from: $ => prec.right(select_rules.from($)),
		_select_statement: $ =>
			optional_parenthesis(seq(
				$.select,
				choice(
					$.from,
					seq(
						optional($.where),
						optional($.group_by),
						optional($.having),
						optional($.order_by),
						optional($.limit),
					),
				),
			)),
		set_operation: $ =>
			seq(
				$._select_statement,
				repeat1(seq(
					field(
						'operation',
						seq(
							choice($.keyword_union, $.keyword_intersect, $.keyword_except),
							optional(choice($.keyword_all, $.keyword_distinct)),
						),
					),
					$._select_statement,
				)),
			),
		_delete_from: $ =>
			seq(
				$.keyword_from,
				optional($.keyword_only),
				$.relation,
				optional($.where),
				optional($.order_by),
				optional($.limit),
			),
		_update_statement: $ => seq($.update, optional($.order_by), optional($.limit), optional($.returning)),
		_insert_values: $ => prec(1, statement_rules._insert_values($)),
		_byte_size: () =>
			seq(
				optional(choice('-', '+')),
				token(
					/([0-9][0-9_]*(\.[0-9_]*)?|\.[0-9][0-9_]*)([eE][+-]?[0-9][0-9_]*)?([bB]|[kKmMgGtTpP]([iI]?[bB])?)/,
				),
			),

		binary_expression: $ =>
			choice(
				expression_rules.binary_expression($),
				...[$.keyword_glob, $.keyword_match].map(operator =>
					prec.left(
						'pattern_matching',
						seq(
							field('left', $._expression),
							field('operator', operator),
							field('right', $._expression),
						),
					)
				),
				prec.left(
					'clause_connective',
					seq(
						field('left', $._expression),
						field('operator', $.keyword_xor),
						field('right', $._expression),
					),
				),
			),

		// Both INTERVAL '7 days' and INTERVAL '7' DAY are useful in query files.
		interval: $ =>
			prec.right(seq(
				$.keyword_interval,
				$._literal_string,
				optional(seq($.interval_unit, optional(seq($.keyword_to, $.interval_unit)))),
			)),
		interval_unit: () =>
			token(prec(
				1,
				choice(...[
					'year',
					'month',
					'week',
					'day',
					'hour',
					'minute',
					'second',
					'millisecond',
					'microsecond',
					'nanosecond',
				].map(make_keyword)),
			)),
	},
});
