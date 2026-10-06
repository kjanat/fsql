[(comment) (marginalia)] @comment

((literal) @string
  (#match? @string "^['$]"))

((identifier) @string
  (#match? @string "^[\"`]"))
