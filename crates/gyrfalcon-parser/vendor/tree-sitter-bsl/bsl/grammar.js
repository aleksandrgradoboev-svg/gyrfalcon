/// <reference types='tree-sitter-cli/dsl' />

const PREC = {
  LOGICAL_OR: 10,
  LOGICAL_AND: 11,
  COMPARISON: 13,
  ADDITIVE: 14,
  MULTIPLICATIVE: 15,
  UNARY: 16,
  ACCESS: 17,
  CALL: 18,
  NEW: 19,
  TERNARY: 20,
  ASSIGNMENT: 21,
  AWAIT: 22,
};

const keyword = (...words) => token(choice(...words.map(caseInsensitive)));
const caseInsensitive = (word) => new RegExp(word, 'i');

const CORE_KEYWORDS = [
  // Control flow
  ['если', 'if'],
  ['тогда', 'then'],
  ['иначеесли', 'elsif'],
  ['иначе', 'else'],
  ['конецесли', 'endif'],
  ['для', 'for'],
  ['каждого', 'each'],
  ['из', 'in'],
  ['по', 'to'],
  ['пока', 'while'],
  ['цикл', 'do'],
  ['конеццикла', 'enddo'],
  ['перейти', 'goto'],
  ['возврат', 'return'],
  ['прервать', 'break'],
  ['продолжить', 'continue'],

  // Declarations
  ['процедура', 'procedure'],
  ['функция', 'function'],
  ['конецпроцедуры', 'endprocedure'],
  ['конецфункции', 'endfunction'],
  ['перем', 'var'],
  ['экспорт', 'export'],
  ['знач', 'val'],

  // Values
  ['истина', 'true'],
  ['ложь', 'false'],
  ['неопределено', 'undefined'],

  // Exceptions
  ['попытка', 'try'],
  ['исключение', 'except'],
  ['вызватьисключение', 'raise'],
  ['конецпопытки', 'endtry'],

  // Async/await
  ['асинх', 'async'],
  ['ждать', 'await'],

  // New
  ['новый', 'new'],

  // Handlers
  ['добавитьобработчик', 'addhandler'],
  ['удалитьобработчик', 'removehandler'],

  // Operators
  ['и', 'and'],
  ['или', 'or'],
  ['не', 'not'],
];

const PREPROC_KEYWORDS = [
  ['если', 'if'],
  ['иначеесли', 'elsif'],
  ['иначе', 'else'],
  ['конецесли', 'endif'],
  ['область', 'region'],
  ['конецобласти', 'endregion'],
];

/**
 * Формирует правила для ключевых слов
 */
function buildKeywords() {
  const kw = {};
  for (const [rus, eng] of CORE_KEYWORDS) {
    kw[`${eng.toUpperCase()}_KEYWORD`] = ($) => keyword(rus, eng);
  }

  for (const [rus, eng] of PREPROC_KEYWORDS) {
    // The 1C platform tolerates horizontal whitespace between `#` and the
    // directive word (`# Область Имя` compiles and ships in live УНФ code);
    // `caseInsensitive` builds a RegExp, so the class is embedded directly.
    kw[`PREPROC_${eng.toUpperCase()}_KEYWORD`] = ($) =>
      keyword('#[ \\t]*' + rus, '#[ \\t]*' + eng);
  }

  kw['NULL_KEYWORD'] = ($) => token(/null/i);
  return kw;
}

/**
 * Формирует список резервируемых слов
 *
 * @param {*} $ grammar object
 */
function reservedKeywords($) {
  return Object.keys(buildKeywords()).map((k) => $[k]);
}

/**
 * Формирует список ключевых слов, допустимых как имена членов после доступа
 *
 * @param {*} $ grammar object
 */
function memberNameKeywords($) {
  return [
    ...CORE_KEYWORDS.map(([, eng]) => $[`${eng.toUpperCase()}_KEYWORD`]),
    $.NULL_KEYWORD,
  ];
}

