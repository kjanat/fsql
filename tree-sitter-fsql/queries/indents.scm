[
 (select)
 (cte)
 (case)
 (subquery)
 (insert)
] @indent.begin

(subquery ")" @indent.branch)
(cte ")" @indent.branch)

[
 (keyword_end)
 (keyword_values)
 (keyword_into)
] @indent.branch

(keyword_end) @indent.end
