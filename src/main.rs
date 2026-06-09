//! x4-xpath-validator
//!
//! For each `<diff>`-format XML in an X4 mod, check that every
//! `<add sel="...">`, `<replace sel="...">`, and `<remove sel="...">` xpath
//! still resolves against a vanilla X4 snapshot extracted by extract-vanilla.ps1.
//!
//! Exit code: 0 if all xpaths resolve and there are no parse errors; 1 otherwise.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::Serialize;
use sxd_document::Package;
use sxd_document::dom::{ChildOfElement, Document, Element};
use sxd_document::parser as sxd_parser;
use sxd_document::QName;
use sxd_xpath::nodeset::Node;
use sxd_xpath::{Factory, Value};
use walkdir::WalkDir;

/// Strict XML wellformedness check using quick-xml. Catches close-tag name
/// mismatches that sxd-document silently accepts. Returns first error or Ok.
fn strict_parse_check(source: &str) -> std::result::Result<(), String> {
    let mut reader = Reader::from_str(source);
    let config = reader.config_mut();
    config.check_end_names = true;
    config.expand_empty_elements = false;
    config.trim_text(false);

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => return Ok(()),
            Err(e) => {
                let pos = reader.buffer_position() as usize;
                let line = source[..pos.min(source.len())]
                    .bytes()
                    .filter(|&b| b == b'\n')
                    .count()
                    + 1;
                return Err(format!("line {}: {}", line, e));
            }
            _ => {}
        }
        buf.clear();
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "x4-xpath-validator",
    about = "Validate X4 mod <diff> sel xpaths against a vanilla snapshot.",
    long_about = None,
)]
struct Args {
    /// Path to mod root (e.g. I:\Software\dynamic_universe)
    #[arg(long)]
    mod_root: PathBuf,

    /// Path to extracted vanilla snapshot (e.g. I:\Software\x4_vanilla\9.0_rc4)
    #[arg(long)]
    vanilla: PathBuf,

    /// Optional path to write the full report as JSON.
    #[arg(long)]
    json_out: Option<PathBuf>,

    /// Only print summary and failures.
    #[arg(long)]
    quiet: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
enum Op {
    Add,
    Replace,
    Remove,
}

impl Op {
    fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "add" => Some(Op::Add),
            "replace" => Some(Op::Replace),
            "remove" => Some(Op::Remove),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Op::Add => "add",
            Op::Replace => "replace",
            Op::Remove => "remove",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
enum Status {
    Ok,
    BrokenXpath,
    MissingVanilla,
    ParseError,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::BrokenXpath => "BROKEN",
            Status::MissingVanilla => "NO VANILLA",
            Status::ParseError => "PARSE",
        }
    }
}

#[derive(Debug, Serialize)]
struct Check {
    mod_file: String,
    vanilla_file: Option<String>,
    op: Option<&'static str>,
    sel: Option<String>,
    status: Status,
    detail: String,
}

/// Mod paths that are full-file mod scripts, not diffs. Skip xpath checking on these.
fn is_non_diff(rel: &Path) -> bool {
    let s = rel.to_string_lossy().replace('\\', "/");
    s.starts_with("aiscripts/order.da.")
        || s.starts_with("md/dynamicuniverse")
        || s.starts_with("md/deadairdynamicuniverse")
        || s.starts_with("t/0001.xml")
}

/// Resolve a mod-relative path against the vanilla snapshot root. For mod files
/// living under `extensions/ego_dlc_X/...`, the vanilla side keeps the same path;
/// for base files under `libraries/...` etc, it's a direct join.
fn resolve_vanilla_path(vanilla_root: &Path, mod_rel: &Path) -> Option<PathBuf> {
    let candidate = vanilla_root.join(mod_rel);
    if candidate.exists() { Some(candidate) } else { None }
}

// ---------- deep clone across packages ----------

