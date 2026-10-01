//! A small parser for the ninja build file format.
//!
//! Every build system that emits ninja (CMake, Meson, GN, ...) can be imported
//! this way: let the generator run until `build.ninja` exists, then turn each
//! ninja edge into an individually scheduled and cached step.
//!
//! Supported: variables, rules, build edges (explicit/implicit/order-only inputs,
//! implicit outputs, validations), `default`, `pool`, `include`, `subninja`,
//! escapes and line continuations.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use fs_err as fs;

use anyhow::{Context, bail};

#[derive(Debug, Clone)]
enum Piece {
    Lit(String),
    Var(String),
}

#[derive(Debug, Clone, Default)]
struct EvalString(Vec<Piece>);

impl EvalString {
    fn eval(&self, lookup: &mut dyn FnMut(&str) -> String) -> String {
        let mut s = String::new();
        for p in &self.0 {
            match p {
                Piece::Lit(l) => s.push_str(l),
                Piece::Var(v) => s.push_str(&lookup(v)),
            }
        }
        s
    }
}

#[derive(Debug, Default)]
struct Scope {
    parent: Option<usize>,
    vars: HashMap<String, String>,
    rules: HashMap<String, HashMap<String, EvalString>>,
}

#[derive(Debug)]
struct RawEdge {
    scope: usize,
    rule: String,
    outputs: Vec<String>,
    implicit_outputs: Vec<String>,
    inputs: Vec<String>,
    implicit_inputs: Vec<String>,
    order_only: Vec<String>,
    bindings: HashMap<String, String>,
}

/// A fully evaluated ninja build edge. Paths are as written in the ninja file
/// (usually relative to the build directory).
#[derive(Debug, Clone)]
pub struct Edge {
    pub rule: String,
    pub outputs: Vec<String>,
    pub implicit_outputs: Vec<String>,
    pub inputs: Vec<String>,
    pub implicit_inputs: Vec<String>,
    pub order_only: Vec<String>,
    pub command: String,
    pub description: String,
    pub depfile: Option<String>,
    pub deps: Option<String>,
    pub pool: Option<String>,
    pub rspfile: Option<(String, String)>,
    pub generator: bool,
    pub restat: bool,
    /// For `deps = msvc`: the prefix of `/showIncludes` lines.
    pub msvc_deps_prefix: Option<String>,
}

impl Edge {
    pub fn is_phony(&self) -> bool {
        self.rule == "phony"
    }

    pub fn all_outputs(&self) -> impl Iterator<Item = &String> {
        self.outputs.iter().chain(&self.implicit_outputs)
    }

    pub fn all_inputs(&self) -> impl Iterator<Item = &String> {
        self.inputs
            .iter()
            .chain(&self.implicit_inputs)
            .chain(&self.order_only)
    }
}

#[derive(Debug, Default)]
pub struct Graph {
    pub edges: Vec<Edge>,
    pub defaults: Vec<String>,
    /// output path -> edge index
    pub producer: HashMap<String, usize>,
}

impl Graph {
    /// Parse `build.ninja` (paths in `include` statements are resolved relative to `build_dir`).
    pub fn load(build_dir: &Path, file: &str) -> anyhow::Result<Graph> {
        let mut p = Parser {
            build_dir: build_dir.to_path_buf(),
            scopes: vec![Scope::default()],
            edges: Vec::new(),
            defaults: Vec::new(),
        };
        p.parse_file(&build_dir.join(file), 0)?;
        p.finish()
    }

    /// Resolve a target through phony edges to the set of non-phony edges needed
    /// to build `targets` (transitively), in no particular order.
    pub fn closure(&self, targets: &[String]) -> Vec<usize> {
        let mut seen_paths = HashSet::new();
        let mut seen_edges = HashSet::new();
        let mut stack: Vec<&str> = targets.iter().map(String::as_str).collect();
        let mut result = Vec::new();
        while let Some(path) = stack.pop() {
            if !seen_paths.insert(path) {
                continue;
            }
            let Some(&idx) = self.producer.get(path) else {
                continue; // a source file
            };
            if !seen_edges.insert(idx) {
                continue;
            }
            let edge = &self.edges[idx];
            if !edge.is_phony() {
                result.push(idx);
            }
            stack.extend(edge.all_inputs().map(String::as_str));
        }
        result
    }

