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
use sxd_document::parser as sxd_parser;
use sxd_document::Package;
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

/// CLI args.
#[derive(Parser, Debug)]
#[command(
    name = "x4-xpath-validator",
    about = "Validate X4 mod <diff> sel xpaths against a vanilla snapshot.",
    long_about = None,
)]
struct Args {
    /// Path to mod root (e.g. I:\Software\deadair_scripts)
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

/// Operation in a `<diff>`.
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
    DlcUnverified,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::BrokenXpath => "BROKEN",
            Status::MissingVanilla => "NO VANILLA",
            Status::ParseError => "PARSE",
            Status::DlcUnverified => "DLC SKIP",
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

/// Mod file -> vanilla file by identical relative path.
fn resolve_vanilla_path(vanilla_root: &Path, mod_rel: &Path) -> Option<PathBuf> {
    let candidate = vanilla_root.join(mod_rel);
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Crude line counter — sxd-document doesn't preserve source lines, so we scan the
/// raw text and count newlines up to the byte offset of the first occurrence of
/// the sel attribute literal. Good enough to point a human at the file region.
fn locate_sel_line(source: &str, sel_value: &str) -> usize {
    let needle = format!("sel=\"{}\"", sel_value);
    if let Some(idx) = source.find(&needle) {
        source[..idx].bytes().filter(|&b| b == b'\n').count() + 1
    } else {
        0
    }
}

/// Parse mod file, find its `<diff>` root and all add/replace/remove ops, then
/// resolve each sel xpath against the vanilla file.
fn check_mod_file(
    mod_file: &Path,
    mod_root: &Path,
    vanilla_root: &Path,
    out: &mut Vec<Check>,
) -> Result<()> {
    let rel = mod_file.strip_prefix(mod_root).unwrap_or(mod_file).to_path_buf();

    let mod_text = fs::read_to_string(mod_file)
        .with_context(|| format!("read mod file {}", mod_file.display()))?;

    // Strict wellformedness check first. quick-xml with check_end_names catches
    // close-tag name mismatches (e.g. <do_if>...</do_elseif>) that sxd-document
    // silently accepts. X4's libxml2 enforces this at load time, so we must too.
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
    let mod_root_el = match mod_doc.root().children().into_iter().find_map(|c| c.element()) {
        Some(e) => e,
        None => return Ok(()),
    };
    if mod_root_el.name().local_part() != "diff" {
        // Whole-file mod script (md / aiscript / t). Already filtered by is_non_diff
        // for known cases — anything else we just skip silently.
        return Ok(());
    }

    // Build operation list before doing any vanilla work, so we can record a single
    // missing-vanilla row per op.
    let mut ops: Vec<(Op, String)> = Vec::new();
    for child in mod_root_el.children() {
        if let Some(el) = child.element() {
            if let Some(op) = Op::from_tag(el.name().local_part()) {
                if let Some(sel) = el.attribute("sel").map(|a| a.value().to_string()) {
                    ops.push((op, sel));
                }
            }
        }
    }

    if ops.is_empty() {
        return Ok(());
    }

    let vanilla_file = match resolve_vanilla_path(vanilla_root, &rel) {
        Some(p) => p,
        None => {
            for (op, sel) in ops {
                out.push(Check {
                    mod_file: rel.display().to_string(),
                    vanilla_file: None,
                    op: Some(op.as_str()),
                    sel: Some(sel),
                    status: Status::MissingVanilla,
                    detail: format!("no vanilla file at {} in snapshot", rel.display()),
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
    // If the vanilla "equivalent" is itself a <diff> document (DLC layered onto base),
    // our mod xpaths target the merged base+DLC tree at X4 runtime — but we don't yet
    // implement diff merging here, so any xpath that depends on DLC-added content will
    // fail with "matched 0 nodes" as a false positive. Detect that case and emit a
    // dedicated DLC-unverified status instead of BROKEN. Future work (task #8 in mod
    // backlog) will implement proper merge.
    if let Some(root_el) = vanilla_doc.root().children().into_iter().find_map(|c| c.element()) {
        if root_el.name().local_part() == "diff" {
            let vanilla_rel_local = vanilla_file
                .strip_prefix(vanilla_root)
                .unwrap_or(&vanilla_file)
                .display()
                .to_string();
            for (op, sel) in ops {
                out.push(Check {
                    mod_file: rel.display().to_string(),
                    vanilla_file: Some(vanilla_rel_local.clone()),
                    op: Some(op.as_str()),
                    sel: Some(sel),
                    status: Status::DlcUnverified,
                    detail: "vanilla equivalent is itself a <diff> — DLC merge not implemented, xpath assumed valid".to_string(),
                });
            }
            return Ok(());
        }
    }
    let factory = Factory::new();
    let context = sxd_xpath::Context::new();

    let vanilla_rel = vanilla_file
        .strip_prefix(vanilla_root)
        .unwrap_or(&vanilla_file)
        .display()
        .to_string();

    for (op, sel) in ops {
        let _line = locate_sel_line(&mod_text, &sel); // reserved for richer reporting later
        let (status, detail) = match factory.build(&sel) {
            Err(e) => (Status::BrokenXpath, format!("xpath compile error: {e}")),
            Ok(None) => (Status::BrokenXpath, "empty xpath".to_string()),
            Ok(Some(xpath)) => match xpath.evaluate(&context, vanilla_doc.root()) {
                Err(e) => (Status::BrokenXpath, format!("xpath eval error: {e}")),
                Ok(Value::Nodeset(ns)) => {
                    if ns.size() == 0 {
                        (Status::BrokenXpath, "matched 0 nodes".to_string())
                    } else {
                        (Status::Ok, format!("matched {} node(s)", ns.size()))
                    }
                }
                Ok(Value::Boolean(b)) => (
                    if b { Status::Ok } else { Status::BrokenXpath },
                    format!("boolean result: {b}"),
                ),
                Ok(Value::Number(n)) => (Status::Ok, format!("number result: {n}")),
                Ok(Value::String(s)) => (Status::Ok, format!("string result: {s:?}")),
            },
        };
        out.push(Check {
            mod_file: rel.display().to_string(),
            vanilla_file: Some(vanilla_rel.clone()),
            op: Some(op.as_str()),
            sel: Some(sel),
            status,
            detail,
        });
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
    let mut dlc_skip = 0usize;
    for c in &checks {
        match c.status {
            Status::Ok => ok += 1,
            Status::BrokenXpath => broken += 1,
            Status::MissingVanilla => missing += 1,
            Status::ParseError => parse_err += 1,
            Status::DlcUnverified => dlc_skip += 1,
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
        "Summary: ok={ok}  broken={broken}  missing-vanilla={missing}  parse-errors={parse_err}  dlc-skipped={dlc_skip}"
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
