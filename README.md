# Blitzen

The **sigma-rust** runner for the [SANTA](../santa) Ergo consensus conformance
suite, covering the **eval**, **wire**, and **transaction** tiers. Blitzen runs
SANTA's committed vectors through sigma-rust and reports per-entry actuals —
eval `{ value, cost, error }`, wire `{ bytes_hex, error }`, transaction
`{ valid, cost, error }` — the runner half of the `run(vector) → actuals`
contract (SANTA `docs/contract/runner-contract.md`).
The JVM reference (sigma-state / ergo-core) is canonical; where sigma-rust
diverges from the blessed `expected`, that divergence is a **finding surfaced**,
not a bug hidden.

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

**Lenient parse entry** (`ErgoTree::sigma_parse_bytes_lenient` — eval.rs's parse
entry): parses the **real header** — so size-bit semantics (consensus Rule 1012)
are preserved — and accepts an arbitrary-typed (non-`SigmaProp`) root, mirroring
the JVM blesser's `deserializeErgoTree(checkType=false)`. Provenance differs by
branch (SANTA runner-contract §3 — build identity is declared):
- **eni** carries the helper natively behind the `arbitrary` feature (`642041be` —
  the same gated conformance surface as `try_eval_out`); no build patch.
- **develop** (upstream has no helper yet) applies
  `patches/sigma-rust-lenient-parse.patch` to the checkout at build time — additive
  and behavior-neutral (production `sigma_parse` / `sigma_parse_bytes` unchanged);
  the checkout is restored pristine after every run (EXIT trap; healed on entry
  after a crash). The patch retires when the helper + Rule-1012 PR
  (`fix/header-size-bit-rule1012`) merges to develop.

## Build / run

```bash
cargo build
# self-compare: run every entry, compare vs the blessed `expected`, print nice/coal
cargo run -- ../santa/vectors/eval/v5
# emit: write actuals (no comparison) for the SANTA orchestrator, one file per vector
cargo run -- emit ../santa/vectors/eval/v5 /tmp/blitzen-actuals
cargo test            # unit tests (sval round-trips, wire, transaction, panic net)
```

Blitzen is **version-agnostic** — it evaluates each entry under that entry's
declared `(activated, ergoTree)` versions, so it runs the v6 corpus too once that
lands.

## Design (5 modules)

- **`sval.rs`** — the SValue ⇄ canonical-JSON bridge (contract §4): `Long`/`BigInt`
  as decimal strings, lower-case hex, `SigmaProp` as the **bare** serialized
  `SigmaBoolean` (not the ErgoTree-wrapped `prop_bytes()`), the `{tag}` SType form.
- **`eval.rs`** — a deterministic `Context` (input bound at ContextExtension var 1)
  + **lenient tree parse** via `ErgoTree::sigma_parse_bytes_lenient` (real header
  honored — Rule-1012 semantics preserved; arbitrary-typed roots accepted, since
  SANTA corpus roots are non-`SigmaProp`; native on eni / build-patched on develop —
  see the lenient-parse entry note above) + eval via `try_eval_out`, capturing
  the raw JIT cost from `ctx.jit_cost_value()`.
- **`wire.rs`** — wire-tier round-trips: parse a kind's canonical bytes with the
  sigma serializer and reserialize for byte-identity comparison.
- **`transaction.rs`** — transaction-tier validation: decode a captured tx + its
  input boxes from node-API JSON, build the minimal state context the JVM blesser
  pinned (synthetic pre-header at the entry's height, blockVersion = activated+1,
  launch-default parameters), and run ergo-lib's stateful validation
  (`TransactionContext::validate` — the port of the node's `validateStateful`).
  Accept ⇒ `valid: true` (+ the accumulated block-cost total on builds with
  `jit-cost`; upstream has no tx-cost model ⇒ `cost: null`); any
  `TxValidationError` ⇒ a clean `valid: false` + the impl's reason, mirroring the
  JVM oracle's `validateStateful → Failure ⇒ invalid` mapping.
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