    /// The default targets (`default` statements, or all root outputs).
    pub fn default_targets(&self) -> Vec<String> {
        if !self.defaults.is_empty() {
            return self.defaults.clone();
        }
        let consumed: HashSet<&String> = self.edges.iter().flat_map(|e| e.all_inputs()).collect();
        self.edges
            .iter()
            .flat_map(|e| e.all_outputs())
            .filter(|o| !consumed.contains(o))
            .cloned()
            .collect()
    }

    /// Non-phony edges that produce `path`'s inputs, looking through phony edges.
    pub fn producing_edges(&self, paths: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<usize> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack: Vec<String> = paths.into_iter().map(|p| p.as_ref().to_string()).collect();
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            if let Some(&idx) = self.producer.get(&p) {
                let e = &self.edges[idx];
                if e.is_phony() {
                    stack.extend(e.all_inputs().cloned());
                } else if !out.contains(&idx) {
                    out.push(idx);
                }
            }
        }
        out
    }
}

struct Parser {
    build_dir: PathBuf,
    scopes: Vec<Scope>,
    edges: Vec<RawEdge>,
    defaults: Vec<EvalString>,
}

/// Split the file into logical lines, joining `$\n` continuations.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '$' => match chars.peek() {
                Some('\n') => {
                    chars.next();
                    while chars.peek() == Some(&' ') {
                        chars.next();
                    }
                }
                Some('\r') => {
                    chars.next();
                    chars.next_if_eq(&'\n');
                    while chars.peek() == Some(&' ') {
                        chars.next();
                    }
                }
                Some(_) => {
                    cur.push('$');
                    cur.push(chars.next().unwrap());
                }
                None => cur.push('$'),
            },
            '\n' => lines.push(std::mem::take(&mut cur)),
            '\r' => {}
            _ => cur.push(c),
        }
    }
    lines.push(cur);
    lines
}

fn is_var_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Parse text into an [`EvalString`]. With `path_mode`, stops at unescaped
/// spaces, `:` and `|`; returns the number of bytes consumed.
fn parse_eval(s: &str, path_mode: bool) -> anyhow::Result<(EvalString, usize)> {
    let mut pieces = Vec::new();
    let mut lit = String::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut consumed_bytes = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if path_mode && (c == ' ' || c == ':' || c == '|') {
            break;
        }
        if c == '$' {
            let Some(&n) = bytes.get(i + 1) else {
                bail!("trailing `$` in `{s}`");
            };
            match n {
                '$' | ' ' | ':' => {
                    lit.push(n);
                    i += 2;
                    consumed_bytes += 2;
                }
                '{' => {
                    let end = bytes[i + 2..]
                        .iter()
                        .position(|&c| c == '}')
                        .with_context(|| format!("unterminated `${{` in `{s}`"))?;
                    let name: String = bytes[i + 2..i + 2 + end].iter().collect();
                    if !lit.is_empty() {
                        pieces.push(Piece::Lit(std::mem::take(&mut lit)));
                    }
                    consumed_bytes += name.len() + 3;
                    pieces.push(Piece::Var(name));
                    i += end + 3;
                }
                c if is_var_char(c) => {
                    let mut j = i + 1;
                    while j < bytes.len() && is_var_char(bytes[j]) {
                        j += 1;
                    }
                    let name: String = bytes[i + 1..j].iter().collect();
                    if !lit.is_empty() {
                        pieces.push(Piece::Lit(std::mem::take(&mut lit)));
                    }
                    consumed_bytes += name.len() + 1;
                    pieces.push(Piece::Var(name));
                    i = j;
                }
                _ => bail!("bad `$` escape in `{s}`"),
            }
        } else {
            lit.push(c);
            consumed_bytes += c.len_utf8();
            i += 1;
        }
    }
    if !lit.is_empty() {
        pieces.push(Piece::Lit(lit));
    }
    Ok((EvalString(pieces), consumed_bytes))
}