const Preprocessor = {
  preprocessor: ($) => {
    const region = seq(
      $.PREPROC_REGION_KEYWORD,
      field('name', $.identifier),
      repeat($._definition),
      $.PREPROC_ENDREGION_KEYWORD,
    );

    const preproc_if = seq(
      $.PREPROC_IF_KEYWORD,
      $.expression,
      $.THEN_KEYWORD,
      repeat($._definition),
      repeat(
        seq(
          $.PREPROC_ELSIF_KEYWORD,
          $.expression,
          $.THEN_KEYWORD,
          repeat($._definition),
        ),
      ),
      optional(seq($.PREPROC_ELSE_KEYWORD, repeat($._definition))),
      $.PREPROC_ENDIF_KEYWORD,
    );

    const preproc_change = [
      'Вставка',
      'Insert',
      'КонецВставки',
      'EndInsert',
      'Удаление',
      'Delete',
      'КонецУдаления',
      'EndDelete',
    ].map((annotation) =>
      // Same whitespace tolerance after `#` as PREPROC_KEYWORDS above.
      alias(token(caseInsensitive('#[ \\t]*' + annotation)), $.preproc),
    );

    const annotations = [
      'Перед',
      'Before',
      'После',
      'After',
      'Вместо',
      'Around',
      'ИзменениеИКонтроль',
      'ChangeAndValidate',
    ].map((annotation) =>
      seq(
        alias(token(caseInsensitive('&' + annotation)), $.annotation),
        '(',
        $.string,
        ')',
      ),
    );
    // Some 1C platform versions emit `&НаСервере()` / `&НаСервереБезКонтекста()`
    // with an empty argument list; these appear in live УНФ code. Accept the
    // optional `()` after the directive token so the parser does not ERROR on it.
    const compilation_directives = [
      'НаКлиенте',
      'AtClient',
      'НаСервере',
      'AtServer',
      'НаСервереБезКонтекста',
      'AtServerNoContext',
      'НаКлиентеНаСервереБезКонтекста',
      'AtClientAtServerNoContext',
      'НаКлиентеНаСервере',
      'AtClientAtServer',
    ].map((annotation) =>
      seq(
        alias(token(caseInsensitive('&' + annotation)), $.annotation),
        optional(seq('(', ')')),
      ),
    );
    return choice(
      region,
      preproc_if,
      ...preproc_change,
      ...annotations,
      ...compilation_directives,
    );
  },
};

