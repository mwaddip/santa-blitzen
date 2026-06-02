# Blitzen

The **sigma-rust** eval-tier runner for the [SANTA](../santa) Ergo consensus
conformance suite. Blitzen evaluates SANTA's committed eval vectors through
sigma-rust and reports each entry's `{ value, cost, error }` — the runner half
of the `run(vector) → actuals` contract (SANTA `docs/contract/runner-contract.md`).
The JVM reference (sigma-state) is canonical; where sigma-rust diverges from the
blessed `expected`, that divergence is a **finding surfaced**, not a bug hidden.

Reindeer naming: Rudolph = the JVM reference, Dasher = ergots, **Blitzen = sigma-rust**.

## Layout

Blitzen `path`-depends on the sigma-rust crates as a sibling checkout: from this repo,
`../sigma-rust` is the sigma-rust **repo root** (the directory containing `ergotree-ir/`,
`ergotree-interpreter/`, `ergo-chain-types/`). Cargo resolves `path` deps relative to the
manifest, not the CWD, so the build runs from any working directory once that sibling is
in place.

- **SANTA / CI** — SANTA owns the checkout: it clones `impl` (`<url>#<ref>`) into a
  per-instance cache and passes `<impl-path>`; `santa-run` wires `../sigma-rust` to
  `<impl-path>/sigma-rust`, then builds + emits (runner-integration contract §2-3).
- **Local dev** — put a sigma-rust checkout at `../sigma-rust` (a sibling clone).

`Cargo.lock` is committed and pins the yanked `core2 0.4.0` (sigma-rust's lock is
gitignored upstream — see the `core2_yank_local_lock` note). The
`ergotree-interpreter` **`arbitrary`** feature is enabled because it exposes the
only public arbitrary-root eval entry (`test_util::try_eval_out`); Blitzen never
uses its random generators — it builds a deterministic `Context` by hand.

## Build / run

```bash
cargo build
# self-compare: run every entry, compare vs the blessed `expected`, print nice/coal
cargo run -- ../santa/vectors/eval/v5
# emit: write actuals (no comparison) for the SANTA orchestrator, one file per vector
cargo run -- emit ../santa/vectors/eval/v5 /tmp/blitzen-actuals
cargo test            # SValue ⇄ JSON round-trip tests
```

Blitzen is **version-agnostic** — it evaluates each entry under that entry's
declared `(activated, ergoTree)` versions, so it runs the v6 corpus too once that
lands.

## Design (3 modules)

- **`sval.rs`** — the SValue ⇄ canonical-JSON bridge (contract §4): `Long`/`BigInt`
  as decimal strings, lower-case hex, `SigmaProp` as the **bare** serialized
  `SigmaBoolean` (not the ErgoTree-wrapped `prop_bytes()`), the `{tag}` SType form.
- **`eval.rs`** — a deterministic `Context` (input bound at ContextExtension var 1)
  + **lenient tree parse** (clear the size bit + drop the size VLQ so a non-`SigmaProp`
  root skips sigma-rust's root-type check — SANTA roots are arbitrary-typed) + eval
  via `try_eval_out`, capturing the raw JIT cost from `ctx.jit_cost_value()`.
- **`main.rs`** — the CLI + corpus driver. `run_entry` is **blind** (it never reads
  `expected`); the comparison is a separate structural-equality step (contract §5–6).

## SANTA orchestrator integration

The orchestrator runs every runner through a uniform entrypoint, then applies one
shared comparator over all runners' actuals — so "equal" is pinned identically across
runners (runner-contract §6). Blitzen therefore only **emits** actuals on this path; it
never self-judges.

- **`santa-run <impl-path> <vectors-dir> <out-dir>`** — builds against the sigma-rust
  SANTA checked out at `<impl-path>/sigma-rust` (wiring it to our `../sigma-rust`
  path-dep), then runs `blitzen emit`. Exit 0 = actuals written; non-zero = the runner
  itself failed.
- **`runner.json`** — the runner manifest: this runner's identity (`name`/`label`),
  its `impl` = `<url>#<ref>` (the sigma-rust to test), and the selection metadata the
  orchestrator grades/filters on. Its exact field set is defined by the
  [SANTA runner contract](https://github.com/mwaddip/santa/blob/HEAD/docs/contract/runner-contract.md)
  — a living document; refer to it rather than duplicating the schema here.

SANTA owns the `impl` checkout (clones `<url>` per-instance, checks out `<ref>`), so two
runner dirs — e.g. `blitzen-develop` and `blitzen-eni` — pin different refs and compare
the same implementation's branches side by side without colliding.