#[derive(Debug)]
enum Tok {
    Path(EvalString),
    Colon,
    Pipe,
    Pipe2,
    PipeAt,
}

fn tokenize_paths(s: &str) -> anyhow::Result<Vec<Tok>> {
    let mut toks = Vec::new();
    let mut rest = s;
    loop {
        rest = rest.trim_start_matches(' ');
        if rest.is_empty() {
            break;
        }
        if let Some(r) = rest.strip_prefix("||") {
            toks.push(Tok::Pipe2);
            rest = r;
        } else if let Some(r) = rest.strip_prefix("|@") {
            toks.push(Tok::PipeAt);
            rest = r;
        } else if let Some(r) = rest.strip_prefix('|') {
            toks.push(Tok::Pipe);
            rest = r;
        } else if let Some(r) = rest.strip_prefix(':') {
            toks.push(Tok::Colon);
            rest = r;
        } else {
            let (es, n) = parse_eval(rest, true)?;
            toks.push(Tok::Path(es));
            rest = &rest[n..];
        }
    }
    Ok(toks)
}

impl Parser {
    fn lookup_scope(&self, mut scope: usize, name: &str) -> String {
        loop {
            if let Some(v) = self.scopes[scope].vars.get(name) {
                return v.clone();
            }
            match self.scopes[scope].parent {
                Some(p) => scope = p,
                None => return String::new(),
            }
        }
    }

    fn find_rule(&self, mut scope: usize, name: &str) -> Option<&HashMap<String, EvalString>> {
        loop {
            if let Some(r) = self.scopes[scope].rules.get(name) {
                return Some(r);
            }
            scope = self.scopes[scope].parent?;
        }
    }

    fn eval_in_scope(&self, scope: usize, es: &EvalString) -> String {
        es.eval(&mut |v| self.lookup_scope(scope, v))
    }

    fn parse_file(&mut self, path: &Path, scope: usize) -> anyhow::Result<()> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let lines = logical_lines(&text);
        let mut i = 0;
        // Collect the indented binding lines following a statement.
        let take_bindings = |i: &mut usize| -> anyhow::Result<Vec<(String, EvalString)>> {
            let mut out = Vec::new();
            while *i < lines.len() {
                let l = &lines[*i];
                if !l.starts_with(' ') && !l.starts_with('\t') {
                    break;
                }
                let t = l.trim_start();
                *i += 1;
                if t.is_empty() || t.starts_with('#') {
                    continue;
                }
                let (k, v) = t
                    .split_once('=')
                    .with_context(|| format!("expected binding, got `{t}`"))?;
                out.push((k.trim().to_string(), parse_eval(v.trim_start(), false)?.0));
            }
            Ok(out)
        };

