# Blitzen

sigma-rust's runner for [SANTA](https://github.com/mwaddip/santa), the Ergo consensus conformance
suite. SANTA holds test vectors whose expected answers were recorded ("blessed") from the
reference implementation (sigma-state and the JVM node); Blitzen runs them through sigma-rust,
grades sigma-rust's answers against the blessed ones, and writes its results; SANTA re-grades them. Where sigma-rust answers
differently, that is a divergence to look at, not something this runner smooths over.

It covers four tiers:

| tier | what an entry is | what Blitzen answers |
|---|---|---|
| eval | an ErgoTree, an input, the `(activated, ergoTree)` versions | `{value, cost, error}` |
| wire | bytes of a `Constant`, `Box`, `Transaction`, `SigmaBoolean`, `ErgoTree` or block-transactions section | the bytes sigma-rust writes back |
| transaction | a transaction, its input boxes, the headers, pre-header and parameters it is validated under | `{valid, cost, error}` |
| nipopow | a header chain, and an interlinks or proof request over it | the interlinks, or the proof's bytes |

The formats are SANTA's runner contract
([`docs/contract/runner-contract.md`](https://github.com/mwaddip/santa/blob/main/docs/contract/runner-contract.md)
and its `-wire`, `-transaction` and `-nipopow` companions). This sigma-rust has no JIT-cost
model, so the runner declares `cost: false` and SANTA grades values and verdicts only.

## Run it

```bash
cargo test                                            # the runner's own tests
cargo run --release -- <vector.json | dir>            # run entries, compare with `expected`, print nice/coal
cargo run --release -- run runner.json <santa-dir> <out-dir>   # the self-contained runner
SANTA_DIR=<santa-dir> cargo test --manifest-path harness/Cargo.toml   # the harness's own tests
```

`santa-run <impl-path> <santa-dir> <out-dir>` is the entry SANTA calls: it builds the runner and runs
`blitzen run`, which, through the harness (`harness/`), checks its grading interpreter and selection
against the SANTA checkout's oracles, selects the vector files `runner.json`'s scope covers (`version`,
`tiers`, `cost`), runs them (each bounded file in a child process under its budget), grades every
answer by SANTA's `vectors/grading.json`, and writes `results.json` and `actuals/` (SANTA's
runner-integration §3, §4). SANTA re-grades the actuals as a canary.

## How it works

- **`eval.rs`**: evaluates an entry's tree in the canonical evaluation context the reference
  builds for a bare evaluation (SELF holding the entry's tree, `INPUTS = [SELF]`, no outputs,
  height 0, a fixed pre-header), with the entry's input bound where its envelope puts it. Trees
  are read with `ErgoTree::sigma_parse_bytes_lenient`, since the vectors' roots are of any type,
  and evaluated with `eval::test_util::try_eval_with_deserialize`, the public entry (behind the
  `arbitrary` feature) that evaluates a root of any type. `Context` holds exactly ten headers
  where the canonical context has none, so it carries ten copies of one fixed header.
- **`wire.rs`**: parses a kind's bytes with sigma-rust's serializer and writes them back. The
  `ErgoTree` kind goes through the lenient parse, so the tree is re-encoded from its structure.
- **`transaction.rs`**: decodes the transaction and its boxes from their bytes, builds the
  `ErgoStateContext` from the entry's own headers, pre-header and parameters, and runs ergo-lib's
  stateful validation (`TransactionContext::validate`). A validation error is a clean
  `valid: false` with sigma-rust's reason; bytes sigma-rust cannot read are `errored`.
- **`nipopow.rs`**: rebuilds the chain as `PoPowHeader`s and answers interlinks or
  `NipopowAlgos::prove`. The proof's bytes end with its `continuous` byte (0), which the node's
  wire encoding adds and sigma-rust's proof serializer leaves out.
- **`sval.rs`** and **`binding.rs`**: the bridge between SANTA's JSON values and sigma-rust's.
  `Long` and `BigInt` are decimal strings, bytes lower-case hex, a `SigmaProp` its bare
  `SigmaBoolean` bytes; an input given as `Value` bytes is read with sigma-rust's own reader for
  that position.
- **`main.rs`**: the CLI, and the dispatch by vector schema. Every entry gets exactly one outcome:
  a value or verdict, `errored` (sigma-rust's own refusal, a divergence where the reference
  accepted), `not-implemented`, `panicked` (a panic, caught, or a failure of the runner itself,
  with a note), or, for a bounded file, `exceeded` (its child went over its memory or time budget).

## Until this runner lives in sigma-rust

Today it is a separate repository that SANTA checks out next to the sigma-rust under test:
`Cargo.toml` depends on `../sigma-rust/<crate>`, and `santa-run` links `../sigma-rust` to the
checkout SANTA passes as `<impl-path>`. Upstream sigma-rust has no lenient parse yet, so
`santa-run` applies `patches/sigma-rust-lenient-parse-body.patch` to that checkout for the build
and restores it on exit. Both go away when the runner moves into sigma-rust's `santa/` directory
with the lenient parse as a real entry.
