# x4-xpath-validator

A static checker for X4: Foundations mod diff files. It walks every `<diff>`-format XML in your mod and resolves each `<add sel="…">`, `<replace sel="…">`, `<remove sel="…">` xpath against an extracted vanilla snapshot. Anything that doesn't match a real node in vanilla is reported — so you find broken xpaths before X4 silently ignores them at load time.

Catches the failure mode where Egosoft restructures vanilla XML between game versions and your mod's xpaths quietly stop matching, with no error in the log.

---

## What it does

For each `*.xml` under `--mod-root`:

1. **Strict parse check** with `quick-xml` (`check_end_names = true`). Catches close-tag mismatches like `<do_if>…</do_elseif>` that `sxd-document` (and many ad-hoc validators) silently accept. X4's libxml2 enforces this at load time, so the validator must too.
2. **Diff detection** — if the document root is `<diff>`, enumerate its `<add>`, `<replace>`, `<remove>` children and pull the `sel` attribute from each.
3. **Vanilla target resolution** — locate the right vanilla file:
   - Base-game diff (e.g. `libraries/jobs.xml`) → vanilla's same path.
   - DLC-scoped diff (e.g. `extensions/ego_dlc_terran/libraries/jobs.xml`) where the vanilla side is itself a `<diff>` → base + that DLC's diff overlay applied.
   - DLC-scoped diff where the vanilla side is a full document (e.g. `extensions/ego_dlc_split/libraries/jobs.xml` with root `<jobs>`) → that DLC file directly.
   - Missing → `NO VANILLA`.
4. **DLC overlay merge** — for base-file mod diffs, every `extensions/ego_dlc_*/<same path>` that is itself a `<diff>` is applied to vanilla before evaluation, so DLC-added content is visible to the mod xpaths.
5. **Eval-then-apply per op** — each mod diff op is evaluated against the current vanilla tree, then applied to it before the next op runs. Chained replaces (`<replace sel="…">A</replace>` followed by `<replace sel="…[@attr='A']/…">B</replace>`) validate correctly because op N+1 sees the mutated tree from op N.
6. **xpath evaluation** — compile each `sel` with `sxd-xpath` and evaluate against the current vanilla tree. Zero-result nodesets = `BROKEN`.

Non-diff XML in the mod (full-file MD scripts, aiscripts, t-files) is skipped — there's no vanilla equivalent to diff against.

---

## Build

Requires Rust 1.85+ (edition 2024).

```sh
cargo build --release
```

Produces `target/release/x4-xpath-validator` (`.exe` on Windows).

---

## Usage

```sh
x4-xpath-validator \
    --mod-root  /path/to/your_mod \
    --vanilla   /path/to/vanilla_snapshot/9.0_rc4 \
    --quiet \
    --json-out  validation.json
```

### Arguments

| Arg | Required | Description |
|---|---|---|
| `--mod-root` | yes | Path to mod folder (the one containing `content.xml`). |
| `--vanilla` | yes | Path to extracted vanilla XML snapshot. See below. |
| `--quiet` | no | Suppress per-OK rows; only print non-OK + summary. |
| `--json-out <path>` | no | Write the full report as JSON, one row per check. |

### Vanilla snapshot

The snapshot is the vanilla X4 game's XML, extracted from its `.cat`/`.dat` archives with Egosoft's `XRCatTool.exe`. Layout must mirror mod-relative paths:

```
vanilla_snapshot/9.0_rc4/
├── aiscripts/
├── libraries/
├── maps/xu_ep2_universe/
├── md/
├── t/
└── extensions/ego_dlc_split/  …  ego_dlc_terran/  …
```

A PowerShell wrapper for extraction (`extract-vanilla.ps1`) is shipped in the [`dynamic_universe`](https://github.com/IllustrisJack/dynamic_universe) mod repo's docs.

### Output

Each diff op produces one row:

```
[OK        ] libraries/mapdefaults.xml  replace sel=/defaults/dataset[@macro='Cluster_17_Sector001_macro']/properties/area/@tags  -- replace matched 1 node(s)
[BROKEN    ] libraries/modules.xml      replace sel=…  -- matched 0 nodes
[NO VANILLA] (file not in snapshot)     add sel=…
[PARSE     ] libraries/colors.xml       - sel=-  -- strict parse error: line 5: ill-formed document
```

Final line is a summary:

```
Summary: ok=24  broken=0  missing-vanilla=0  parse-errors=0
```

### Exit codes

- `0` — all xpaths resolved, no parse errors.
- `1` — at least one `BROKEN` xpath or `PARSE` error.

Suitable for CI: fail the build if exit is non-zero.

---

## Diff semantics implemented

The internal diff engine applies each op to the in-memory vanilla tree before evaluating the next op's `sel`. Supported:

- `<remove sel="X"/>` — removes matched elements / attributes / text nodes.
- `<replace sel="X/@attr">value</replace>` — sets the attribute on matched parent(s).
- `<replace sel="X">…</replace>` — replaces matched element with the diff body (element content deep-cloned into the vanilla document).
- `<add sel="X">…</add>` — appends diff body as children of matched element.
- `<add sel="X" pos="before|after">…</add>` — inserts diff body as preceding/following siblings.
- `<add sel="X" type="@attr">value</add>` — adds (or replaces) attribute on matched element.

Cross-package deep clone copies element names with namespaces, attributes, text, and comments.

---

## Limitations

- **Whole-file mod scripts skipped.** Non-`<diff>` XML in the mod (added MD scripts, full-file aiscript replacements, t-files) is detected and skipped — there's no vanilla counterpart to validate against.
- **Sibling order in `<replace>` of element nodes** is approximate: the replacement clones are spliced in at the target's position, which is fine for the common cases but isn't proof against positional xpath selectors (`[1]`, `[2]`) used by later diff ops. X4 best practice rejects positional selectors anyway.

---

## Maintainer workflow for an X4 version bump

When Egosoft ships a new X4 version:

1. Extract a fresh vanilla snapshot from the new game version.
2. Run the validator against your mod + the new snapshot.
3. Triage every `BROKEN` row. Typical causes:
   - Egosoft restructured a nested `do_if` chain — the xpath needs one more or one fewer segment.
   - An attribute value changed (`<replace sel="…/@attr=oldvalue">`) — update the value.
   - A parameter renamed — update the parameter selector.
4. Fix the diffs.
5. Re-run. Stop when `broken=0` and `parse-errors=0`.
6. Smoke-test in-game (the validator is a static check, not a runtime check).

This turns an X4 minor-version port from a half-day of in-game debugging into a one-hour task.

---

## Dependencies

- [`clap`](https://crates.io/crates/clap) — CLI parsing
- [`walkdir`](https://crates.io/crates/walkdir) — recursive file walk
- [`quick-xml`](https://crates.io/crates/quick-xml) — strict wellformedness pass
- [`sxd-document`](https://crates.io/crates/sxd-document) + [`sxd-xpath`](https://crates.io/crates/sxd-xpath) — XPath evaluator
- [`anyhow`](https://crates.io/crates/anyhow), [`serde`](https://crates.io/crates/serde), [`serde_json`](https://crates.io/crates/serde_json)

---

## License

Dual-licensed under MIT or Apache-2.0, at your option.