        while i < lines.len() {
            let line = lines[i].clone();
            i += 1;
            let t = line.trim_start();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let (kw, rest) = t.split_once(' ').unwrap_or((t, ""));
            match kw {
                "rule" => {
                    let name = rest.trim().to_string();
                    let bindings = take_bindings(&mut i)?.into_iter().collect();
                    self.scopes[scope].rules.insert(name, bindings);
                }
                "pool" => {
                    take_bindings(&mut i)?;
                }
                "default" => {
                    for tok in tokenize_paths(rest)? {
                        if let Tok::Path(p) = tok {
                            self.defaults.push(p);
                        }
                    }
                }
                "include" | "subninja" => {
                    let (es, _) = parse_eval(rest.trim(), false)?;
                    let file = self.eval_in_scope(scope, &es);
                    let child = if kw == "subninja" {
                        self.scopes.push(Scope {
                            parent: Some(scope),
                            ..Default::default()
                        });
                        self.scopes.len() - 1
                    } else {
                        scope
                    };
                    let p = self.build_dir.join(file);
                    self.parse_file(&p, child)?;
                }
                "build" => {
                    let toks = tokenize_paths(rest)?;
                    let bindings = take_bindings(&mut i)?;
                    self.parse_build(scope, toks, bindings)?;
                }
                _ => {
                    // variable assignment
                    let (k, v) = t
                        .split_once('=')
                        .with_context(|| format!("unexpected line `{t}`"))?;
                    let (es, _) = parse_eval(v.trim_start(), false)?;
                    let value = self.eval_in_scope(scope, &es);
                    self.scopes[scope].vars.insert(k.trim().to_string(), value);
                }
            }
        }
        Ok(())
    }

    fn parse_build(
        &mut self,
        scope: usize,
        toks: Vec<Tok>,
        bindings: Vec<(String, EvalString)>,
    ) -> anyhow::Result<()> {
        // Edge bindings are evaluated against the enclosing scope.
        let mut edge_vars = HashMap::new();
        for (k, v) in bindings {
            let val = v.eval(&mut |name| {
                edge_vars
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| self.lookup_scope(scope, name))
            });
            edge_vars.insert(k, val);
        }
        let eval_path = |es: &EvalString| {
            es.eval(&mut |name| {
                edge_vars
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| self.lookup_scope(scope, name))
            })
        };

        let mut outs = Vec::new();
        let mut implicit_outs = Vec::new();
        let mut ins = Vec::new();
        let mut implicit = Vec::new();
        let mut order_only = Vec::new();
        let mut rule = None;
        // section: 0 outs, 1 implicit outs, 2 rule, 3 ins, 4 implicit, 5 order-only, 6 validations
        let mut section = 0;
        for tok in toks {
            match tok {
                Tok::Colon => section = 2,
                Tok::Pipe if section == 0 => section = 1,
                Tok::Pipe => section = 4,
                Tok::Pipe2 => section = 5,
                Tok::PipeAt => section = 6,
                Tok::Path(p) => {
                    let v = eval_path(&p);
                    match section {
                        0 => outs.push(v),
                        1 => implicit_outs.push(v),
                        2 => {
                            rule = Some(v);
                            section = 3;
                        }
                        3 => ins.push(v),
                        4 => implicit.push(v),
                        5 => order_only.push(v),
                        _ => {} // validations are not build inputs
                    }
                }
            }
        }
        let rule = rule.context("build statement without a rule")?;
        self.edges.push(RawEdge {
            scope,
            rule,
            outputs: outs,
            implicit_outputs: implicit_outs,
            inputs: ins,
            implicit_inputs: implicit,
            order_only,
            bindings: edge_vars,
        });
        Ok(())
    }

    fn finish(self) -> anyhow::Result<Graph> {
        let mut graph = Graph::default();
        for raw in &self.edges {
            let edge = if raw.rule == "phony" {
                Edge {
                    rule: "phony".into(),
                    outputs: raw.outputs.clone(),
                    implicit_outputs: raw.implicit_outputs.clone(),
                    inputs: raw.inputs.clone(),
                    implicit_inputs: raw.implicit_inputs.clone(),
                    order_only: raw.order_only.clone(),
                    command: String::new(),
                    description: String::new(),
                    depfile: None,
                    deps: None,
                    pool: None,
                    rspfile: None,
                    generator: false,
                    restat: false,
                    msvc_deps_prefix: None,
                }
            } else {
                let rule = self
                    .find_rule(raw.scope, &raw.rule)
                    .with_context(|| format!("unknown rule `{}`", raw.rule))?;
                let get = |key: &str| -> String { self.eval_edge_var(raw, rule, key, 0) };
                let opt = |key: &str| Some(get(key)).filter(|s| !s.is_empty());
                Edge {
                    rule: raw.rule.clone(),
                    outputs: raw.outputs.clone(),
                    implicit_outputs: raw.implicit_outputs.clone(),
                    inputs: raw.inputs.clone(),
                    implicit_inputs: raw.implicit_inputs.clone(),
                    order_only: raw.order_only.clone(),
                    command: get("command"),
                    description: get("description"),
                    depfile: opt("depfile"),
                    deps: opt("deps"),
                    pool: opt("pool"),
                    rspfile: opt("rspfile").map(|f| (f, get("rspfile_content"))),
                    generator: opt("generator").is_some(),
                    restat: opt("restat").is_some(),
                    msvc_deps_prefix: opt("msvc_deps_prefix"),
                }
            };
            graph.edges.push(edge);
        }
        for (idx, e) in graph.edges.iter().enumerate() {
            for o in e.all_outputs() {
                graph.producer.insert(o.clone(), idx);
            }
        }
        graph.defaults = self
            .defaults
            .iter()
            .map(|d| self.eval_in_scope(0, d))
            .collect();
        Ok(graph)
    }

    fn eval_edge_var(
        &self,
        raw: &RawEdge,
        rule: &HashMap<String, EvalString>,
        key: &str,
        depth: usize,
    ) -> String {
        if depth > 32 {
            return String::new();
        }
        match key {
            "in" => return join_shell(&raw.inputs, " "),
            "in_newline" => return join_shell(&raw.inputs, "\n"),
            "out" => return join_shell(&raw.outputs, " "),
            _ => {}
        }
        if let Some(v) = raw.bindings.get(key) {
            return v.clone();
        }
        if let Some(es) = rule.get(key) {
            return es.eval(&mut |name| self.eval_edge_var(raw, rule, name, depth + 1));
        }
        self.lookup_scope(raw.scope, key)
    }
}

