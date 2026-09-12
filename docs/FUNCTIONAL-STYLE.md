# Functional style and immutability — fleet rule

This rule applies to every language in the fleet (Rust, Go, TypeScript on Node/Bun/Deno,
Gleam, Dart/Flutter, and anything else that shows up). It is the house answer to a habit
we inherited from Go: hand a pointer into a function, let the function mutate it, read the
result out of the variable afterwards.

## The rule in one line

**Build values; don't mutate them.** A function takes its inputs and returns a *new* value.
The caller never has to look at a variable it passed in to learn what happened.

```
// not this                          // this
var foo Foo                          foo := NewFoo(inputs)
fill(&foo)                           fmt.Println(foo)
fmt.Println(foo)
```

"New value" means a brand-new object/struct with all of its own fields, not a shallow copy
whose nested collections still alias the original. If a nested map or slice is unchanged it
may be *moved* (Rust) or shared *only when it is never written again* (Go/TS/Dart); when in
doubt, rebuild it.

## What this looks like per language

**Rust.** No `&mut T` parameters except `&mut self` on a type that owns the state, and
`fmt::Formatter` / streaming APIs the standard library forces on us. Prefer:

- `fn build(inputs) -> T` and `fn with_x(self, x) -> Self` (struct-update syntax `Self { x, ..self }`
  moves the untouched fields — it is the cheapest "new object" there is).
- `Option<T>` / `Result<T, E>` returned, never filled through an out-parameter
  (`set_once(slot: Option<T>, v) -> Result<Option<T>, E>`, not `set_once(&mut slot, v)`).
- Accumulators become iterator chains: `rules.iter().filter_map(|r| r(cfg)).chain(...).collect()`
  instead of `let mut issues = Vec::new(); ... issues.push(..)`.
- Parsers and state machines become `try_fold` over an immutable state struct whose `step`
  returns a new state.
- Generics do the heavy lifting: a `type Rule = fn(&Config) -> Option<Issue>` table, `impl
  Iterator<Item = T>` return types, `FnOnce() -> String` for lazy messages.

**Go.** Generics are narrower, but the same shape works:

- Constructors return values: `func NewFoo(...) Foo`, `func (f Foo) WithX(x X) Foo`.
- Validation as a `[]Rule[T]` table run by a generic `runRules`; multi-issue rules as
  `filterSlice` + `mapSlice`. See `src/golang/fp.go` in ores-middleware for the tiny helper set
  (`Rule`, `issueWhen`, `runRules`, `mapSlice`, `filterSlice`, `filterMap`, `mapOf`, `kv`,
  `kvWhen`, `mergeMaps`). Copy it; it has no dependencies.
- Maps are built with `mapOf(kv(..), kvWhen(cond, ..))`, not `m := map{}; if c { m[k] = v }`.
- Pointer receivers are for types that *are* a stateful store (a cache, a registry, a
  connection). Everything else uses value receivers and returns new values.
- Writing to `http.Header`, `io.Writer`, a `*sql.DB` etc. is an *effect*: compute the value
  first in a pure function, then apply it in one small boundary function
  (`securityHeaders(cfg) map[string]string` → `applySecurityHeaders(cfg, hdr)`).

**TypeScript (Node, Bun, Deno).** `const` everywhere; a `let` that is reassigned inside a
closure and read outside it is a bug waiting to happen — return the value instead
(`{ response, context }`). Build objects with spread, arrays with
`map`/`filter`/`flatMap`, and validation as `rules.flatMap(rule => rule(config))`. Use
`readonly` on rule tables and public arrays. Never `push` into an array that was passed in.

**Gleam.** Already immutable; the rule is about *shape*: prefer `list.fold`/`result.try`
pipelines over hand-rolled recursion with accumulators when the pipeline reads better, and
return `Result(value, error)` rather than sentinel values.

**Dart / Flutter.** `final` fields, `const` constructors where possible, `copyWith` that
returns a new instance, and `UnmodifiableListView` / `Map.unmodifiable` on anything handed to a
caller. Collections are built with collection-if/for and spread, not `add` in a loop.
State-holding objects (ChangeNotifier, Bloc, Rx subjects) are the *boundary*: the values
they hold are immutable; only the holder swaps them.

## Hot paths: when imperative is the right answer

If a path is genuinely hot — per-request or per-packet, inside a lock, zeroizing secrets, a
streaming `Mac`/`Hasher`/`Write` API, a bounded cache — and allocating fresh values would
measurably cost, keep it imperative **and say so** with a comment that is greppable across
every language:

```
// HOT-PATH (imperative by design): <what it does>, <why a new value per call is too
// expensive or impossible>, <where the mutation is confined>, <what immutable value
// callers still receive>.
```

The four clauses are the point. A HOT-PATH comment without the *why* is a smell; a
performance claim that hasn't been measured belongs in a `// TODO(measure):` instead.

Examples of legitimate hot paths in this repo: the token-bucket store purge inside the
rate-limiter lock (`purge_expired`, `evict_one`), HMAC key-material streaming that must be
zeroized in place, and the Go `http.Header` write boundary.

## Review checklist

- New `&mut`/pointer parameter, or a `let`/`var` mutated across a function boundary? Needs
  either a rewrite to a returned value or a `HOT-PATH (imperative by design)` comment.
- Function fills a caller-owned collection? Return the collection instead.
- Shallow copy that still aliases a nested map/slice which is later written? Rebuild it.
- Constructor that leaves the object half-initialised for a later `init(&self)`? Make the
  constructor return the finished value, or return `Result`.
- The imperative form is *shorter* but hides a state transition? Prefer the explicit
  functional form; readability of state transitions beats line count.
