//! Workspace indexing: walk the repository, parse analyzable sources, resolve
//! module imports, and record every reason analysis had to skip something.
//!
//! Every skip is recorded as a [`BlindSpot`] — the analysis is never called
//! "complete" while relevant inputs were excluded.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use sha2::{Digest, Sha256};

use crate::cache::ParseCache;
use crate::config::{glob_match, Config};
use crate::evidence::{BlindSpot, BlindSpotKind};
use crate::gitrepo::GitRepo;
use crate::lang::{self, ParsedFile, RefKind};

/// Resolved binding of a local name to a target module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingKind {
    /// `import { a as b }` → named export `a` (None = default export).
    Named(Option<String>),
    /// `import * as ns` / `const ns = require('./m')` → every export.
    Namespace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingTarget {
    pub file: String,
    pub kind: BindingKind,
}

/// A resolved module-level edge between two files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportEdge {
    pub from: String,
    pub to: String,
    pub specifier: String,
    pub line: u32,
    /// True when the edge came from `import()` or `require()`.
    pub dynamic: bool,
}

/// The indexed state of a repository at one point in time.
pub struct Workspace {
    /// Current content of every analyzable file (repo-relative, sorted).
    pub files: BTreeMap<String, ParsedFile>,
    /// Old versions of changed files (for deleted/modified symbol mapping).
    pub old_files: BTreeMap<String, ParsedFile>,
    /// Resolved import bindings: file → local name → target.
    pub bindings: HashMap<String, HashMap<String, BindingTarget>>,
    /// Resolved module edges.
    pub edges: Vec<ImportEdge>,
    /// Export tables: file → exported name → binding.
    pub exports: HashMap<String, BTreeMap<String, crate::lang::ExportBinding>>,
    /// Bare (external) module specifiers encountered.
    pub external_modules: BTreeSet<String>,
    /// Every reason something was not analyzed.
    pub blind_spots: Vec<BlindSpot>,
    /// Cache hit/miss counters for diagnostics.
    pub cache_hits: usize,
    pub cache_misses: usize,
    /// Non-literal dynamic imports / computed dispatch, per file.
    pub runtime_dispatch: BTreeMap<String, Vec<u32>>,
}

impl Workspace {
    /// Index the repository: walk, filter, parse, then resolve modules.
    pub fn build(
        repo: &GitRepo,
        cfg: &Config,
        cache: &mut ParseCache,
        extra_sources: &[(String, Option<String>, Option<String>)],
    ) -> Result<Self, String> {
        // extra_sources: (path, old_content, new_content) for changed files —
        // ensures both sides are parsed even when a side is not on disk.
        let mut ws = Workspace {
            files: BTreeMap::new(),
            old_files: BTreeMap::new(),
            bindings: HashMap::new(),
            edges: Vec::new(),
            exports: HashMap::new(),
            external_modules: BTreeSet::new(),
            blind_spots: Vec::new(),
            cache_hits: 0,
            cache_misses: 0,
            runtime_dispatch: BTreeMap::new(),
        };

        ws.index_worktree(repo, cfg, cache);
        ws.index_extra(repo, cfg, cache, extra_sources);
        ws.resolve_modules();
        ws.collect_dispatch_blind_spots();
        Ok(ws)
    }