/// `$in`/`$out` expansion, escaped like ninja does for the platform shell.
fn join_shell(paths: &[String], sep: &str) -> String {
    paths
        .iter()
        .map(|p| {
            if cfg!(windows) {
                // ninja on Windows: quote only paths with spaces (Win32 rules).
                if p.contains(' ') || p.contains('"') {
                    format!("\"{}\"", p.replace('"', "\\\""))
                } else {
                    p.clone()
                }
            } else if p
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_+-./=:,@%".contains(&b))
            {
                p.clone()
            } else {
                format!("'{}'", p.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(sep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_graph() {
        let dir =
            std::env::temp_dir().join(format!("rattler-build-ninja-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("rules.ninja"),
            "rule CC\n  command = cc $FLAGS -MD -MF $DEP_FILE -c $in -o $out\n  depfile = $DEP_FILE\n  deps = gcc\n",
        )
        .unwrap();
        fs::write(
            dir.join("build.ninja"),
            "cflags = -O2\ninclude rules.ninja\n\
             build a.o: CC a$ b.c | gen.h || order\n  FLAGS = $cflags -DX\n  DEP_FILE = a.o.d\n\
             build all: phony a.o\ndefault all\n",
        )
        .unwrap();
        let g = Graph::load(&dir, "build.ninja").unwrap();
        let e = &g.edges[0];
        // $in is escaped for the platform shell, like ninja does.
        let quoted = if cfg!(windows) {
            "\"a b.c\""
        } else {
            "'a b.c'"
        };
        assert_eq!(
            e.command,
            format!("cc -O2 -DX -MD -MF a.o.d -c {quoted} -o a.o")
        );
        assert_eq!(e.inputs, vec!["a b.c"]);
        assert_eq!(e.implicit_inputs, vec!["gen.h"]);
        assert_eq!(e.order_only, vec!["order"]);
        assert_eq!(e.depfile.as_deref(), Some("a.o.d"));
        assert_eq!(g.closure(&g.default_targets()), vec![0]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
