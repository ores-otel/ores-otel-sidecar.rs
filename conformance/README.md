# Conformance

`contracts/` is the authority for API and data-shape semantics in `ores-otel/ores-otel-sidecar.rs`. `conformance/` is the shared behavioral corpus that runtime implementations, sidecar adapters, and compatibility layers must consume.

## Rules

1. Keep shared behavioral vectors under `conformance/cases/` and feed the same case bytes to every implementation under test.
2. Do not maintain runtime-specific golden vectors. A case may describe implementation-neutral inputs and a normalized expected receipt, but the expected result must be shared.
3. Before promotion, bind runtime evidence to the exact current contract inputs and conformance-case digests. Stale evidence fails closed.
4. Missing evidence from any runtime or adapter declared required by the promotion gate is a failure, not a skip.
5. Generated reports, normalized runtime receipts, parity reports, and other artifacts are evidence only. They do not become contract or conformance authority.
6. `contracts/` remains authoritative for structure and wire shape; `conformance/` owns shared behavioral expectations. Neither directory silently rewrites the other.

The initial `cases/bootstrap.v1.json` case establishes this repository-level authority boundary only. It does **not** prove domain-level runtime equivalence. Add sidecar protocol and lifecycle cases as normalized inputs plus normalized expected receipts, then make each supported adapter execute the same corpus.

Where a shared contract-instance corpus already exists, conformance runners should reuse or reference those exact bytes instead of creating divergent runtime-local copies.
