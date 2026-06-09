# x4-xpath-validator

Static checker for X4: Foundations mod diff files. Walks every `<diff>`-format XML in a mod and resolves each `<add sel="…">`, `<replace sel="…">`, `<remove sel="…">` xpath against an extracted vanilla snapshot. Anything that doesn't match a node in vanilla is reported.

---

## What it does

For each `*.xml` under `--mod-root`:

1. **Strict parse check** with `quick-xml` (`check_end_names = true`). Catches close-tag mismatches like `<do_if>…</do_elseif>`. X4's libxml2 enforces this at load time.
2. **Diff detection** — if the document root is `<diff>`, enumerate its `<add>`, `<replace>`, `<remove>` children and read the `sel` attribute from each.
3. **Vanilla target resolution**:
   - Base-game diff (e.g. `libraries/jobs.xml`) → vanilla's same path.
   - DLC-scoped diff where the vanilla side is itself a `<diff>` → base file + that DLC's diff overlay applied.
   - DLC-scoped diff where the vanilla side is a full document (e.g. `extensions/ego_dlc_split/libraries/jobs.xml` with root `<jobs>`) → that DLC file directly.
   - Missing → `NO VANILLA`.
4. **DLC overlay merge** — for base-file mod diffs, every `extensions/ego_dlc_*/<same path>` that is itself a `<diff>` is applied to vanilla before evaluation.
5. **Eval-then-apply per op** — each mod diff op is evaluated against the current vanilla tree, then applied to it before the next op runs. Chained replaces validate correctly because op N+1 sees the mutated tree from op N.
6. **xpath evaluation** — compile each `sel` with `sxd-xpath`. Zero-result nodesets = `BROKEN`.

Non-diff XML in the mod (full-file MD scripts, aiscripts, t-files) is skipped.

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
| `--vanilla` | yes | Path to extracted vanilla XML snapshot. |
| `--quiet` | no | Suppress per-OK rows; only print non-OK + summary. |
| `--json-out <path>` | no | Write the full report as JSON. |

### Vanilla snapshot

Extracted from X4's `.cat`/`.dat` archives with Egosoft's `XRCatTool.exe`. Layout mirrors mod-relative paths:

```
vanilla_snapshot/9.0_rc4/
├── aiscripts/
├── libraries/
├── maps/xu_ep2_universe/
├── md/
├── t/
└── extensions/ego_dlc_split/  …  ego_dlc_terran/  …
```

An extraction wrapper (`extract-vanilla.ps1`) is shipped in the [`dynamic_universe`](https://github.com/IllustrisJack/dynamic_universe) mod repo under `docs/toolchain.md`.

### Output

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

---

## Diff semantics

The internal diff engine applies each op to the in-memory vanilla tree before evaluating the next op's `sel`. Supported:

- `<remove sel="X"/>` — removes matched elements / attributes / text nodes.
- `<replace sel="X/@attr">value</replace>` — sets the attribute on matched parent(s).
- `<replace sel="X">…</replace>` — replaces matched element with the diff body (deep-cloned into the vanilla document).
- `<add sel="X">…</add>` — appends diff body as children of matched element.
- `<add sel="X" pos="before|after">…</add>` — inserts diff body as preceding/following siblings.
- `<add sel="X" type="@attr">value</add>` — adds (or replaces) attribute on matched element.

Deep clone preserves element names (with namespaces), attributes, text, and comments across packages.

---

## Limitations

- Non-`<diff>` XML in the mod (added MD scripts, full-file aiscript replacements, t-files) is skipped.
- Sibling order in `<replace>` of element nodes is approximate: replacement clones are spliced in at the target's position, which is correct for the common case but not against later diffs using positional selectors (`[1]`, `[2]`).

---

## Version-bump workflow

1. Extract a fresh vanilla snapshot from the new X4 version.
2. Run the validator against the mod + the new snapshot.
3. Fix `BROKEN` rows. Typical causes: restructured `do_if` chain (off-by-one segment), attribute value changed, parameter renamed.
4. Re-run until `broken=0` and `parse-errors=0`.
5. Smoke-test in-game (the validator is a static check).

---

## Dependencies

- `clap`, `walkdir`, `quick-xml`, `sxd-document`, `sxd-xpath`, `anyhow`, `serde`, `serde_json`.

---

## License

Dual-licensed under MIT or Apache-2.0, at your option.
