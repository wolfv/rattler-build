//! Turn a `build.ninja` (written by CMake, Meson, GN, ...) into rattler-build
//! *generated sub-steps*: one step per ninja edge, with the files it reads
//! and writes. rattler-build schedules them in parallel and caches each one by
//! the content of its inputs (see `docs/build_steps/dynamic.md`).

mod graph;

use std::collections::{BTreeMap, HashSet};

pub use graph::{Edge, Graph};
use serde::Serialize;

/// The steps file a generator writes to `$RATTLER_BUILD_STEPS_FILE`.
#[derive(Debug, Serialize)]
pub struct StepsFile {
    /// Working directory of every step (relative paths are relative to it).
    pub cwd: String,
    /// The environment the steps run with (the generator's own, i.e. the
    /// activated build environment).
    pub env: BTreeMap<String, String>,
    pub steps: Vec<GeneratedStep>,
}

#[derive(Debug, Serialize)]
pub struct GeneratedStep {
    pub name: String,
    pub run: String,
    /// Files whose content the step depends on (hashed into its cache key).
    pub inputs: Vec<String>,
    /// Files that must exist before the step runs, without being inputs
    /// (ninja's order-only dependencies, e.g. generated headers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    pub outputs: Vec<String>,
    /// Make-style depfile the command writes: the headers it read become
    /// additional inputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depfile: Option<String>,
    /// Response file written before the command runs (path, content).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rspfile: Option<(String, String)>,
}

/// The sub-steps that build `targets` (default: the graph's default targets).
/// Phony edges become ordering only; generator edges (CMake re-running
/// itself) are left out.
pub fn steps(graph: &Graph, targets: &[String]) -> Vec<GeneratedStep> {
    let targets = if targets.is_empty() {
        graph.default_targets()
    } else {
        targets.to_vec()
    };
    let mut edges = graph.closure(&targets);
    edges.sort();
    // Phony outputs aren't files: resolve them to what they stand for.
    let real = |paths: &mut dyn Iterator<Item = &String>| -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack: Vec<String> = paths.cloned().collect();
        while let Some(p) = stack.pop() {
            if !seen.insert(p.clone()) {
                continue;
            }
            match graph.producer.get(&p).map(|&i| &graph.edges[i]) {
                Some(e) if e.is_phony() => stack.extend(e.all_inputs().cloned()),
                _ => out.push(p),
            }
        }
        out.sort();
        out
    };
    edges
        .into_iter()
        .map(|i| &graph.edges[i])
        .filter(|e| !e.generator && !e.command.trim().is_empty())
        .map(|e| GeneratedStep {
            name: if e.description.trim().is_empty() {
                e.outputs.first().cloned().unwrap_or_default()
            } else {
                e.description.clone()
            },
            run: e.command.clone(),
            inputs: real(&mut e.inputs.iter().chain(&e.implicit_inputs)),
            after: real(&mut e.order_only.iter()),
            outputs: e
                .outputs
                .iter()
                .chain(&e.implicit_outputs)
                .cloned()
                .collect(),
            depfile: e.depfile.clone().filter(|d| !d.is_empty()),
            rspfile: e.rspfile.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_become_steps_with_real_inputs() {
        let dir =
            std::env::temp_dir().join(format!("rattler-build-ninja-steps-{}", std::process::id()));
        fs_err::create_dir_all(&dir).unwrap();
        fs_err::write(
            dir.join("build.ninja"),
            "rule CC\n  command = cc -c $in -o $out\n  description = CC $out\n  depfile = $out.d\n\
             rule LINK\n  command = cc $in -o $out\n\
             rule RERUN\n  command = cmake .\n  generator = 1\n\
             rule GEN\n  command = gen > $out\n\
             build gen.h: GEN\n\
             build headers: phony gen.h\n\
             build a.o: CC a.c || headers\n\
             build app: LINK a.o\n\
             build build.ninja: RERUN CMakeLists.txt\n\
             build all: phony app\ndefault all\n",
        )
        .unwrap();
        let g = Graph::load(&dir, "build.ninja").unwrap();
        let s = steps(&g, &[]);
        let names: Vec<&str> = s.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["gen.h", "CC a.o", "app"]);
        let cc = &s[1];
        assert_eq!(cc.inputs, ["a.c"]);
        // The phony `headers` is ordering on the file it stands for.
        assert_eq!(cc.after, ["gen.h"]);
        assert_eq!(cc.depfile.as_deref(), Some("a.o.d"));
        fs_err::remove_dir_all(&dir).unwrap();
    }
}
