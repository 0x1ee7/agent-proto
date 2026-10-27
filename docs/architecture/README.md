# Architecture pages

Eight A3 landscape PDFs, drawn from reading the code at the commit printed in each page's footer. The dependency grid and the crate counts are computed from `crates/*/Cargo.toml` and the source files at build time; nothing is typed in by hand. Pages are drawn from reading the code, not generated from the compiler.

- `01-crates-by-layer.pdf`: every crate in its RFC-0001 layer, with its modules and size.
- `02-dependency-grid.pdf`: path dependencies between crates, read from `Cargo.toml`; layer violations fail the build.
- `03-public-api.pdf`: public items per crate, as declared in the source.
- `04-interfaces.pdf`: the items each consumer imports from each provider, read from `use` lines.
- `05-runtime-lifecycle.pdf`: how `a2ad` starts, supervises and stops its child processes.
- `06-approvals.pdf`: the approval path from a gated tool call to the operator's decision.
- `07-audit-path.pdf`: the audit path from an event to the sink and the verifier.
- `08-improvement-register.pdf`: every improvement point, with its evidence and suggested order.

Badges in the pages (for example `E-01`) are keyed to the register on page 08. Colour shows priority: rose is high, amber is medium, grey is low.

Rebuild (needs Python 3.11 or newer for `tomllib`, and Microsoft Edge or Google Chrome; set `CHROME` to the browser path if it is not in a standard install folder):

    python tools/architecture_diagrams.py

The generator is `tools/architecture_diagrams.py`; the PDF kit it uses is `tools/diagram_kit.py`.