    fn index_worktree(&mut self, repo: &GitRepo, cfg: &Config, cache: &mut ParseCache) {
        let root = repo.root().to_path_buf();
        let mut excluded_dirs: BTreeMap<String, usize> = BTreeMap::new();
        let mut excluded_files: BTreeMap<String, usize> = BTreeMap::new();
        let mut unsupported: BTreeMap<String, usize> = BTreeMap::new();
        let mut oversized: Vec<String> = Vec::new();
        let mut binaries: Vec<String> = Vec::new();
        let mut generated: Vec<String> = Vec::new();
        let mut walked: Vec<(String, std::path::PathBuf)> = Vec::new();
        collect_dir(
            &root,
            &root,
            cfg,
            &mut walked,
            &mut excluded_dirs,
            &mut excluded_files,
        );

        for (rel, path) in walked {
            let ext = rel.rsplit('.').next().map(|e| e.to_ascii_lowercase());
            let supported = ext
                .as_deref()
                .map(|e| lang::SUPPORTED_EXTENSIONS.contains(&e))
                .unwrap_or(false);
            if !supported {
                // Only note extensions that look like source code so reports
                // are not flooded with data files.
                if let Some(e) = &ext {
                    if LOOKS_LIKE_SOURCE.contains(&e.as_str()) {
                        *unsupported.entry(e.clone()).or_insert(0) += 1;
                    }
                }
                continue;
            }

            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if bytes.contains(&0) {
                binaries.push(rel);
                continue;
            }
            if bytes.len() as u64 > cfg.analysis.max_file_bytes {
                oversized.push(rel);
                continue;
            }
            let text = match String::from_utf8(bytes) {
                Ok(t) => t,
                Err(_) => {
                    binaries.push(rel);
                    continue;
                }
            };
            if is_generated(&text, &cfg.analysis.generated_markers) {
                generated.push(rel);
                continue;
            }

            let hash = content_hash(&text);
            match cache.get(&rel, &hash) {
                Some(parsed) => {
                    self.cache_hits += 1;
                    self.files.insert(rel, parsed);
                }
                None => {
                    self.cache_misses += 1;
                    if let Some(adapter) = lang::adapter_for(&rel) {
                        let parsed = adapter.parse(&rel, &text);
                        cache.put(rel.clone(), hash, parsed.clone());
                        self.files.insert(rel, parsed);
                    }
                }
            }
        }

        // Aggregate blind spots (capped, deduplicated by construction).
        for (dir, count) in excluded_dirs {
            self.blind_spots.push(BlindSpot {
                id: format!("ignored-dir:{dir}"),
                kind: BlindSpotKind::IgnoredDirectory,
                file: Some(dir.clone()),
                reason: format!(
                    "directory excluded by configuration ({count} analyzable file(s) inside)"
                ),
                impact: "changes inside this directory are not analyzed".into(),
            });
        }
        for (file, count) in excluded_files {
            self.blind_spots.push(BlindSpot {
                id: format!("ignored-file:{file}"),
                kind: BlindSpotKind::IgnoredDirectory,
                file: Some(file.clone()),
                reason: format!("file excluded by configuration ({count} match(es))"),
                impact: "this file is not analyzed".into(),
            });
        }
        for (ext, count) in unsupported {
            self.blind_spots.push(BlindSpot {
                id: format!("unsupported:{ext}"),
                kind: BlindSpotKind::UnsupportedLanguage,
                file: None,
                reason: format!("{count} .{ext} file(s) present; no adapter for this language"),
                impact: "symbols and relationships in these files are invisible to the analysis"
                    .into(),
            });
        }
        for f in oversized.iter().take(10) {
            self.blind_spots.push(BlindSpot {
                id: format!("oversized:{f}"),
                kind: BlindSpotKind::OversizedFile,
                file: Some(f.clone()),
                reason: format!(
                    "file exceeds max_file_bytes ({})",
                    cfg.analysis.max_file_bytes
                ),
                impact: "contents not parsed; relationships through this file are unknown".into(),
            });
        }
        for f in binaries.iter().take(10) {
            self.blind_spots.push(BlindSpot {
                id: format!("binary:{f}"),
                kind: BlindSpotKind::BinaryFile,
                file: Some(f.clone()),
                reason: "file is binary or not valid UTF-8".into(),
                impact: "contents not parsed".into(),
            });
        }
        for f in generated.iter().take(10) {
            self.blind_spots.push(BlindSpot {
                id: format!("generated:{f}"),
                kind: BlindSpotKind::GeneratedCode,
                file: Some(f.clone()),
                reason: "file carries a generated-code marker".into(),
                impact:
                    "generated files are skipped so generated churn does not drown real signal; \
                          changes to their *inputs* are what matters"
                        .into(),
            });
        }
    }

