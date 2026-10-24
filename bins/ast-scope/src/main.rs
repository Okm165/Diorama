//! AST scope analysis (fitness gate): parse every source file with the
//! compiler's own parser (`syn`) and check the shapes the doctrine keeps
//! small and auditable — function bodies, nesting depth, struct fields, enum
//! variants. The report names the largest offenders; the gate fails when one
//! exceeds the documented threshold. Test code is out of scope (fixtures may
//! be long; production code may not).
//!
//! clippy's committed 50-line / 5-arg ratchet is the first line of defence on
//! function shape; this gate adds the structural metrics clippy has no lint
//! for (nesting depth, field counts) and counts doc comments, so a function
//! whose *documentation* outgrows its body is visible too.
//!
//! Usage: `cargo run -p ast-scope -- [paths...]` (default `crates bins`).
use proc_macro2::Span;
use std::path::{Path, PathBuf};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

/// The gate's thresholds. A shape beyond these is the drift this gate
/// catches: a body past a screenful, logic nested past reading order, a
/// record with too many independent fields to hold in mind.
const MAX_LINES: usize = 60;
const MAX_DEPTH: usize = 5;
const MAX_FIELDS: usize = 20;
const MAX_VARIANTS: usize = 25;
/// How many offenders each report section lists.
const REPORT_TOP: usize = 12;

/// One production item's shape.
struct Item {
    file: String,
    kind: &'static str,
    name: String,
    lines: usize,
    depth: usize,
    size: usize,
    /// `#[cfg(test)]` subtree or `#[test]`: the suite's fixtures, out of the
    /// gate's scope.
    test: bool,
}

impl Item {
    /// The threshold this item exceeds, if any (the gate's verdict).
    fn violation(&self) -> Option<(&'static str, usize)> {
        match self.kind {
            "fn" => {
                if self.lines > MAX_LINES {
                    Some(("lines", MAX_LINES))
                } else if self.depth > MAX_DEPTH {
                    Some(("depth", MAX_DEPTH))
                } else {
                    None
                }
            }
            "struct" => (self.size > MAX_FIELDS).then_some(("fields", MAX_FIELDS)),
            "enum" => (self.size > MAX_VARIANTS).then_some(("variants", MAX_VARIANTS)),
            _ => None,
        }
    }

    fn metric(&self) -> usize {
        match self.kind {
            "fn" => self.lines,
            _ => self.size,
        }
    }

    fn is_test(&self) -> bool {
        self.test
    }
}

fn main() {
    let mut roots: Vec<String> = std::env::args().skip(1).collect();
    if roots.is_empty() {
        roots = vec!["crates".into(), "bins".into()];
    }
    let mut files = Vec::new();
    for root in &roots {
        collect(Path::new(root), &mut files);
    }
    files.sort();
    let mut items = Vec::new();
    for file in &files {
        parse_into(file, &mut items);
    }
    report(&items);
    std::process::exit(gate(&items));
}

/// Parse one file and push its production items (test modules skipped).
fn parse_into(path: &Path, items: &mut Vec<Item>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("ast-scope: cannot read {}: {e}", path.display());
            return;
        }
    };
    let parsed = match syn::parse_file(&text) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("ast-scope: parse fault in {}: {e}", path.display());
            return;
        }
    };
    let file = path.to_string_lossy().to_string();
    walk(&file, &parsed.items, false, items);
}

/// Walk items, descending into modules and impl blocks. `test` marks the
/// `#[cfg(test)]` subtree: its items are the suite's fixtures, not the
/// production shape the gate guards.
fn walk(file: &str, items: &[syn::Item], test: bool, out: &mut Vec<Item>) {
    for item in items {
        match item {
            syn::Item::Fn(f) => out.push(fn_item(file, &f.sig, &f.block, &f.attrs, test)),
            syn::Item::Struct(s) => out.push(Item {
                file: file.into(),
                kind: "struct",
                name: s.ident.to_string(),
                lines: span_lines(s.span()),
                depth: 0,
                size: s.fields.len(),
                test,
            }),
            syn::Item::Enum(e) => out.push(Item {
                file: file.into(),
                kind: "enum",
                name: e.ident.to_string(),
                lines: span_lines(e.span()),
                depth: 0,
                size: e.variants.len(),
                test,
            }),
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    walk(file, inner, test || has_test_cfg(&m.attrs), out);
                }
            }
            syn::Item::Impl(i) => walk_impl(file, i, test, out),
            _ => {}
        }
    }
}

/// The methods of one impl block, named `Type.method` so the report says
/// where the shape lives.
fn walk_impl(file: &str, i: &syn::ItemImpl, test: bool, out: &mut Vec<Item>) {
    let type_name = match &*i.self_ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "?".into()),
        _ => "?".into(),
    };
    for item in &i.items {
        if let syn::ImplItem::Fn(f) = item {
            let mut item = fn_item(file, &f.sig, &f.block, &f.attrs, test);
            item.name = format!("{type_name}.{}", f.sig.ident);
            out.push(item);
        }
    }
}

