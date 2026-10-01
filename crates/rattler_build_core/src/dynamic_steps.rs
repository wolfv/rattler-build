//! Generated sub-steps: a build step can write a list of steps to
//! `$RATTLER_BUILD_STEPS_FILE`; they run after it, in parallel, before the
//! next authored step.
//!
//! Each sub-step is a single command with declared inputs and outputs (for
//! example one ninja edge: `rattler-build steps from-ninja build`). Its cache
//! key is the command, working directory, environment and the content of its
//! inputs (plus the headers its depfile names), with the build's
//! checkout-specific paths (`$SRC_DIR`, `$PREFIX`, ...) replaced by
//! placeholders. Outputs are stored in a content-addressed cache shared by all
//! builds, and restored on a hit, so a rebuild, or another package with the
//! same sources, only runs what changed.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use fs_err as fs;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Environment variables that differ between otherwise identical runs.
const VOLATILE_ENV: &[&str] = &[
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "SOURCE_DATE_EPOCH",
    "OUTPUT_FILE",
    "RATTLER_BUILD_OUTPUT_FILE",
    "RATTLER_BUILD_STEP_CACHE",
    "RATTLER_BUILD_STEPS_FILE",
];

/// Bump to invalidate every cached sub-step after a semantic change.
const CACHE_VERSION: &str = "rattler-build-substep-v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepsFile {
    /// Default working directory of the steps.
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// The environment the steps run with. Generators should record their
    /// own (activated) environment here; default: the step's base environment.
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    steps: Vec<SubStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubStep {
    name: String,
    run: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Files whose content the step depends on.
    #[serde(default)]
    inputs: Vec<PathBuf>,
    /// Files that must exist first (ordering only).
    #[serde(default)]
    after: Vec<PathBuf>,
    #[serde(default)]
    outputs: Vec<PathBuf>,
    #[serde(default)]
    depends_on: Vec<String>,
    /// Make-style depfile written by the command (discovered inputs).
    #[serde(default)]
    depfile: Option<PathBuf>,
    /// (path, content) written before the command runs.
    #[serde(default)]
    rspfile: Option<(PathBuf, String)>,
    #[serde(default = "default_true")]
    cache: bool,
}

fn default_true() -> bool {
    true
}

/// A step resolved against its working directory.
struct Resolved {
    name: String,
    run: String,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    inputs: Vec<PathBuf>,
    after: Vec<PathBuf>,
    depends_on: Vec<String>,
    outputs: Vec<PathBuf>,
    depfile: Option<PathBuf>,
    rspfile: Option<(PathBuf, String)>,
    cache: bool,
}

/// How the generated steps of one build step run.
pub struct Options {
    /// Base environment when the steps file has none.
    pub env: BTreeMap<String, String>,
    /// Default working directory (the generating step's).
    pub cwd: PathBuf,
    /// Shared cache directory (`None`: no caching).
    pub cache_dir: Option<PathBuf>,
    /// How many steps run at once.
    pub jobs: usize,
}

/// What a set of generated steps did.
#[derive(Debug, Default, Clone, Copy)]
pub struct Summary {
    /// Generated steps.
    pub total: usize,
    /// Steps whose command ran.
    pub executed: usize,
    /// Steps whose outputs were restored from the cache.
    pub cached: usize,
}

/// Where step `index` may write its generated steps.
pub fn steps_file(work_dir: &Path, index: usize) -> PathBuf {
    crate::recipe_patch::output_file(work_dir, index).with_extension("steps.json")
}

