# x4-xpath-validator

A static checker for X4: Foundations mod diff files. It walks every `<diff>`-format XML in your mod and resolves each `<add sel="…">`, `<replace sel="…">`, `<remove sel="…">` xpath against an extracted vanilla snapshot. Anything that doesn't match a real node in vanilla is reported — so you find broken xpaths before X4 silently ignores them at load time.

Catches the failure mode where Egosoft restructures vanilla XML between game versions and your mod's xpaths quietly stop matching, with no error in the log.

---

## What it does

For each `*.xml` under `--mod-root`:

1. **Strict parse check** with `quick-xml` (`check_end_names = true`). Catches close-tag mismatches like `<do_if>…</do_elseif>` that `sxd-document` (and many ad-hoc validators) silently accept. X4's libxml2 enforces this at load time, so the validator must too.
2. **Diff detection** — if the document root is `<diff>`, enumerate its `<add>`, `<replace>`, `<remove>` children and pull the `sel` attribute from each.
3. **Vanilla lookup** — resolve the mod-relative path against `--vanilla` (the snapshot root). Non-existent file = `NO VANILLA`.
4. **DLC layering** — if the vanilla "equivalent" is itself a `<diff>` (i.e. a DLC-layered base file), the validator emits `DLC SKIP` rather than a false-positive `BROKEN`, because DLC-merge isn't implemented.
5. **xpath evaluation** — compile each `sel` with `sxd-xpath` and evaluate against the vanilla document. Zero-result nodesets = `BROKEN`.

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
[OK        ] libraries/mapdefaults.xml  replace sel=/defaults/dataset[@macro='Cluster_17_Sector001_macro']/properties/area/@tags  -- matched 1 node(s)
[BROKEN    ] libraries/modules.xml      replace sel=…  -- matched 0 nodes
[NO VANILLA] (file not in snapshot)     add sel=…
[PARSE     ] libraries/colors.xml       - sel=-  -- strict parse error: line 5: ill-formed document
[DLC SKIP  ] extensions/ego_dlc_split/libraries/mapdefaults.xml  …
```

Final line is a summary:

```
Summary: ok=21  broken=0  missing-vanilla=0  parse-errors=0  dlc-skipped=3
```

### Exit codes

- `0` — all xpaths resolved, no parse errors. (DLC SKIP doesn't count against you.)
- `1` — at least one `BROKEN` xpath or `PARSE` error.

Suitable for CI: fail the build if exit is non-zero.

---

## Known limitations

These are tracked, not yet implemented:

1. **Sequential diff dependency.** A `<replace>` whose `sel` matches the *result of a previous `<replace>` in the same file* will report `matched 0 nodes`, because the validator evaluates each diff op against pristine vanilla independently. Workaround: avoid chained replaces that depend on prior diff state (it's also a bad-compat practice).
2. **DLC merge not implemented.** A base-file diff (`libraries/mapdefaults.xml`) targeting a macro that vanilla only defines in a DLC-scoped file (`extensions/ego_dlc_*/libraries/mapdefaults.xml`) currently emits `DLC SKIP` rather than a real check. X4's runtime merges base + DLC before applying mod diffs; the validator does not.
3. **Whole-file mod scripts skipped.** Non-`<diff>` XML in the mod (added MD scripts, full-file aiscript replacements, t-files) is detected and skipped — no validation. That's a different kind of file with no vanilla counterpart.

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