module.exports = grammar({
  name: 'bsl',

  // U+00A0 (NON-BREAKING SPACE) appears as indentation in live УНФ code edited
  // by 1C platform tools; tree-sitter's regex engine does NOT treat it as
  // whitespace even though JS /\s/ matches it — add it explicitly so the parser
  // skips NBSP rather than emitting ERROR nodes on every NBSP-indented line.
  // Прочие пробельные символы Юникода: 1С принимает их молча, и они попадают
  // в код копипастом из редакторов и мессенджеров. В выгрузке ЗУП + СППР найдены
  // NBSP (551 вхождение) и EM SPACE (36). Остальные перечислены на будущее —
  // источник у них тот же, и ловить их по одному дороже, чем закрыть класс целиком.
  extras: ($) => [/[\s\u00A0\u2000-\u200B\u202F\u205F\u3000\uFEFF]/, $.line_comment],

  supertypes: ($) => [],

  inline: ($) => [],

  conflicts: ($) => [
    [$._plain_variable_spec, $._exported_variable_spec],
  ],

  word: ($) => $.identifier,

  reserved: {
    global: ($) => reservedKeywords($),
  },
  rules: {
    source_file: ($) => repeat($._definition),

    _definition: ($) =>
      choice(
        $.procedure_definition,
        $.function_definition,
        $.var_definition,
        $._statement,
      ),

    procedure_definition: ($) =>
      seq(
        optional($.ASYNC_KEYWORD),
        $.PROCEDURE_KEYWORD,
        field('name', $.identifier),
        field('parameters', $.parameters),
        optional(field('export', $.EXPORT_KEYWORD)),
        repeat($._statement),
        $.ENDPROCEDURE_KEYWORD,
      ),

    function_definition: ($) =>
      seq(
        optional($.ASYNC_KEYWORD),
        $.FUNCTION_KEYWORD,
        field('name', $.identifier),
        field('parameters', $.parameters),
        optional(field('export', $.EXPORT_KEYWORD)),
        repeat($._statement),
        $.ENDFUNCTION_KEYWORD,
      ),

    var_definition: ($) =>
      prec.right(
        1,
        seq(
          $.VAR_KEYWORD,
          $._var_definition_variables,
          optional(field('export', $.EXPORT_KEYWORD)),
          optional(';'),
        ),
      ),
    _var_definition_variables: ($) =>
      seq(
        repeat(
          choice(
            seq(
              field('variable', alias($._exported_variable_spec, $.variable_spec)),
              ',',
            ),
            seq(
              field('variable', alias($._plain_variable_spec, $.variable_spec)),
              ',',
            ),
          ),
        ),
        field('variable', alias($._plain_variable_spec, $.variable_spec)),
      ),
    _plain_variable_spec: ($) =>
      prec(2, seq(field('name', $.identifier))),
    _exported_variable_spec: ($) =>
      prec(
        2,
        seq(
          field('name', $.identifier),
          field('export', $.EXPORT_KEYWORD),
        ),
      ),
    parameters: ($) => seq('(', commaSep(field('parameter', $.parameter)), ')'),

    parameter: ($) =>
      seq(
        field('val', optional($.VAL_KEYWORD)),
        field('name', $.identifier),
        // Default values may carry a leading unary sign — `Параметр = -1` is
        // pervasive in real code. `_const_value` alone (a bare literal) rejected
        // the sign, erroring the whole parameter list.
        optional(
          seq(
            '=',
            field('def', choice($._const_value, seq(choice('-', '+'), $._const_value))),
          ),
        ),
      ),

    // Statements
    _statement: ($) =>
      choice(
        $._empty_statement,
        $.execute_statement,
        $.call_statement,
        $.assignment_statement,
        $.return_statement,
        $.try_statement,
        $.rise_error_statement,
        $.var_statement,
        $.if_statement,
        $.while_statement,
        $.for_statement,
        $.for_each_statement,
        $.continue_statement,
        $.break_statement,
        $.goto_statement,
        $.label_statement,
        $.add_handler_statement,
        $.remove_handler_statement,
        $.preprocessor,
        $.await_statement,
      ),

    _empty_statement: ($) => prec(-1, ';'),

    call_statement: ($) =>
      prec.right(seq(choice($.method_call, $.call_expression), optional(';'))),

    assignment_statement: ($) =>
      prec.right(seq(
        field('left', $._assignment_member),
        '=',
        field('right', $.expression),
        optional(';'),
      )),

    return_statement: ($) =>
      prec.right(seq($.RETURN_KEYWORD, field('result', optional($.expression)), optional(';'))),

    try_statement: ($) =>
      prec.right(seq(
        $.TRY_KEYWORD,
        repeat($._statement),
        $.EXCEPT_KEYWORD,
        repeat($._exception_statement),
        $.ENDTRY_KEYWORD,
        optional(';'),
      )),

    // The Исключение-block body is just a normal statement list now that
    // `rise_error_statement` accepts a bare (expression-less) re-raise at any
    // position — the former `_rise_error_rethrow_statement` special case is
    // subsumed and removed (it conflicted with the optional-expression form).
    _exception_statement: ($) => $._statement,

    rise_error_statement: ($) =>
      // The raised expression is OPTIONAL: a bare `ВызватьИсключение;` re-raises
      // the current exception and is valid anywhere inside a Попытка…Исключение
      // block — including nested statements (`Если … ВызватьИсключение;`), which
      // `_exception_statement`'s top-level-only rethrow rule did not reach,
      // erroring with a MISSING expression.
      prec.right(seq($.RAISE_KEYWORD, optional(choice(prec(1, $.arguments), $.expression)), optional(';'))),

    var_statement: ($) =>
      prec.right(seq(
        $.VAR_KEYWORD,
        sepBy1(',', field('var_name', $.identifier)),
        optional(';'),
      )),

    if_statement: ($) =>
      prec.right(seq(
        $.IF_KEYWORD,
        $.expression,
        $.THEN_KEYWORD,
        repeat($._statement),
        repeat($.elseif_clause),
        optional($.else_clause),
        $.ENDIF_KEYWORD,
        optional(';'),
      )),

    elseif_clause: ($) =>
      seq($.ELSIF_KEYWORD, $.expression, $.THEN_KEYWORD, repeat($._statement)),

    else_clause: ($) => seq($.ELSE_KEYWORD, repeat($._statement)),

    while_statement: ($) =>
      seq(
        $.WHILE_KEYWORD,
        $.expression,
        $.DO_KEYWORD,
        repeat($._statement),
        $.ENDDO_KEYWORD,
      ),

    for_statement: ($) =>
      prec.right(seq(
        $.FOR_KEYWORD,
        $.identifier,
        '=',
        $.expression,
        $.TO_KEYWORD,
        $.expression,
        $.DO_KEYWORD,
        repeat($._statement),
        $.ENDDO_KEYWORD,
        optional(';'),
      )),

    for_each_statement: ($) =>
      prec.right(seq(
        $.FOR_KEYWORD,
        $.EACH_KEYWORD,
        $.identifier,
        $.IN_KEYWORD,
        $.expression,
        $.DO_KEYWORD,
        repeat($._statement),
        $.ENDDO_KEYWORD,
        optional(';'),
      )),

    continue_statement: ($) => prec.right(seq($.CONTINUE_KEYWORD, optional(';'))),

    break_statement: ($) => prec.right(seq($.BREAK_KEYWORD, optional(';'))),

    execute_statement: ($) => choice(
      prec.right(seq(keyword('выполнить', 'execute'), $.expression, optional(';'))),
      prec.right(1, seq(keyword('выполнить', 'execute'), '(', $.expression, ')', optional(';'))),
    ),

    goto_statement: ($) =>
      prec.right(seq($.GOTO_KEYWORD, '~', $.identifier, optional(';'))),

    label_statement: ($) => prec.right(seq('~', $.identifier, ':', optional(';'))),

    add_handler_statement: ($) =>
      prec.right(seq($.ADDHANDLER_KEYWORD, $.expression, ',', $.expression, optional(';'))),

    remove_handler_statement: ($) =>
      prec.right(seq(
        $.REMOVEHANDLER_KEYWORD,
        $.expression,
        ',',
        $.expression,
        optional(';'),
      )),
    await_statement: ($) => prec.right(seq($.await_expression, optional(';'))),

    // Expressions
    expression: ($) =>
      choice(
        alias($._const_value, $.const_expression),
        $.string_concat_expression,
        $.identifier,
        $.parenthesized_expression,
        $.unary_expression,
        $.binary_expression,
        $.ternary_expression,
        $.new_expression,
        $.new_expression_method,
        $.method_call,
        $.call_expression,
        $.property_access,
        $.await_expression,
      ),

    // Implicit string concatenation: `"part1" "part2"` — two or more adjacent
    // string literals without an explicit operator, common in formatted НСтр()
    // calls and long string assignments split across lines in 1C platform code.
    // Precedence just above ADDITIVE (+) so `"a" + "b" "c"` is not ambiguous.
    string_concat_expression: ($) =>
      prec.left(
        PREC.ADDITIVE + 1,
        seq(
          choice($.string, alias($.multiline_string, $.string)),
          repeat1(choice($.string, alias($.multiline_string, $.string))),
        ),
      ),

    unary_expression: ($) =>
      prec.left(
        PREC.UNARY,
        seq(
          field('operator', alias(choice('-', '+', $.NOT_KEYWORD), $.operator)),
          field('argument', $.expression),
        ),
      ),

    parenthesized_expression: ($) => seq('(', $.expression, ')'),

    binary_expression: ($) => {
      const operations = [
        [PREC.LOGICAL_AND, $.AND_KEYWORD],
        [PREC.LOGICAL_OR, $.OR_KEYWORD],
        [PREC.COMPARISON, choice('<>', '=', '>', '<', '>=', '<=')],
        [PREC.ADDITIVE, choice('+', '-')],
        [PREC.MULTIPLICATIVE, choice('*', '/', '%')],
      ];

      return choice(
        ...operations.map(([priority, operator]) => {
          return prec.left(
            priority,
            seq(
              field('left', $.expression),
              field('operator', alias(operator, $.operator)),
              field('right', $.expression),
            ),
          );
        }),
      );
    },

    ternary_expression: ($) =>
      prec.right(
        seq(
          // `?` and `(` are SEPARATE tokens, not a glued `'?('` literal: the
          // platform accepts horizontal whitespace between them (`? (`, `=? (`),
          // which is widespread in live УНФ code. A single `'?('` token only
          // matched the no-space form and errored on the rest.
          '?',
          '(',
          field('condition', $.expression),
          ',',
          field('consequence', $.expression),
          ',',
          field('alternative', $.expression),
          ')',
        ),
      ),

    new_expression: ($) =>
      prec(
        PREC.NEW,
        seq(
          $.NEW_KEYWORD,
          field('type', $.identifier),
          field('arguments', optional($.arguments)),
        )),
    new_expression_method: ($) =>
      prec.right(
        PREC.NEW,
        seq(
          $.NEW_KEYWORD,
          '(',
          field('type', $.expression),
          choice(seq(',', field('arguments', $.expression), ')'), ')'),
        )),

    call_expression: ($) => prec(PREC.CALL - 1, $._access_call),

    await_expression: ($) =>
      prec(PREC.AWAIT, seq($.AWAIT_KEYWORD, $.expression)),

    _assignment_member: ($) => choice($.identifier, $.property_access),

    property_access: ($) =>
      prec(PREC.ACCESS, choice($._access_property, $._access_index)),

    access: ($) =>
      prec(
        1,
        choice(
          $._access_call,
          $._access_index,
          $._access_property,
          $.identifier,
          $.method_call,
          // A ternary expression can head an access chain: `?(Усл, А, Б).Метод()`
          // is a valid receiver in BSL and common in real code. Without this the
          // closing `)` had nothing to attach the trailing `.Метод()` to and
          // errored. (Ternary leads with the unique `?(` token, so it adds no
          // ambiguity. A bare parenthesized head `(Выражение).Метод()` is also
          // valid BSL but collides with new_expression's `(` argument list — it
          // needs a precedence/conflict pass and is deferred.)
          $.ternary_expression,
        ),
      ),
    _access_call: ($) => choice(
      seq($.access, '.', alias($._access_method_call, $.method_call)),
      seq(choice($._access_index, $._access_call), $.arguments),
    ),
    _access_index: ($) => seq($.access, '[', alias($.expression, $.index), ']'),
    _access_property: ($) =>
      seq(
        $.access,
        '.',
        choice(
          alias($.identifier, $.property),
          alias($._member_keyword, $.property),
        ),
      ),

    method_call: ($) =>
      prec(
        PREC.CALL,
        seq(field('name', $.identifier), field('arguments', $.arguments)),
      ),

    _access_method_call: ($) =>
      prec(
        PREC.CALL,
        seq(
          field('name', choice($.identifier, alias($._member_keyword, $.identifier))),
          field('arguments', $.arguments),
        ),
      ),

    _member_keyword: ($) => choice(...memberNameKeywords($)),

    arguments: ($) =>
      prec(
        1,
        seq(
          '(',
          optional($._argument_list),
          ')',
        ),
      ),
    _argument_list: ($) =>
      prec.right(
        1,
        choice(
          $.expression,
          seq($.expression, ',', $._argument_list),
          seq($.expression, alias(',', $.omitted_argument)),
          seq(alias(',', $.omitted_argument), optional($._argument_list)),
        ),
      ),

    // Primitive
    ...buildKeywords(),
    ...Preprocessor,

    _const_value: ($) =>
      choice(
        $.number,
        $.date,
        $.string,
        alias($.multiline_string, $.string),
        $.boolean,
        $.UNDEFINED_KEYWORD,
        $.NULL_KEYWORD,
      ),

    boolean: ($) => choice($.TRUE_KEYWORD, $.FALSE_KEYWORD),
    null: ($) => $.NULL_KEYWORD,

    number: ($) => /\d+(\.\d+)?/,
    date: ($) =>
      /'\d{4}[^0-9'\r\n]*\d{2}[^0-9'\r\n]*\d{2}([^0-9'\r\n]*\d{2}[^0-9'\r\n]*\d{2}([^0-9'\r\n]*\d{2})?)?'/,
    string: ($) =>
      seq(
        '"',
        alias(token.immediate(prec(1, /([^\r\n"]|"")*/)), $.string_content),
        '"',
      ),
    multiline_string: ($) =>
      seq(
        '"',
        alias(token.immediate(prec(1, /([^\r\n"]|"")*/)), $.string_content),
        repeat1(
          seq(
            '|',
            alias(token.immediate(prec(1, /([^\r\n"]|"")*/)), $.string_content),
          ),
        ),
        '"',
      ),
    // `ё`/`Ё` (U+0451 / U+0401) sit OUTSIDE the `а-я` block (U+0430..U+044F) and
    // are not covered by ASCII `\w`, so they must be listed explicitly — without
    // this, any identifier containing `ё` (Приёмник, Объём, Учёт, СчётФактура, …)
    // produces an ERROR node, which breaks downstream name extraction and aborts
    // the whole-module stack-graph build. The `/i` flag folds `ё`↔`Ё`.
    identifier: ($) => /[\wа-яёЁ_][\wа-яёЁ_0-9]*/i,

    line_comment: ($) => seq('//', /.*/),
  },
});

/**
 * Creates a rule to optionally match one or more of the rules separated by a comma
 *
 * @param {RuleOrLiteral} rule
 */
function commaSep(rule) {
  return sepBy(',', rule);
}

/**
 * Creates a rule to optionally match one or more of the rules separated by a separator
 *
 * @param {RuleOrLiteral} sep
 *
 * @param {RuleOrLiteral} rule
 */
function sepBy(sep, rule) {
  return optional(sepBy1(sep, rule));
}

/**
 * Creates a rule to match one or more of the rules separated by a separator
 *
 * @param {RuleOrLiteral} sep
 *
 * @param {RuleOrLiteral} rule
 */
function sepBy1(sep, rule) {
  return seq(rule, repeat(seq(sep, rule)));
}