    fn index_extra(
        &mut self,
        repo: &GitRepo,
        cfg: &Config,
        cache: &mut ParseCache,
        extra: &[(String, Option<String>, Option<String>)],
    ) {
        for (path, old_content, new_content) in extra {
            // Current (new) side: prefer disk (authoritative for the worktree),
            // fall back to provided content (staged/revision scopes).
            if !self.files.contains_key(path) {
                let text = new_content.clone().or_else(|| {
                    repo.read_worktree_file(path)
                        .ok()
                        .flatten()
                        .and_then(|b| String::from_utf8(b).ok())
                });
                if let Some(text) = text {
                    let hash = content_hash(&text);
                    match cache.get(path, &hash) {
                        Some(parsed) => {
                            self.cache_hits += 1;
                            self.files.insert(path.clone(), parsed);
                        }
                        None => {
                            self.cache_misses += 1;
                            if let Some(adapter) = lang::adapter_for(path) {
                                let parsed = adapter.parse(path, &text);
                                cache.put(path.clone(), hash, parsed.clone());
                                self.files.insert(path.clone(), parsed);
                            }
                        }
                    }
                }
            }
            // Old side (needed for deleted symbols / removed lines).
            if let Some(old) = old_content {
                if !old.is_empty() && !self.old_files.contains_key(path) {
                    if let Some(adapter) = lang::adapter_for(path) {
                        let parsed = adapter.parse(path, old);
                        self.old_files.insert(path.clone(), parsed);
                    }
                }
            }
            let _ = cfg;
        }
    }

