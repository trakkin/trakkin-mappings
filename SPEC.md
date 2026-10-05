# Trakkin Mapping DSL

**Status:** Draft
**Version:** 0.0
**Canonical source:** this repository
**Grammar:** [`grammar/Trakkin.g4`](grammar/Trakkin.g4)

## 1. Scope

The Trakkin Mapping DSL describes how media entities and synchronization units correspond across independent sources. Its purpose is to determine when state associated with media in one source can be safely applied to media in another.

The DSL does not define a universal media ontology. Each source retains its own identifiers, hierarchy, numbering, ordering, editions, and other source-specific concepts.

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are to be interpreted as described in [BCP 14](https://www.rfc-editor.org/info/bcp14/).

## 2. Grammar and conformance

The syntax grammar is defined with [ANTLR4](https://www.antlr.org/) in [`grammar/Trakkin.g4`](grammar/Trakkin.g4).

The grammar defines syntax only. Adapter capabilities, ordering, cardinality, recursive correspondence, and other semantic constraints are defined by this specification.

A conforming implementation:

1. accepts syntactically valid input described by the grammar;
2. rejects the semantic errors required by this specification; and
3. emits the canonical form defined in §10.

## 3. Lexical rules

### 3.1 Spacing

Mapping operators and the `::` selector separator may be surrounded by one or more ASCII spaces. Tabs are not valid syntax whitespace.

Canonical form emits exactly one ASCII space in those positions.

Whitespace is significant when distinguishing a selector from an opaque
reference. Therefore:

```text
example://foo::bar
```

is one opaque reference, while:

```text
example://foo :: episode=1
```

contains a selector.

### 3.2 Source references

A reference has the form:

```text
<source>://<opaque-value>
```

The opaque value belongs to the source adapter. The core language MUST NOT interpret its internal structure.

The grammar reserves ASCII whitespace, `,`, `<`, `>`, `[`, and `]` as reference boundaries. A source adapter whose native identifier contains one of those characters MUST expose an encoded opaque value that does not contain the reserved character. Percent-encoding is RECOMMENDED where it is appropriate for the source.

Reverse-domain source names are RECOMMENDED to reduce namespace collisions.

Examples:

```text
com.thetvdb://series/123
org.themoviedb://tv/456
com.imdb://title/tt1234567
co.anilist://media/5081
net.myanimelist://anime/5114
net.anidb://anime/6107
```

### 3.3 Scalars

Unquoted scalars use a restricted character set: letters, digits, `_`, `-`, `:`, `+`, `%`, and `@`, with single `.` characters allowed as separators. Two consecutive dots are reserved for ranges.

The extent-shaped form `@N`, where `N` is a positive integer, is reserved. A scalar with that exact shape MUST be quoted.

Values requiring other characters MUST be quoted.

Quoted scalars support these escapes:

```text
\"    quotation mark
\\    backslash
\n    line feed
\r    carriage return
\t    tab
```

## 4. Selectors

A selector is a source-specific query scoped to a reference:

```text
com.thetvdb://series/123 :: episode=2,order=aired,season=1
```

Selector dimensions are source-specific, not built-in media concepts. An adapter MUST reject unsupported dimensions.

### 4.1 Predicates

A selector contains one or more conjunctive predicates:

```text
episode=2,order=aired,season=1
```

Predicate order has no semantic significance. Duplicate predicates for the same dimension MUST be rejected.

### 4.2 Scalar

```text
episode=3
order=absolute
edition="director-cut"
```

A scalar selects one adapter-defined value.

### 4.3 Ordered range

```text
episode=1..12
episode=13..
episode=..12
```

Ranges are inclusive and ordered. An open bound is valid only when the adapter can resolve it. The adapter defines the deterministic order.

### 4.4 Unordered set

```text
episode={1,4,7}
```

Sets are unordered, so `{1,2,3}` and `{3,1,2}` have identical semantics. An unordered set MUST NOT be implicitly zipped by position.

### 4.5 Wildcard

```text
episode=*
season=1,episode=*
```

A wildcard selects every adapter-defined value that satisfies the other predicates.

### 4.6 Recursive hierarchy

```text
com.thetvdb://series/123 :: **
```

`**` selects the complete descendant hierarchy rooted at the reference. It is distinct from a wildcard predicate.

A bare reference addresses only the source entity itself. It MUST NOT implicitly select descendants.

## 5. Expressions and composites

An expression is either a selection or an ordered composite.

```text
[a.example://part/1,b.example://part/2]
```

Order is significant. Selections may appear inside composites:

```text
[co.anilist://media/100 :: episode=1..12,co.anilist://media/101 :: episode=1..12]
```

### 5.1 Relative extent

A selection MAY declare a relative structural extent with `@N`, where `N` is a positive integer:

```text
a.example://episode/1 @2
b.example://show/2 :: episode=1..12 @3
```

An omitted extent means `@1`. Extent applies independently to every synchronization unit resolved from that selection. It MAY appear only on a selection, not on a composite as a whole. A composite's total extent is the sum of its resolved child extents.

Extent is a dimensionless relative measure. Multiplying every extent in a statement by the same positive factor does not change the statement's meaning.

Ordered expressions align by cumulative extent. Unordered sets express collective coverage but do not establish positional boundaries.

## 6. Mapping operators

The language defines three mapping operators.

### 6.1 Exact synchronization equivalence: `<=>`

```text
a.example://episode/1 <=> b.example://episode/4
```

`<=>` states that both sides are directly interchangeable synchronization units.

When both sides are ordered multi-item selections, items correspond by position:

```text
a.example://show/1 :: episode=1..3 <=> b.example://show/2 :: episode=4..6
```

This form is valid only when both sides have deterministic ordering and equal cardinality.

Corresponding units MUST also have equal extent.

### 6.2 Collective coverage equivalence: `<~>`

```text
a.example://show/1 :: episode=1 @2 <~> b.example://show/2 :: episode={1,2}
```

`<~>` states that both sides cover the same media collectively, without asserting pairwise identity. It is used for 1:N, N:1, and N:M relationships.

When both expressions are closed and finite, their total extents MUST be equal. Wildcards, open ranges, and recursive selectors align only the currently shared weighted extent; unmatched tails remain unmapped until source hierarchy data changes.

### 6.3 Directional state implication: `=>`

```text
a.example://edition/extended => b.example://edition/theatrical
```

`=>` states that state on the left safely implies corresponding state on the right. The operator expresses semantic asymmetry, not a user's configured synchronization direction.

It MAY operate positionally over ordered selections of equal cardinality.

It MAY also align unequal cardinalities by cumulative extent when both expressions are ordered. When both expressions are closed and finite, their total extents MUST be equal.

### 6.4 Non-associativity

Mapping operators are non-associative. For example, the following is invalid:

```text
a.example://x <=> b.example://y <=> c.example://z
```

Write separate statements instead.

## 7. Cardinality

Cardinality is determined by expressions; the language has no cardinality-specific operators.

```text
# 1:1
a.example://show/1 :: episode=1 <=> b.example://show/2 :: episode=3

# N:N positional
a.example://show/1 :: episode=1..12 <=> b.example://show/2 :: episode=13..24

# 1:N with equal total extent
a.example://show/1 :: episode=1 @2 <~> b.example://show/2 :: episode={1,2}

# N:M with unequal unit sizes
a.example://show/1 :: episode={1,2,3} @2 <~> b.example://show/2 :: episode={4,5} @3
```

An implementation MUST reject ambiguous positional correspondence instead of inferring one.

## 8. Source-pair constraint

Each logical side of a mapping SHOULD contain exactly one source namespace.

```text
com.thetvdb://series/123 :: order=aired,season=2 <~> [co.anilist://media/100,co.anilist://media/101]
```

Relationships that involve more than two source namespaces SHOULD be written as separate pairwise mappings.

## 9. Comments and annotations

Human comments begin with `#` followed by either end-of-line or a space:

```text
# The broadcast premiere combines two separately listed episodes.
```

Machine-readable annotations begin with `#@`:

```text
#@source https://example.org/official-episode-guide
#@reason split-episode
a.example://show/1 :: episode=1 <~> b.example://show/2 :: episode={1,2}
```

Comments and annotations immediately preceding a statement belong to that statement. Human comments have no machine-readable semantics.

The initial annotation vocabulary is:

```text
#@source <opaque-value>
#@reason <identifier>
#@note <text>
```

`source` MAY be repeated. Suggested `reason` values include `numbering-offset`, `season-boundary`, `split-episode`, `merged-episode`, `alternate-order`, `source-error`, and `manual-verification`.

Mapping files SHOULD NOT duplicate repository-derived provenance such as contributor identity, timestamps, commit hashes, or pull-request numbers.

## 10. Canonical form

Canonical serialization MUST be deterministic.

A canonical mapping statement:

- occupies one physical line;
- uses exactly one ASCII space around a mapping operator;
- uses exactly one ASCII space around `::`;
- contains no unnecessary spaces inside selectors or composites;
- sorts unordered set values deterministically; and
- divides all statement extents by their greatest common divisor;
- omits `@1`; and
- formats equivalent expressions identically.

For example:

```text
episode={3,1,2}
```

canonicalizes to:

```text
episode={1,2,3}
```

Similarly:

```text
a.example://x @2 <~> b.example://y @4
```

canonicalizes to:

```text
a.example://x <~> b.example://y @2
```

Canonical mapping text SHOULD be suitable for hashing, deduplication, deterministic sharding, and stable diffs.

## 11. Validation

Parsing and semantic validation are separate phases.

The semantic validator MUST reject at least the following:

- unsupported selector dimensions;
- duplicate selector dimensions;
- positional mappings with unequal cardinality;
- positional mappings without deterministic order;
- exact mappings with unequal corresponding extents;
- closed coverage or implication mappings with unequal total extent;
- implicit zipping of unordered selections;
- recursive exact mappings whose descendant structures do not correspond; and
- source-adapter selections that are invalid or ambiguous.

Ambiguity MUST be rejected rather than guessed.

## 12. Source adapter contract

Each source adapter is responsible for the following:

- resolving opaque references;
- exposing supported selector dimensions;
- evaluating predicates, sets, and ranges;
- defining deterministic sequence ordering;
- resolving wildcards;
- enumerating descendants for `**`;
- exposing stable coordinates for recursive correspondence;
- reporting selection cardinality; and
- rejecting invalid or ambiguous selections.

The core language does not assign intrinsic meaning to dimensions such as `season` or `episode`. Source adapters resolve units independently of extent; the core applies the authored extent after resolution.

## 13. Representative examples

### Same media entity

```text
com.imdb://title/tt0133093 <=> org.themoviedb://movie/603
```

### Same show, descendant structure not asserted

```text
com.thetvdb://series/123 <=> org.themoviedb://tv/456
```

### Entire descendant hierarchy

```text
com.thetvdb://series/123 :: ** <=> org.themoviedb://tv/456 :: **
```

### Episode renumbering

```text
com.thetvdb://series/123 :: episode=2,order=dvd,season=1 <=> org.themoviedb://tv/456 :: episode=1,episode_group=abc123,group=1
```

### Range offset

```text
a.example://show/1 :: episode=1..12 <=> b.example://show/2 :: episode=13..24
```

### Split or combined episode

```text
a.example://show/1 :: episode=1 @2 <~> b.example://show/2 :: episode={1,2}
```

### Different season boundaries

```text
a.example://show/1 <=> b.example://show/2
a.example://show/1 :: season=1,episode=1..12 <=> b.example://show/2 :: season=1,episode=1..12
a.example://show/1 :: season=1,episode=13..24 <=> b.example://show/2 :: season=2,episode=1..12
```

### Cour split across top-level entities

```text
com.thetvdb://series/123 :: episode=1..12,order=aired,season=2 <~> [co.anilist://media/100 @6,co.anilist://media/101 @6]
```

## 14. Core invariants

Implementations MUST preserve these invariants:

1. References are opaque to the core language.
2. Selectors are interpreted by source adapters.
3. Predicates are filters, not universal hierarchy traversal.
4. Bare mappings never implicitly recurse.
5. `**` explicitly selects the complete descendant hierarchy.
6. Ranges are ordered.
7. Sets are unordered.
8. Composites are ordered.
9. Unordered selections are never implicitly zipped.
10. Cardinality comes from expressions, not separate operators.
11. Omitted extent means `@1` for every resolved synchronization unit.
12. Extent is relative structure, not synchronization policy.
13. `<=>` means direct synchronization equivalence with equal corresponding extents.
14. `<~>` means collective coverage equivalence with aligned total extent.
15. `=>` means safe semantic implication with left-to-right extent alignment.
16. Coverage equivalence does not imply arbitrary fractional-progress translation.
17. Ambiguity is rejected rather than guessed.
