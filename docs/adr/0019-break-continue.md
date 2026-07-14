# ADR 0019 — `break` and `continue`

- **Status:** Accepted
- **Date:** 2026-07-14
- **Extends:** ADR 0010 (which deferred them as a named seat), ADR 0017
  (Phase C item), ADR 0018 (the feature recipe this follows)

## Context

Loops have no early exit: leaving a `while`/`for` early means flag
variables or function extraction, the last piece of control-flow
boilerplate in everyday code. ADR 0010 deferred `break`/`continue`;
ADR 0017 scheduled them in Phase C. Their semantics are settled practice;
what this ADR pins is the interaction with this language's specifics —
live iteration, narrowing, definite return — and the lowering story.

## Decisions

1. **`break;` exits the innermost enclosing loop; `continue;` skips to
   its next iteration.** Unlabeled only, like C: for `while`, `continue`
   re-evaluates the condition; for `for`, it advances to the next
   element — index increment plus the live length re-read of ADR 0010.
   Labeled loops are a deferred seat (own ADR if real programs demand
   them); until then, nested escapes use a flag or a function.
2. **Hard keywords.** `break` and `continue` are reserved words. This is
   a pre-1.0 breaking change under ADR 0017's stability policy: no
   corpus program, example, or golden uses either as an identifier —
   zero goldens move.
3. **Outside a loop is a checker error** ("'break' outside of a loop"),
   not a parse error: the parser accepts the statement anywhere, so
   recovery stays statement-shaped and the diagnostic carries a span.
4. **Definite-return analysis is unchanged.** Loops still never satisfy
   "always returns", and `break`/`continue` are not returns. `while true
   { … break; }` therefore still needs a return after the loop —
   conservative, sound, and zero new analysis. Code after a
   `break`/`continue` in the same block is silently dead, exactly like
   code after `return` today.
5. **Narrowing is unaffected.** Both statements carry no expressions and
   assign nothing: `body_effects` treats them as inert, and the existing
   loop-entry fact invalidation already covers every path they create.
6. **Lowering story: control flow only, no layout.** The lowerer keeps a
   stack of `(continue_target, break_target)` labels, pushed per loop:
   `while` pushes (condition label, end label); `for` gains a dedicated
   continue label placed before its index increment — jumping to the
   loop top instead would skip the increment and re-run the same
   element. `break`/`continue` lower to one `jmp` each.
7. **Interpreter (the normative semantics):** control flow gains
   `Break`/`Continue` signals beside `Return`; blocks propagate them
   upward, the innermost loop consumes them. Reaching a function
   boundary is checker-impossible and stays an `unreachable!` arm.

## Consequences

**Positive:** loop control without flag variables; the feature costs one
lowering plus spec artifacts (ADR 0018's promise); no representation or
layout questions at all.

**Accepted costs:** two new reserved words (documented break, zero
in-repo impact); dead statements after a jump are allowed silently —
consistent with `return` today, revisit only if a reachability lint
ever lands.