/// The shared cache directory: `$RATTLER_BUILD_ACTION_CACHE` (`off` disables
/// it), else `<rattler cache>/rattler-build-actions`.
pub fn default_cache_dir() -> Option<PathBuf> {
    match std::env::var("RATTLER_BUILD_ACTION_CACHE") {
        Ok(v) if v == "off" => None,
        Ok(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => rattler_cache::default_cache_dir()
            .ok()
            .map(|d| d.join("rattler-build-actions")),
    }
}

/// Checkout-specific path prefixes -> placeholders (longest first).
#[derive(Clone)]
struct PathMap(Vec<(String, String)>);

impl PathMap {
    fn from_env(env: &BTreeMap<String, String>) -> Self {
        let mut v = Vec::new();
        for var in ["SRC_DIR", "PREFIX", "BUILD_PREFIX", "RECIPE_DIR"] {
            if let Some(p) = env.get(var).filter(|p| !p.is_empty()) {
                v.push((p.clone(), format!("@@{var}@@")));
            }
        }
        // The build directory (`.../rattler-build_<name>_<timestamp>`) holds
        // the work dir and the prefixes.
        if let Some(root) = env
            .get("SRC_DIR")
            .and_then(|s| Path::new(s).parent())
            .map(|p| p.to_string_lossy().into_owned())
        {
            v.push((root, "@@BUILD_ROOT@@".into()));
        }
        v.sort_by_key(|(p, _)| std::cmp::Reverse(p.len()));
        v.dedup_by(|a, b| a.0 == b.0);
        PathMap(v)
    }

    fn norm(&self, s: &str) -> String {
        let mut s = s.to_string();
        for (real, placeholder) in &self.0 {
            if s.contains(real.as_str()) {
                s = s.replace(real.as_str(), placeholder);
            }
        }
        s
    }

    fn denorm(&self, s: &str) -> String {
        let mut s = s.to_string();
        for (real, placeholder) in &self.0 {
            if s.contains(placeholder.as_str()) {
                s = s.replace(placeholder.as_str(), real);
            }
        }
        s
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Content digests of files, computed once per run.
#[derive(Default)]
struct Digests(Mutex<HashMap<PathBuf, Option<String>>>);

impl Digests {
    fn file(&self, path: &Path) -> std::io::Result<Option<String>> {
        if let Some(d) = self.0.lock().unwrap().get(path) {
            return Ok(d.clone());
        }
        let d = match fs::read(path) {
            Ok(bytes) => Some(hex(&Sha256::digest(&bytes))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) if path.is_dir() => {
                let _ = e;
                Some("<dir>".into())
            }
            Err(e) => return Err(e),
        };
        self.0.lock().unwrap().insert(path.to_path_buf(), d.clone());
        Ok(d)
    }

    fn forget(&self, path: &Path) {
        self.0.lock().unwrap().remove(path);
    }
}

#[derive(Serialize, Deserialize)]
struct CachedOutput {
    path: String,
    sha256: String,
    executable: bool,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    /// Inputs found in the depfile: (normalized path, sha256).
    discovered: Vec<(String, String)>,
    outputs: Vec<CachedOutput>,
}

struct Cache {
    dir: PathBuf,
    map: PathMap,
    digests: Digests,
}

impl Cache {
    fn key(&self, step: &Resolved) -> std::io::Result<String> {
        let m = &self.map;
        let mut h = Sha256::new();
        let mut field = |s: &str| {
            h.update((s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        };
        field(CACHE_VERSION);
        field(&m.norm(&step.run));
        field(&m.norm(&step.cwd.to_string_lossy()));
        for (k, v) in &step.env {
            if !VOLATILE_ENV.contains(&k.as_str()) {
                field(k);
                field(&m.norm(v));
            }
        }
        if let Some((path, content)) = &step.rspfile {
            field(&m.norm(&path.to_string_lossy()));
            field(&m.norm(content));
        }
        let mut inputs: Vec<(String, &PathBuf)> = step
            .inputs
            .iter()
            .map(|p| (m.norm(&p.to_string_lossy()), p))
            .collect();
        inputs.sort();
        for (normalized, path) in inputs {
            field(&normalized);
            field(self.digests.file(path)?.as_deref().unwrap_or("<absent>"));
        }
        for o in &step.outputs {
            field(&m.norm(&o.to_string_lossy()));
        }
        Ok(hex(&h.finalize()))
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir.join(&key[..2]).join(format!("{key}.json"))
    }

    fn blob(&self, sha: &str) -> PathBuf {
        self.dir.join("cas").join(&sha[..2]).join(sha)
    }

    /// Restore the outputs of a previous run with the same key.
    fn restore(&self, key: &str) -> std::io::Result<bool> {
        let Ok(bytes) = fs::read(self.entry_path(key)) else {
            return Ok(false);
        };
        let Ok(entry) = serde_json::from_slice::<CacheEntry>(&bytes) else {
            return Ok(false);
        };
        for (path, sha) in &entry.discovered {
            let current = self.digests.file(Path::new(&self.map.denorm(path)))?;
            if current.as_deref() != Some(sha.as_str()) {
                return Ok(false);
            }
        }
        if entry.outputs.iter().any(|o| !self.blob(&o.sha256).exists()) {
            return Ok(false);
        }
        for o in &entry.outputs {
            let path = PathBuf::from(self.map.denorm(&o.path));
            if self.digests.file(&path)?.as_deref() != Some(o.sha256.as_str()) {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let _ = fs::remove_file(&path);
                fs::copy(self.blob(&o.sha256), &path)?;
                set_executable(&path, o.executable)?;
                self.digests.forget(&path);
            }
        }
        Ok(true)
    }

    fn store(&self, key: &str, step: &Resolved) -> std::io::Result<()> {
        let declared: HashSet<&PathBuf> = step.inputs.iter().chain(&step.outputs).collect();
        let mut discovered = Vec::new();
        if let Some(depfile) = &step.depfile
            && let Ok(text) = fs::read_to_string(depfile)
        {
            for dep in parse_depfile(&text) {
                let path = step.cwd.join(dep);
                if declared.contains(&path) {
                    continue;
                }
                if let Some(d) = self.digests.file(&path)? {
                    discovered.push((self.map.norm(&path.to_string_lossy()), d));
                }
            }
        }
        let mut outputs = Vec::new();
        for o in &step.outputs {
            self.digests.forget(o);
            let Some(sha) = self.digests.file(o)? else {
                // An output the command didn't write: don't cache the step.
                return Ok(());
            };
            let blob = self.blob(&sha);
            if !blob.exists() {
                fs::create_dir_all(blob.parent().unwrap())?;
                let tmp = blob.with_extension("tmp");
                fs::copy(o, &tmp)?;
                fs::rename(&tmp, &blob)?;
            }
            outputs.push(CachedOutput {
                path: self.map.norm(&o.to_string_lossy()),
                sha256: sha,
                executable: is_executable(o),
            });
        }
        let path = self.entry_path(key);
        fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("tmp");
        fs::write(
            &tmp,
            serde_json::to_vec(&CacheEntry {
                discovered,
                outputs,
            })?,
        )?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_: &Path) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_executable(_: &Path, _: bool) -> std::io::Result<()> {
    Ok(())
}

/// The prerequisites of a make-style depfile (`out: in1 in2 \` ...).
fn parse_depfile(text: &str) -> Vec<String> {
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut deps = Vec::new();
    for line in joined.lines() {
        // The target ends at the first `: ` (a Windows drive letter has no space after it).
        let Some(idx) = line
            .find(": ")
            .or_else(|| line.strip_suffix(':').map(|l| l.len()))
        else {
            continue;
        };
        let mut current = String::new();
        let mut chars = line[idx + 1..].chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&' ') => {
                    current.push(' ');
                    chars.next();
                }
                '$' if chars.peek() == Some(&'$') => {
                    current.push('$');
                    chars.next();
                }
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        deps.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            deps.push(current);
        }
    }
    deps
}

async fn run_command(step: &Resolved) -> std::io::Result<std::process::Output> {
    if let Some((path, content)) = &step.rspfile {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content)?;
    }
    for o in &step.outputs {
        if let Some(parent) = o.parent() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd.exe");
        c.arg("/d").arg("/c").arg(&step.run);
        c
    } else {
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c").arg(&step.run);
        c
    };
    cmd.current_dir(&step.cwd)
        .env_clear()
        .envs(&step.env)
        .stdin(std::process::Stdio::null())
        .output()
        .await
}

/// Run the steps a build step generated (no-op when it didn't write any).
pub async fn run_generated(steps_file: &Path, opts: Options) -> std::io::Result<Option<Summary>> {
    let text = match fs::read_to_string(steps_file) {
        Ok(t) if !t.trim().is_empty() => t,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let file: StepsFile = serde_json::from_str(&text).map_err(|e| {
        std::io::Error::other(format!(
            "invalid generated steps in {}: {e}",
            steps_file.display()
        ))
    })?;
    let base_env = file.env.unwrap_or(opts.env);
    let default_cwd = file.cwd.unwrap_or(opts.cwd);
    let steps: Vec<Resolved> = file
        .steps
        .into_iter()
        .map(|s| {
            let cwd = default_cwd.join(s.cwd.unwrap_or_default());
            let mut env = base_env.clone();
            env.extend(s.env);
            Resolved {
                name: s.name,
                run: s.run,
                inputs: s.inputs.iter().map(|p| cwd.join(p)).collect(),
                after: s.after.iter().map(|p| cwd.join(p)).collect(),
                depends_on: s.depends_on,
                outputs: s.outputs.iter().map(|p| cwd.join(p)).collect(),
                depfile: s.depfile.map(|p| cwd.join(p)),
                rspfile: s.rspfile.map(|(p, c)| (cwd.join(p), c)),
                cache: s.cache,
                env,
                cwd,
            }
        })
        .collect();

    let producer: HashMap<&PathBuf, usize> = steps
        .iter()
        .enumerate()
        .flat_map(|(i, s)| s.outputs.iter().map(move |o| (o, i)))
        .collect();
    let by_name: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.as_str(), i))
        .collect();
    let mut deps: Vec<HashSet<usize>> = vec![HashSet::new(); steps.len()];
    for (i, s) in steps.iter().enumerate() {
        for p in s.inputs.iter().chain(&s.after) {
            if let Some(&j) = producer.get(p)
                && j != i
            {
                deps[i].insert(j);
            }
        }
        for n in &s.depends_on {
            let j = *by_name.get(n.as_str()).ok_or_else(|| {
                std::io::Error::other(format!(
                    "generated step `{}` depends on unknown step `{n}`",
                    s.name
                ))
            })?;
            deps[i].insert(j);
        }
    }

    let total = steps.len();
    let map = PathMap::from_env(&base_env);
    let cache = Arc::new(opts.cache_dir.map(|dir| Cache {
        dir,
        map,
        digests: Digests::default(),
    }));
    let steps: Arc<Vec<Resolved>> = Arc::new(steps);
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); total];
    let mut waiting: Vec<usize> = deps.iter().map(HashSet::len).collect();
    for (i, d) in deps.iter().enumerate() {
        for &j in d {
            dependents[j].push(i);
        }
    }
    let mut ready: Vec<usize> = (0..total).filter(|&i| waiting[i] == 0).rev().collect();
    let mut running = tokio::task::JoinSet::new();
    let mut summary = Summary {
        total,
        ..Default::default()
    };
    let mut done = 0;
    let jobs = opts.jobs.max(1);
    while done < total {
        while running.len() < jobs
            && let Some(i) = ready.pop()
        {
            let (steps, cache) = (steps.clone(), cache.clone());
            running.spawn(async move {
                let step = &steps[i];
                let key = match cache.as_ref() {
                    Some(c) if step.cache => Some(c.key(step)?),
                    _ => None,
                };
                if let (Some(c), Some(k)) = (cache.as_ref(), &key)
                    && c.restore(k)?
                {
                    return Ok::<_, std::io::Error>((i, true));
                }
                let out = run_command(step).await?;
                if !out.status.success() {
                    return Err(std::io::Error::other(format!(
                        "generated step `{}` failed ({})\n  command: {}\n{}{}",
                        step.name,
                        out.status,
                        step.run,
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    )));
                }
                if let (Some(c), Some(k)) = (cache.as_ref(), &key) {
                    c.store(k, step)?;
                }
                Ok((i, false))
            });
        }
        let Some(joined) = running.join_next().await else {
            return Err(std::io::Error::other(
                "generated steps have a dependency cycle",
            ));
        };
        let (i, hit) = joined.map_err(std::io::Error::other)??;
        done += 1;
        if hit {
            summary.cached += 1;
        } else {
            summary.executed += 1;
        }
        tracing::info!(
            "[{done}/{total}] {}{}",
            steps[i].name,
            if hit { " (cached)" } else { "" }
        );
        for &d in &dependents[i] {
            waiting[d] -= 1;
            if waiting[d] == 0 {
                ready.push(d);
            }
        }
    }
    Ok(Some(summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depfile_prerequisites() {
        let text = "a.o: a.c /usr/include/stdio.h \\\n  my\\ header.h $$x.h\n";
        assert_eq!(
            parse_depfile(text),
            ["a.c", "/usr/include/stdio.h", "my header.h", "$x.h"]
        );
    }
}
