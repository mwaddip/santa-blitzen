# Blitzen

The **sigma-rust** eval-tier runner for the [SANTA](../santa) Ergo consensus
conformance suite. Blitzen evaluates SANTA's committed eval vectors through
sigma-rust and reports each entry's `{ value, cost, error }` — the runner half
of the `run(vector) → actuals` contract (SANTA `docs/contract/runner-contract.md`).
The JVM reference (sigma-state) is canonical; where sigma-rust diverges from the
blessed `expected`, that divergence is a **finding surfaced**, not a bug hidden.

Reindeer naming: Rudolph = the JVM reference, Dasher = ergots, **Blitzen = sigma-rust**.

## Layout

A standalone cargo crate with its own git repo, `path`-depending on a sibling
sigma-rust checkout:

```
~/projects/
  blitzen/            ← this repo
  sigma-rust/sigma-rust/   ← path dep (ergotree-ir, ergotree-interpreter, ergo-chain-types)
  santa/              ← the vectors + the frozen contract
```

`Cargo.lock` is committed and pins the yanked `core2 0.4.0` (sigma-rust's lock is
gitignored upstream — see the `core2_yank_local_lock` note). The
`ergotree-interpreter` **`arbitrary`** feature is enabled because it exposes the
only public arbitrary-root eval entry (`test_util::try_eval_out`); Blitzen never
uses its random generators — it builds a deterministic `Context` by hand.

## Build / run

```bash
cargo build
# self-compare: run every entry, compare vs the blessed `expected`, print nice/coal
cargo run -- ~/projects/santa/vectors/eval/v5
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

## Current state (v5): 1670 / 1705 nice

The remaining 35 coal are **surfaced findings**, not runner bugs:

- **29 unrepresentable** — test boxes whose value (1, 20, …) is below sigma-rust's
  `BoxValue` minimum, which it rejects at parse. The JVM test context allows sub-min
  boxes; sigma-rust cannot represent them → emitted as `unrepresentable`.
- **6 `Coll.updateMany`** — a genuine sigma-rust **JIT-cost undercharge** (1–2 low;
  the value is byte-exact, the cost scales with size). The one real eval finding;
  to be verified against Scala `methods.scala` and fixed on the sigma-rust cost path.
