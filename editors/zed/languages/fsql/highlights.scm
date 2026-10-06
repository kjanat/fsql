; Adapted from tree-sitter-fsql/queries/highlights.scm; see ../../THIRD_PARTY_NOTICES.md.
(object_reference
  name: (identifier) @type)

(invocation
  (object_reference
    name: (identifier) @function))

[
  (keyword_hash)
  (keyword_array)
  (keyword_object_id)
] @function

(relation
  alias: (identifier) @variable)

(field
  name: (identifier) @property)

(term
  alias: (identifier) @variable)

((term
   value: (cast
    name: (keyword_cast) @function
    parameter: [(literal)]?)))

(literal) @string
(comment) @comment
(marginalia) @comment

((literal) @number
  (#match? @number "^[-+]?(0[xX][0-9a-fA-F_]+|0[oO][0-7_]+|0[bB][01_]+|([0-9][0-9_]*(\\.[0-9_]*)?|\\.[0-9][0-9_]*)([eE][+-]?[0-9][0-9_]*)?([bB]|[kKmMgGtTpP]([iI]?[bB])?)?)$"))

[
  (keyword_glob)
  (keyword_xor)
] @keyword

(interval_unit) @keyword

(parameter) @variable.parameter

[
 (keyword_true)
 (keyword_false)
] @boolean

[
 (keyword_asc)
 (keyword_desc)
 (keyword_unsigned)
 (keyword_nulls)
 (keyword_last)
 (keyword_auto_increment)
 (keyword_default)
 (keyword_always)
 (keyword_generated)
 (keyword_preceding)
 (keyword_following)
 (keyword_first)
 (keyword_current_timestamp)
] @attribute

[
 (keyword_materialized)
 (keyword_recursive)
] @keyword

[
 (keyword_case)
 (keyword_when)
 (keyword_then)
 (keyword_else)
] @keyword

[
  (keyword_select)
  (keyword_from)
  (keyword_where)
  (keyword_index)
  (keyword_join)
  (keyword_primary)
  (keyword_delete)
  (keyword_insert)
  (keyword_distinct)
  (keyword_replace)
  (keyword_update)
  (keyword_into)
  (keyword_overwrite)
  (keyword_values)
  (keyword_set)
  (keyword_left)
  (keyword_right)
  (keyword_outer)
  (keyword_inner)
  (keyword_full)
  (keyword_order)
  (keyword_partition)
  (keyword_group)
  (keyword_with)
  (keyword_without)
  (keyword_as)
  (keyword_having)
  (keyword_limit)
  (keyword_offset)
  (keyword_key)
  (keyword_references)
  (keyword_foreign)
  (keyword_constraint)
  (keyword_force)
  (keyword_use)
  (keyword_for)
  (keyword_if)
  (keyword_exists)
  (keyword_cross)
  (keyword_lateral)
  (keyword_natural)
  (keyword_end)
  (keyword_is)
  (keyword_using)
  (keyword_between)
  (keyword_window)
  (keyword_no)
  (keyword_to)
  (keyword_all)
  (keyword_any)
  (keyword_some)
  (keyword_returning)
  (keyword_only)
  (keyword_like)
  (keyword_rlike)
  (keyword_similar)
  (keyword_over)
  (keyword_range)
  (keyword_rows)
  (keyword_groups)
  (keyword_exclude)
  (keyword_current)
  (keyword_ties)
  (keyword_others)
  (keyword_zerofill)
  (keyword_row)
  (keyword_comment)
  (keyword_stored)
  (keyword_virtual)
  (keyword_partitioned)
  (keyword_conflict)
  (keyword_filter)
  (keyword_name)
  (keyword_oid)
  (keyword_precision)
  (keyword_regclass)
  (keyword_regnamespace)
  (keyword_regproc)
  (keyword_regtype)
  (keyword_separator)
  (keyword_action)
  (keyword_ordinality)
  (keyword_zone)
  (keyword_match)
  (keyword_duplicate)
] @keyword

[
 (keyword_restrict)
 (keyword_unbounded)
 (keyword_unique)
 (keyword_cascade)
 (keyword_delayed)
 (keyword_high_priority)
 (keyword_low_priority)
 (keyword_ignore)
 (keyword_nothing)
 (keyword_check)
] @keyword

[
  (keyword_int)
  (keyword_null)
  (keyword_boolean)
  (keyword_binary)
  (keyword_varbinary)
  (keyword_image)
  (keyword_bit)
  (keyword_inet)
  (keyword_smallserial)
  (keyword_serial)
  (keyword_bigserial)
  (keyword_smallint)
  (keyword_mediumint)
  (keyword_bigint)
  (keyword_tinyint)
  (keyword_decimal)
  (keyword_float)
  (keyword_double)
  (keyword_numeric)
  (keyword_real)
  (double)
  (keyword_money)
  (keyword_smallmoney)
  (keyword_char)
  (keyword_nchar)
  (keyword_varchar)
  (keyword_nvarchar)
  (keyword_varying)
  (keyword_text)
  (keyword_string)
  (keyword_uuid)
  (keyword_json)
  (keyword_jsonb)
  (keyword_xml)
  (keyword_bytea)
  (keyword_enum)
  (keyword_date)
  (keyword_datetime)
  (keyword_time)
  (keyword_datetime2)
  (keyword_datetimeoffset)
  (keyword_smalldatetime)
  (keyword_timestamp)
  (keyword_timestamptz)
  (keyword_geometry)
  (keyword_geography)
  (keyword_box2d)
  (keyword_box3d)
  (keyword_interval)
] @type.builtin

[
  (keyword_in)
  (keyword_and)
  (keyword_or)
  (keyword_not)
  (keyword_by)
  (keyword_on)
  (keyword_do)
  (keyword_union)
  (keyword_except)
  (keyword_intersect)
] @keyword

[
  "+"
  "-"
  "*"
  "/"
  "%"
  "^"
  "="
  "<"
  "<="
  "!="
  ">="
  ">"
  "<>"
  (op_other)
  (op_unary_other)
] @operator

[
  "("
  ")"
] @punctuation.bracket

[
  ";"
  ","
  "."
] @punctuation.delimiter