/// One function's shape: span lines (docs included), nesting depth, and
/// whether it is a test.
fn fn_item(
    file: &str,
    sig: &syn::Signature,
    block: &syn::Block,
    attrs: &[syn::Attribute],
    test: bool,
) -> Item {
    let mut body = Body::default();
    body.visit_block(block);
    Item {
        file: file.into(),
        kind: "fn",
        name: sig.ident.to_string(),
        lines: span_lines(block.span()),
        depth: body.max_depth,
        size: sig.inputs.len(),
        test: test || has_test_attr(attrs),
    }
}

/// The visitor behind `lines`/`depth`: every block-like expression is one
/// level of nesting.
#[derive(Default)]
struct Body {
    depth: usize,
    max_depth: usize,
}

impl Body {
    /// Enter one nesting level for the duration of `f`.
    fn enter<F: FnOnce(&mut Self)>(&mut self, f: F) {
        self.depth += 1;
        self.max_depth = self.max_depth.max(self.depth);
        f(self);
        self.depth -= 1;
    }
}

impl<'ast> Visit<'ast> for Body {
    fn visit_expr_if(&mut self, n: &'ast syn::ExprIf) {
        self.enter(|s| visit::visit_expr_if(s, n));
    }
    fn visit_expr_for_loop(&mut self, n: &'ast syn::ExprForLoop) {
        self.enter(|s| visit::visit_expr_for_loop(s, n));
    }
    fn visit_expr_while(&mut self, n: &'ast syn::ExprWhile) {
        self.enter(|s| visit::visit_expr_while(s, n));
    }
    fn visit_expr_loop(&mut self, n: &'ast syn::ExprLoop) {
        self.enter(|s| visit::visit_expr_loop(s, n));
    }
    fn visit_expr_match(&mut self, n: &'ast syn::ExprMatch) {
        self.enter(|s| visit::visit_expr_match(s, n));
    }
    fn visit_expr_closure(&mut self, n: &'ast syn::ExprClosure) {
        self.enter(|s| visit::visit_expr_closure(s, n));
    }
}

/// The report: the largest production items per kind (the analysis half).
fn report(items: &[Item]) {
    let prod: Vec<&Item> = items.iter().filter(|i| !i.is_test()).collect();
    println!(
        "ast-scope: {} production items ({} functions, {} structs, {} enums)",
        prod.len(),
        count(&prod, "fn"),
        count(&prod, "struct"),
        count(&prod, "enum")
    );
    section("largest functions (lines, docs included)", &prod, "fn");
    section("deepest functions (nesting)", &prod, "fn-depth");
    section("largest records (fields/variants)", &prod, "record");
}

/// One report section, sorted by the section's metric.
fn section(title: &str, prod: &[&Item], kind: &str) {
    let mut rows: Vec<&Item> = match kind {
        "fn" => prod.iter().copied().filter(|i| i.kind == "fn").collect(),
        "fn-depth" => prod.iter().copied().filter(|i| i.kind == "fn").collect(),
        _ => prod.iter().copied().filter(|i| i.kind != "fn").collect(),
    };
    match kind {
        "fn" => rows.sort_by_key(|i| std::cmp::Reverse(i.lines)),
        "fn-depth" => rows.sort_by_key(|i| std::cmp::Reverse(i.depth)),
        _ => rows.sort_by_key(|i| std::cmp::Reverse(i.metric())),
    }
    println!("\n{title}:");
    for item in rows.iter().take(REPORT_TOP) {
        let metric = match kind {
            "fn-depth" => format!("depth {}", item.depth),
            "fn" => format!("{} lines, depth {}", item.lines, item.depth),
            _ => format!("{}", item.size),
        };
        println!("  {metric:>18}  {}:{}", item.file, item.name);
    }
}

fn count(prod: &[&Item], kind: &str) -> usize {
    prod.iter().filter(|i| i.kind == kind).count()
}

/// The gate: every production item inside its threshold; violations are
/// named (file, item, metric, limit) and the exit code is the verdict.
fn gate(items: &[Item]) -> i32 {
    let mut bad = 0;
    for item in items.iter().filter(|i| !i.is_test()) {
        if let Some((metric, limit)) = item.violation() {
            println!(
                "VIOLATION: {}:{} is {} {} (limit {limit})",
                item.file,
                item.name,
                item.metric(),
                metric
            );
            bad = 1;
        }
    }
    if bad == 0 {
        println!(
            "\nast-scope: OK (fn <= {MAX_LINES} lines / depth <= {MAX_DEPTH}, \
             struct <= {MAX_FIELDS} fields, enum <= {MAX_VARIANTS} variants)"
        );
    }
    bad
}

fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(is_test_attr)
}

fn has_test_cfg(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(is_test_attr)
}

/// `#[test]` or `#[cfg(test)]` — the two markers the suite uses.
fn is_test_attr(a: &syn::Attribute) -> bool {
    if a.path().is_ident("test") {
        return true;
    }
    a.path().is_ident("cfg")
        && a.parse_args::<syn::Ident>()
            .map(|i| i == "test")
            .unwrap_or(false)
}

fn span_lines(span: Span) -> usize {
    span.end().line.saturating_sub(span.start().line) + 1
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if path.is_dir() {
            if name != "target" && name != ".git" {
                collect(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}