    /// Resolve import specifiers and build the binding table.
    fn resolve_modules(&mut self) {
        let known: HashSet<String> = self.files.keys().cloned().collect();

        // Named bindings whose target export could not (yet) be confirmed.
        // Validated only after pass 2 builds every file's export table,
        // otherwise a normal `export function` looks "missing".
        let mut pending_suspects: Vec<(String, String, String, String, u32)> = Vec::new();

        // Pass 1: collect bindings and edges per file.
        let paths: Vec<String> = self.files.keys().cloned().collect();
        for path in &paths {
            let parsed = &self.files[path];
            let mut local: HashMap<String, BindingTarget> = HashMap::new();
            // Unresolved imports are collected here and turned into blind
            // spots after the loop: `parsed` borrows `self.files`, so we
            // cannot call `&mut self` methods while iterating its refs.
            let mut unresolved: Vec<(String, u32)> = Vec::new();

            for r in &parsed.refs {
                match r.kind {
                    RefKind::Import | RefKind::NamespaceImport => {
                        let Some(spec) = &r.module else { continue };
                        match resolve_specifier(path, spec, &known) {
                            Resolved::File(target) => {
                                let kind = if r.kind == RefKind::NamespaceImport {
                                    BindingKind::Namespace
                                } else {
                                    BindingKind::Named(r.imported.clone())
                                };
                                local.insert(
                                    r.name.clone(),
                                    BindingTarget {
                                        file: target.clone(),
                                        kind,
                                    },
                                );
                                self.edges.push(ImportEdge {
                                    from: path.clone(),
                                    to: target,
                                    specifier: spec.clone(),
                                    line: r.line,
                                    dynamic: false,
                                });
                            }
                            Resolved::External => {
                                self.external_modules.insert(spec.clone());
                            }
                            Resolved::Unresolved => {
                                unresolved.push((spec.clone(), r.line));
                            }
                        }
                    }
                    RefKind::ReExport => {
                        let Some(spec) = &r.module else { continue };
                        match resolve_specifier(path, spec, &known) {
                            Resolved::File(target) => {
                                self.edges.push(ImportEdge {
                                    from: path.clone(),
                                    to: target,
                                    specifier: spec.clone(),
                                    line: r.line,
                                    dynamic: false,
                                });
                                // A re-export also acts as a binding for `export * from`.
                                if r.name == "*" {
                                    if let Some(m) = self.exports.get_mut(path) {
                                        m.insert(
                                            "*".into(),
                                            crate::lang::ExportBinding {
                                                exported: "*".into(),
                                                local: None,
                                                from: Some(spec.clone()),
                                            },
                                        );
                                    }
                                } else {
                                    let name = r.name.clone();
                                    let imported = r.imported.clone();
                                    self.exports
                                        .entry(path.clone())
                                        .or_default()
                                        .entry(name)
                                        .or_insert(crate::lang::ExportBinding {
                                            exported: r.name.clone(),
                                            local: imported,
                                            from: Some(spec.clone()),
                                        });
                                }
                            }
                            Resolved::External => {
                                self.external_modules.insert(spec.clone());
                            }
                            Resolved::Unresolved => {
                                unresolved.push((spec.clone(), r.line));
                            }
                        }
                    }
                    RefKind::DynamicImport | RefKind::Require => {
                        if !r.literal_specifier {
                            continue; // non-literal: recorded as dispatch elsewhere
                        }
                        let Some(spec) = &r.module else { continue };
                        match resolve_specifier(path, spec, &known) {
                            Resolved::File(target) => {
                                // `const x = require('./m')` binds the namespace;
                                // destructure bindings are recorded as Import refs.
                                if r.kind == RefKind::Require && !r.name.is_empty() {
                                    local.insert(
                                        r.name.clone(),
                                        BindingTarget {
                                            file: target.clone(),
                                            kind: BindingKind::Namespace,
                                        },
                                    );
                                }
                                self.edges.push(ImportEdge {
                                    from: path.clone(),
                                    to: target,
                                    specifier: spec.clone(),
                                    line: r.line,
                                    dynamic: true,
                                });
                            }
                            Resolved::External => {
                                self.external_modules.insert(spec.clone());
                            }
                            Resolved::Unresolved => {
                                unresolved.push((spec.clone(), r.line));
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Turn collected unresolved imports into blind spots now that the
            // immutable borrow of `self.files` is no longer live.
            for (spec, line) in unresolved {
                self.blind_spots.push(BlindSpot {
                    id: format!("unresolved-import:{path}:{line}"),
                    kind: BlindSpotKind::UnresolvedImport,
                    file: Some(path.clone()),
                    reason: format!(
                        "module specifier `{spec}` does not resolve to a file in the repository"
                    ),
                    impact: "relationships through this import are unknown".into(),
                });
            }

            // Validate named bindings against the target's export table.
            // Validation is deferred until after pass 2 (below) because the
            // export tables do not exist yet during pass 1.
            for (local_name, target) in &local {
                if let BindingKind::Named(Some(imported)) = &target.kind {
                    if let Some(line) = parsed
                        .refs
                        .iter()
                        .find(|r| r.kind == RefKind::Import && &r.name == local_name)
                        .map(|r| r.line)
                    {
                        pending_suspects.push((
                            path.clone(),
                            local_name.clone(),
                            imported.clone(),
                            target.file.clone(),
                            line,
                        ));
                    }
                }
            }

            self.bindings.insert(path.clone(), local);
        }

        // Pass 2: export tables from each file's own declarations.
        for (path, parsed) in &self.files {
            let table = self.exports.entry(path.clone()).or_default();
            for e in &parsed.exports {
                table.entry(e.exported.clone()).or_insert_with(|| e.clone());
            }
        }

        // Pass 3: now that every export table exists, validate the named
        // bindings collected in pass 1. A binding whose target really does not
        // export the imported name is incomplete symbol resolution.
        for (importer, local_name, imported, target_file, line) in pending_suspects {
            if self.export_exists(&target_file, &imported) {
                continue;
            }
            self.blind_spots.push(BlindSpot {
                id: format!("unresolved-binding:{importer}:{line}:{local_name}"),
                kind: BlindSpotKind::IncompleteSymbolResolution,
                file: Some(importer.clone()),
                reason: format!(
                    "imports `{imported}` from `{target_file}`, but that file does not export it \
                     (as far as static analysis can see)"
                ),
                impact: format!("calls to `{local_name}` cannot be attributed to a declaration"),
            });
        }

        if !self.external_modules.is_empty() {
            let mut list: Vec<&String> = self.external_modules.iter().collect();
            list.sort();
            list.truncate(20);
            let names: Vec<String> = list.into_iter().cloned().collect();
            self.blind_spots.push(BlindSpot {
                id: "external-modules".into(),
                kind: BlindSpotKind::UnresolvedImport,
                file: None,
                reason: format!(
                    "{} external package import(s) (e.g. {}) resolve outside the repository",
                    self.external_modules.len(),
                    names.join(", ")
                ),
                impact: "symbols inside external packages are not analyzed; \
                         relationships stop at the package boundary"
                    .into(),
            });
        }
    }

    /// Whether `file` exports `name` (following a bounded re-export chain).
    pub fn export_exists(&self, file: &str, name: &str) -> bool {
        self.resolve_export(file, name, 0).is_some()
    }

    /// Resolve an exported name to its declaring file and local name.
    pub fn resolve_export(
        &self,
        file: &str,
        name: &str,
        depth: u8,
    ) -> Option<(String, Option<String>)> {
        if depth > 5 {
            return None; // re-export chain too deep or cyclic
        }
        let table = self.exports.get(file)?;
        if let Some(b) = table.get(name) {
            match &b.from {
                None => {
                    let local = b.local.clone().unwrap_or_else(|| b.exported.clone());
                    return Some((file.to_string(), Some(local)));
                }
                Some(spec) => {
                    let known: HashSet<String> = self.files.keys().cloned().collect();
                    if let Resolved::File(target) = resolve_specifier(file, spec, &known) {
                        let next = b.local.clone().unwrap_or_else(|| b.exported.clone());
                        if next == "*" {
                            // `export * from`: look for the name deeper.
                            return self.resolve_export(&target, name, depth + 1);
                        }
                        return self.resolve_export(&target, &next, depth + 1);
                    }
                    return None;
                }
            }
        }
        // Star re-export fallback: check tables that star-re-export into file.
        if let Some(b) = table.get("*") {
            if let Some(spec) = &b.from {
                let known: HashSet<String> = self.files.keys().cloned().collect();
                if let Resolved::File(target) = resolve_specifier(file, spec, &known) {
                    return self.resolve_export(&target, name, depth + 1);
                }
            }
        }
        None
    }

    fn collect_dispatch_blind_spots(&mut self) {
        let mut files: Vec<&String> = self.runtime_dispatch.keys().collect();
        files.sort();
        if files.is_empty() {
            return;
        }
        let sample: Vec<String> = files.iter().take(5).map(|s| s.to_string()).collect();
        let total: usize = self.runtime_dispatch.values().map(|v| v.len()).sum();
        self.blind_spots.push(BlindSpot {
            id: "runtime-dispatch".into(),
            kind: BlindSpotKind::DynamicDispatch,
            file: None,
            reason: format!(
                "{total} dynamic dispatch site(s) (eval / computed access / non-literal import) \
                 in {} file(s): {}",
                files.len(),
                sample.join(", ")
            ),
            impact: "targets reached only at runtime cannot be resolved statically".into(),
        });
    }

    /// Record a runtime-dispatch site found in a file.
    pub fn note_dispatch(&mut self, file: &str, line: u32) {
        self.runtime_dispatch
            .entry(file.to_string())
            .or_default()
            .push(line);
    }

    /// Find a declaration by name across the workspace (exact, then suffix).
    pub fn find_symbols(&self, name: &str) -> Vec<(String, &crate::lang::Decl)> {
        let mut out = Vec::new();
        for (path, parsed) in &self.files {
            for d in &parsed.decls {
                if d.name == name {
                    out.push((path.clone(), d));
                }
            }
        }
        if out.is_empty() {
            for (path, parsed) in &self.files {
                for d in &parsed.decls {
                    if d.name.rsplit('.').next() == Some(name)
                        || d.name.ends_with(&format!(".{name}"))
                    {
                        out.push((path.clone(), d));
                    }
                }
            }
        }
        out
    }
}

const LOOKS_LIKE_SOURCE: &[&str] = &[
    "py", "rb", "go", "rs", "java", "c", "cpp", "cc", "h", "hpp", "cs", "php", "swift", "kt",
    "scala", "lua", "sh", "vue", "svelte", "dart", "ex", "exs", "erl", "hs", "ml", "r", "jl", "pl",
    "ps1", "sql",
];

fn content_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn is_generated(text: &str, markers: &[String]) -> bool {
    let head: String = text.chars().take(4096).collect();
    markers.iter().any(|m| head.contains(m.as_str()))
}

/// Recursively collect analyzable files, honoring exclusion patterns.
fn collect_dir(
    root: &std::path::Path,
    dir: &std::path::Path,
    cfg: &Config,
    out: &mut Vec<(String, std::path::PathBuf)>,
    excluded_dirs: &mut BTreeMap<String, usize>,
    excluded_files: &mut BTreeMap<String, usize>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" || name == ".aftermath" {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if path.is_dir() {
            let as_pattern = format!("{rel}/");
            let excluded = cfg
                .analysis
                .exclude
                .iter()
                .any(|p| glob_match(p, &rel) || glob_match(p, &format!("{rel}/**")));
            if excluded {
                let count = count_analyzable(&path, 0);
                if count > 0 {
                    *excluded_dirs.entry(rel.clone()).or_insert(0) += count;
                }
                let _ = as_pattern;
                continue;
            }
            collect_dir(root, &path, cfg, out, excluded_dirs, excluded_files);
        } else {
            if cfg.analysis.exclude.iter().any(|p| glob_match(p, &rel)) {
                let ext = rel.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
                if lang::SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
                    *excluded_files.entry(rel.clone()).or_insert(0) += 1;
                }
                continue;
            }
            out.push((rel, path));
        }
    }
}

/// Bounded count of analyzable files under `dir` (for blind-spot reporting).
fn count_analyzable(dir: &std::path::Path, depth: u8) -> usize {
    if depth > 4 {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            n += count_analyzable(&path, depth + 1);
        } else {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if lang::SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
                n += 1;
            }
        }
    }
    n
}