fn deep_clone_child<'dst>(
    src: ChildOfElement<'_>,
    dst_doc: Document<'dst>,
) -> Option<ChildOfElement<'dst>> {
    match src {
        ChildOfElement::Element(src_el) => {
            let sname = src_el.name();
            let qn = QName::with_namespace_uri(sname.namespace_uri(), sname.local_part());
            let new_el = dst_doc.create_element(qn);
            for attr in src_el.attributes() {
                let an = attr.name();
                let aqn = QName::with_namespace_uri(an.namespace_uri(), an.local_part());
                new_el.set_attribute_value(aqn, attr.value());
            }
            for c in src_el.children() {
                if let Some(cloned) = deep_clone_child(c, dst_doc) {
                    new_el.append_child(cloned);
                }
            }
            Some(ChildOfElement::Element(new_el))
        }
        ChildOfElement::Text(t) => Some(ChildOfElement::Text(dst_doc.create_text(t.text()))),
        ChildOfElement::Comment(c) => {
            Some(ChildOfElement::Comment(dst_doc.create_comment(c.text())))
        }
        _ => None,
    }
}

// ---------- diff application ----------

/// Collect direct text-node children of a diff op (the body for attribute-set ops).
fn collect_text(el: Element<'_>) -> String {
    let mut s = String::new();
    for c in el.children() {
        if let ChildOfElement::Text(t) = c {
            s.push_str(t.text());
        }
    }
    s
}

/// True if this diff element's body contains any element children (vs text-only).
fn has_element_children(el: Element<'_>) -> bool {
    el.children()
        .into_iter()
        .any(|c| matches!(c, ChildOfElement::Element(_)))
}

/// Replace `target` in its parent's child list with the deep-cloned children of `diff_el`.
fn replace_element_with_diff_content<'van>(
    target: Element<'van>,
    diff_el: Element<'_>,
    van_doc: Document<'van>,
) {
    let parent_el = match target.parent() {
        Some(sxd_document::dom::ParentOfChild::Element(p)) => p,
        _ => return,
    };
    let current = parent_el.children();
    let mut new_children: Vec<ChildOfElement<'van>> = Vec::with_capacity(current.len());
    for c in current {
        if matches!(c, ChildOfElement::Element(e) if e == target) {
            for diff_child in diff_el.children() {
                if let Some(cloned) = deep_clone_child(diff_child, van_doc) {
                    new_children.push(cloned);
                }
            }
        } else {
            new_children.push(c);
        }
    }
    parent_el.replace_children(new_children);
}

/// Insert deep-cloned children of `diff_el` as siblings of `target`, before or after.
fn insert_sibling_with_diff_content<'van>(
    target: Element<'van>,
    diff_el: Element<'_>,
    van_doc: Document<'van>,
    after: bool,
) {
    let parent_el = match target.parent() {
        Some(sxd_document::dom::ParentOfChild::Element(p)) => p,
        _ => return,
    };
    let current = parent_el.children();
    let mut new_children: Vec<ChildOfElement<'van>> = Vec::with_capacity(current.len() + 4);
    for c in current {
        let is_target = matches!(c, ChildOfElement::Element(e) if e == target);
        if is_target && !after {
            for diff_child in diff_el.children() {
                if let Some(cloned) = deep_clone_child(diff_child, van_doc) {
                    new_children.push(cloned);
                }
            }
        }
        new_children.push(c);
        if is_target && after {
            for diff_child in diff_el.children() {
                if let Some(cloned) = deep_clone_child(diff_child, van_doc) {
                    new_children.push(cloned);
                }
            }
        }
    }
    parent_el.replace_children(new_children);
}