/// Result of resolving a module specifier.
pub enum Resolved {
    File(String),
    External,
    Unresolved,
}

/// Resolve `spec` as imported from `from_file` against the known file set.
///
/// Handles `./x`, `../x`, absolute-ish `/x`, TypeScript's `.js` → `.ts`
/// convention, and directory `index.*` resolution. Bare specifiers are
/// external by definition.
pub fn resolve_specifier(from_file: &str, spec: &str, known: &HashSet<String>) -> Resolved {
    if spec.is_empty() {
        return Resolved::Unresolved;
    }
    let is_relative = spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/');
    if !is_relative {
        return Resolved::External;
    }
    let base = from_file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let joined = if spec.starts_with('/') {
        spec.trim_start_matches('/').to_string()
    } else if base.is_empty() {
        spec.to_string()
    } else {
        format!("{base}/{spec}")
    };
    let normalized = normalize(&joined);

    let mut candidates: Vec<String> = Vec::new();
    candidates.push(normalized.clone());
    // TypeScript emits `.js` specifiers for `.ts` sources.
    if let Some(stem) = normalized.strip_suffix(".js") {
        for ext in [".ts", ".tsx"] {
            candidates.push(format!("{stem}{ext}"));
        }
    }
    if normalized.ends_with(".mjs") {
        candidates.push(normalized.replace(".mjs", ".ts"));
    }
    if !normalized.contains('.') || is_dirish(&normalized) {
        for ext in lang::SUPPORTED_EXTENSIONS {
            candidates.push(format!("{normalized}.{ext}"));
            candidates.push(format!("{normalized}/index.{ext}"));
        }
    }
    for c in &candidates {
        if known.contains(c) {
            return Resolved::File(c.clone());
        }
    }
    Resolved::Unresolved
}