/// Apply one diff op to a vanilla document. Best-effort: silent no-op if the sel
/// doesn't match anything (the caller already reported the broken/no-match state).
fn apply_diff_op<'van>(
    factory: &Factory,
    op: Op,
    diff_el: Element<'_>,
    sel: &str,
    van_doc: Document<'van>,
) {
    let xpath = match factory.build(sel) {
        Ok(Some(x)) => x,
        _ => return,
    };
    let ctx = sxd_xpath::Context::new();
    let val = match xpath.evaluate(&ctx, van_doc.root()) {
        Ok(v) => v,
        Err(_) => return,
    };
    let nodes: Vec<Node<'van>> = match val {
        Value::Nodeset(ns) => ns.iter().collect(),
        _ => return,
    };

    let type_attr = diff_el.attribute_value("type").map(|s| s.to_string());
    let pos_attr = diff_el.attribute_value("pos").map(|s| s.to_string());

    match op {
        Op::Remove => {
            for n in nodes {
                match n {
                    Node::Element(e) => e.remove_from_parent(),
                    Node::Attribute(a) => a.remove_from_parent(),
                    Node::Text(t) => t.remove_from_parent(),
                    _ => {}
                }
            }
        }
        Op::Replace => {
            let text_value = collect_text(diff_el);
            for n in nodes {
                match n {
                    Node::Attribute(a) => {
                        if let Some(parent) = a.parent() {
                            let an = a.name();
                            let aqn = QName::with_namespace_uri(an.namespace_uri(), an.local_part());
                            parent.set_attribute_value(aqn, &text_value);
                        }
                    }
                    Node::Element(target) => {
                        if has_element_children(diff_el) {
                            replace_element_with_diff_content(target, diff_el, van_doc);
                        } else {
                            // Replace element with the diff body as plain text (rare).
                            if let Some(sxd_document::dom::ParentOfChild::Element(parent)) =
                                target.parent()
                            {
                                let current = parent.children();
                                let mut new_children: Vec<ChildOfElement<'van>> =
                                    Vec::with_capacity(current.len());
                                for c in current {
                                    if matches!(c, ChildOfElement::Element(e) if e == target) {
                                        new_children.push(ChildOfElement::Text(
                                            van_doc.create_text(&text_value),
                                        ));
                                    } else {
                                        new_children.push(c);
                                    }
                                }
                                parent.replace_children(new_children);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Op::Add => {
            if let Some(type_str) = type_attr.as_deref() {
                if let Some(attr_name) = type_str.strip_prefix('@') {
                    let val = collect_text(diff_el);
                    for n in nodes {
                        if let Node::Element(target) = n {
                            target.set_attribute_value(attr_name, &val);
                        }
                    }
                    return;
                }
            }
            match pos_attr.as_deref() {
                Some("before") => {
                    for n in nodes {
                        if let Node::Element(target) = n {
                            insert_sibling_with_diff_content(target, diff_el, van_doc, false);
                        }
                    }
                }
                Some("after") => {
                    for n in nodes {
                        if let Node::Element(target) = n {
                            insert_sibling_with_diff_content(target, diff_el, van_doc, true);
                        }
                    }
                }
                _ => {
                    // Default: append diff children as children of matched element.
                    for n in nodes {
                        if let Node::Element(target) = n {
                            for diff_child in diff_el.children() {
                                if let Some(cloned) = deep_clone_child(diff_child, van_doc) {
                                    target.append_child(cloned);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Iterate the `<add>`, `<replace>`, `<remove>` children of a diff root and apply each to van_doc.
fn apply_diff_doc<'van>(
    factory: &Factory,
    diff_root: Element<'_>,
    van_doc: Document<'van>,
) {
    for child in diff_root.children() {
        if let ChildOfElement::Element(diff_el) = child {
            if let Some(op) = Op::from_tag(diff_el.name().local_part())
                && let Some(sel) = diff_el.attribute_value("sel")
            {
                apply_diff_op(factory, op, diff_el, sel, van_doc);
            }
        }
    }
}

/// Locate all DLC overlay files in the vanilla snapshot whose path is
/// `extensions/ego_dlc_*/<rel>`. Returns the resolved absolute paths.
fn find_dlc_overlays(vanilla_root: &Path, base_rel: &Path) -> Vec<PathBuf> {
    let ext_root = vanilla_root.join("extensions");
    if !ext_root.exists() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&ext_root) {
        for ent in entries.flatten() {
            let name = ent.file_name();
            let n = name.to_string_lossy();
            if n.starts_with("ego_dlc_") {
                let candidate = ent.path().join(base_rel);
                if candidate.exists() {
                    out.push(candidate);
                }
            }
        }
    }
    out
}

/// Cheap textual peek to decide whether the root element of an XML file is `<diff>`.
/// Avoids a full parse just for the routing decision.
fn peek_root_is_diff(path: &Path) -> Option<bool> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with("<?") || t.starts_with("<!--") {
            continue;
        }
        if !t.starts_with('<') {
            continue;
        }
        // skip multi-line comments naively
        if t.starts_with("<!") {
            continue;
        }
        let after = &t[1..];
        let end = after.find(|c: char| c.is_whitespace() || c == '>' || c == '/');
        let name = match end {
            Some(i) => &after[..i],
            None => after,
        };
        return Some(name == "diff");
    }
    None
}

/// For a mod file under `extensions/ego_dlc_X/...`, return its base-file
/// counterpart (`<rel without extensions/ego_dlc_X/ prefix>`).
fn dlc_base_rel(rel: &Path) -> Option<PathBuf> {
    let s = rel.to_string_lossy().replace('\\', "/");
    let after_ext = s.strip_prefix("extensions/")?;
    let slash = after_ext.find('/')?;
    let dlc_id = &after_ext[..slash];
    if !dlc_id.starts_with("ego_dlc_") {
        return None;
    }
    Some(PathBuf::from(&after_ext[slash + 1..]))
}

// ---------- top-level check ----------

#[allow(clippy::too_many_arguments)]
fn evaluate_op_against<'van>(
    factory: &Factory,
    op: Op,
    sel: &str,
    van_doc: Document<'van>,
) -> (Status, String) {
    let xpath = match factory.build(sel) {
        Err(e) => return (Status::BrokenXpath, format!("xpath compile error: {e}")),
        Ok(None) => return (Status::BrokenXpath, "empty xpath".to_string()),
        Ok(Some(x)) => x,
    };
    let ctx = sxd_xpath::Context::new();
    match xpath.evaluate(&ctx, van_doc.root()) {
        Err(e) => (Status::BrokenXpath, format!("xpath eval error: {e}")),
        Ok(Value::Nodeset(ns)) => {
            if ns.size() == 0 {
                (Status::BrokenXpath, "matched 0 nodes".to_string())
            } else {
                (Status::Ok, format!("{} matched {} node(s)", op.as_str(), ns.size()))
            }
        }
        Ok(Value::Boolean(b)) => (
            if b { Status::Ok } else { Status::BrokenXpath },
            format!("boolean result: {b}"),
        ),
        Ok(Value::Number(n)) => (Status::Ok, format!("number result: {n}")),
        Ok(Value::String(s)) => (Status::Ok, format!("string result: {s:?}")),
    }
}

fn check_mod_file(
    mod_file: &Path,
    mod_root: &Path,
    vanilla_root: &Path,
    out: &mut Vec<Check>,
) -> Result<()> {
    let rel = mod_file
        .strip_prefix(mod_root)
        .unwrap_or(mod_file)
        .to_path_buf();

    let mod_text = fs::read_to_string(mod_file)
        .with_context(|| format!("read mod file {}", mod_file.display()))?;

    if let Err(e) = strict_parse_check(&mod_text) {
        out.push(Check {
            mod_file: rel.display().to_string(),
            vanilla_file: None,
            op: None,
            sel: None,
            status: Status::ParseError,
            detail: format!("strict parse error: {e}"),
        });
        return Ok(());
    }

    if is_non_diff(&rel) {
        return Ok(());
    }

    let mod_pkg = match sxd_parser::parse(&mod_text) {
        Ok(p) => p,
        Err(e) => {
            out.push(Check {
                mod_file: rel.display().to_string(),
                vanilla_file: None,
                op: None,
                sel: None,
                status: Status::ParseError,
                detail: format!("mod parse error: {e:?}"),
            });
            return Ok(());
        }
    };

    let mod_doc = mod_pkg.as_document();
    let mod_root_el = match mod_doc
        .root()
        .children()
        .into_iter()
        .find_map(|c| c.element())
    {
        Some(e) => e,
        None => return Ok(()),
    };
    if mod_root_el.name().local_part() != "diff" {
        return Ok(());
    }

    let mut ops: Vec<(Op, String, Element<'_>)> = Vec::new();
    for child in mod_root_el.children() {
        if let ChildOfElement::Element(el) = child
            && let Some(op) = Op::from_tag(el.name().local_part())
            && let Some(sel) = el.attribute_value("sel")
        {
            ops.push((op, sel.to_string(), el));
        }
    }

    if ops.is_empty() {
        return Ok(());
    }

    // Resolve the vanilla validation target. There are three cases:
    //
    //   1. Mod file is a base-game diff (e.g. `libraries/jobs.xml`).
    //      Target = vanilla's `libraries/jobs.xml`. Optionally apply every DLC
    //      overlay at the same path so the mod sees DLC-added content too.
    //
    //   2. Mod file is DLC-scoped and the vanilla equivalent is itself a `<diff>`
    //      (e.g. boron/terran jobs.xml). Target = base + that DLC's diff overlay.
    //
    //   3. Mod file is DLC-scoped and the vanilla equivalent is a full document
    //      (e.g. split/jobs.xml has root `<jobs>`, terran/mapdefaults.xml has
    //      root `<defaults>`). Target = that DLC file directly, no merging.
    let mut base_rel = rel.clone();
    let mut dlc_overlay_chain: Vec<PathBuf> = Vec::new();
    if let Some(br) = dlc_base_rel(&rel) {
        let vanilla_dlc = vanilla_root.join(&rel);
        let vanilla_base = vanilla_root.join(&br);
        if vanilla_dlc.exists() {
            let dlc_is_diff = peek_root_is_diff(&vanilla_dlc).unwrap_or(false);
            if dlc_is_diff && vanilla_base.exists() {
                base_rel = br.clone();
                dlc_overlay_chain.push(vanilla_dlc);
            }
            // else: keep base_rel = rel (the DLC file is the target).
        } else if vanilla_base.exists() {
            base_rel = br.clone();
        }
    }

    let vanilla_file = match resolve_vanilla_path(vanilla_root, &base_rel) {
        Some(p) => p,
        None => {
            for (op, sel, _) in &ops {
                out.push(Check {
                    mod_file: rel.display().to_string(),
                    vanilla_file: None,
                    op: Some(op.as_str()),
                    sel: Some(sel.clone()),
                    status: Status::MissingVanilla,
                    detail: format!("no vanilla file at {} in snapshot", base_rel.display()),
                });
            }
            return Ok(());
        }
    };

    let vanilla_text = fs::read_to_string(&vanilla_file)
        .with_context(|| format!("read vanilla file {}", vanilla_file.display()))?;

    let vanilla_pkg: Package = match sxd_parser::parse(&vanilla_text) {
        Ok(p) => p,
        Err(e) => {
            out.push(Check {
                mod_file: rel.display().to_string(),
                vanilla_file: Some(
                    vanilla_file
                        .strip_prefix(vanilla_root)
                        .unwrap_or(&vanilla_file)
                        .display()
                        .to_string(),
                ),
                op: None,
                sel: None,
                status: Status::ParseError,
                detail: format!("vanilla parse error: {e:?}"),
            });
            return Ok(());
        }
    };
    let vanilla_doc = vanilla_pkg.as_document();

    // If the base vanilla is itself a `<diff>` document, we can't materialize the
    // pre-existing tree, so emit BROKEN-with-explanation rather than silently OK.
    let vanilla_root_el = match vanilla_doc
        .root()
        .children()
        .into_iter()
        .find_map(|c| c.element())
    {
        Some(e) => e,
        None => {
            for (op, sel, _) in &ops {
                out.push(Check {
                    mod_file: rel.display().to_string(),
                    vanilla_file: Some(
                        vanilla_file
                            .strip_prefix(vanilla_root)
                            .unwrap_or(&vanilla_file)
                            .display()
                            .to_string(),
                    ),
                    op: Some(op.as_str()),
                    sel: Some(sel.clone()),
                    status: Status::ParseError,
                    detail: "vanilla has no root element".to_string(),
                });
            }
            return Ok(());
        }
    };

    let factory = Factory::new();

    // Vanilla side: if the base file is also a diff (e.g. mod targets a file that's
    // entirely DLC-scoped on the vanilla side), bail with a clearer message.
    if vanilla_root_el.name().local_part() == "diff" && dlc_overlay_chain.is_empty() {
        for (op, sel, _) in &ops {
            out.push(Check {
                mod_file: rel.display().to_string(),
                vanilla_file: Some(
                    vanilla_file
                        .strip_prefix(vanilla_root)
                        .unwrap_or(&vanilla_file)
                        .display()
                        .to_string(),
                ),
                op: Some(op.as_str()),
                sel: Some(sel.clone()),
                status: Status::BrokenXpath,
                detail: "vanilla equivalent is itself a <diff>; no base to validate against"
                    .to_string(),
            });
        }
        return Ok(());
    }

    // Pre-apply DLC overlays so mod xpaths see the merged base+DLC tree X4 builds at runtime.
    // For a mod base diff (libraries/jobs.xml), apply *every* DLC's overlay at that path.
    // For a mod DLC-scoped diff (extensions/ego_dlc_X/libraries/jobs.xml), apply only that DLC's overlay.
    let overlay_paths: Vec<PathBuf> = if dlc_overlay_chain.is_empty() {
        find_dlc_overlays(vanilla_root, &base_rel)
    } else {
        dlc_overlay_chain
    };

    for overlay_path in &overlay_paths {
        let overlay_text = match fs::read_to_string(overlay_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let overlay_pkg = match sxd_parser::parse(&overlay_text) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let overlay_doc = overlay_pkg.as_document();
        if let Some(root_el) = overlay_doc
            .root()
            .children()
            .into_iter()
            .find_map(|c| c.element())
            && root_el.name().local_part() == "diff"
        {
            apply_diff_doc(&factory, root_el, vanilla_doc);
        }
    }

    let vanilla_rel = vanilla_file
        .strip_prefix(vanilla_root)
        .unwrap_or(&vanilla_file)
        .display()
        .to_string();

    // Evaluate each mod op against the (mutated) vanilla, then apply it so subsequent
    // ops see the in-file state — fixes the sequential-diff false-positive case.
    for (op, sel, diff_el) in ops {
        let (status, detail) = evaluate_op_against(&factory, op, &sel, vanilla_doc);
        out.push(Check {
            mod_file: rel.display().to_string(),
            vanilla_file: Some(vanilla_rel.clone()),
            op: Some(op.as_str()),
            sel: Some(sel.clone()),
            status,
            detail,
        });
        apply_diff_op(&factory, op, diff_el, &sel, vanilla_doc);
    }

    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.mod_root.exists() {
        anyhow::bail!("mod path not found: {}", args.mod_root.display());
    }
    if !args.vanilla.exists() {
        anyhow::bail!("vanilla snapshot path not found: {}", args.vanilla.display());
    }

    let mut checks: Vec<Check> = Vec::new();

    for entry in WalkDir::new(&args.mod_root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|s| s.to_str()) != Some("xml") {
            continue;
        }
        if let Err(e) = check_mod_file(entry.path(), &args.mod_root, &args.vanilla, &mut checks) {
            eprintln!("warn: {}: {e:#}", entry.path().display());
        }
    }

    let mut ok = 0usize;
    let mut broken = 0usize;
    let mut missing = 0usize;
    let mut parse_err = 0usize;
    for c in &checks {
        match c.status {
            Status::Ok => ok += 1,
            Status::BrokenXpath => broken += 1,
            Status::MissingVanilla => missing += 1,
            Status::ParseError => parse_err += 1,
        }
    }

    if !args.quiet {
        for c in checks.iter().filter(|c| c.status == Status::Ok) {
            print_row(c);
        }
    }
    for c in checks.iter().filter(|c| !matches!(c.status, Status::Ok)) {
        print_row(c);
    }

    println!();
    println!(
        "Summary: ok={ok}  broken={broken}  missing-vanilla={missing}  parse-errors={parse_err}"
    );

    if let Some(path) = args.json_out {
        let json = serde_json::to_string_pretty(&checks)?;
        fs::write(&path, json).with_context(|| format!("write json {}", path.display()))?;
        println!("Full report: {}", path.display());
    }

    if broken == 0 && parse_err == 0 {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

fn print_row(c: &Check) {
    let op = c.op.unwrap_or("-");
    let sel = c.sel.as_deref().unwrap_or("-");
    println!(
        "[{:<10}] {}  {} sel={}  -- {}",
        c.status.label(),
        c.mod_file,
        op,
        sel,
        c.detail
    );
}