fn is_dirish(path: &str) -> bool {
    // Heuristic: specifiers without a file extension are directories.
    let last = path.rsplit('/').next().unwrap_or("");
    !last.contains('.')
}

/// Normalize `a/b/../c` → `a/c` on `/`-separated paths.
fn normalize(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    stack.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specifier_resolution_handles_extensions_and_index() {
        let mut known = HashSet::new();
        known.insert("src/utils.ts".to_string());
        known.insert("src/lib/index.js".to_string());
        known.insert("src/a.ts".to_string());

        assert!(matches!(
            resolve_specifier("src/a.ts", "./utils", &known),
            Resolved::File(f) if f == "src/utils.ts"
        ));
        // TS convention: `./utils.js` resolves to utils.ts
        assert!(matches!(
            resolve_specifier("src/a.ts", "./utils.js", &known),
            Resolved::File(f) if f == "src/utils.ts"
        ));
        assert!(matches!(
            resolve_specifier("src/a.ts", "./lib", &known),
            Resolved::File(f) if f == "src/lib/index.js"
        ));
        assert!(matches!(
            resolve_specifier("src/a.ts", "react", &known),
            Resolved::External
        ));
        assert!(matches!(
            resolve_specifier("src/a.ts", "./missing", &known),
            Resolved::Unresolved
        ));
    }

    #[test]
    fn normalize_walks_up() {
        assert_eq!(normalize("src/a/../b/./c"), "src/b/c");
        assert_eq!(normalize("../x"), "x"); // leading .. dropped at root
        assert_eq!(normalize("./lib"), "lib");
    }

    #[test]
    fn generated_marker_detection() {
        let markers = vec!["@generated".to_string()];
        assert!(is_generated(
            "// @generated by tool\nconst x = 1;",
            &markers
        ));
        assert!(is_generated(
            "# Code generated by probe\n",
            &[
                "@generated".to_string(),
                "# Code generated by probe".to_string()
            ]
        ));
        assert!(!is_generated(
            "const x = 1; // generated at runtime",
            &markers
        ));
    }
}
