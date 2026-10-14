//! Impact analysis: which symbols are affected by a change, with what evidence,
//! and with what confidence.
//!
//! Resolution rules (documented here because they *are* the product):
//!
//! | reference shape | resolution | confidence |
//! |---|---|---|
//! | same-file call to the changed declaration | names match, single declaration in file | confirmed relationship |
//! | call through an import binding | binding resolves to the changed declaration | confirmed relationship |
//! | `this.method()` inside the declaring class | class scope matches | confirmed relationship |
//! | `ns.method()` where `ns` is a namespace import of the declaring module | module + export resolve | confirmed relationship |
//! | `extends` / type reference resolved through imports | binding resolves | confirmed relationship |
//! | any of the above where only the *name* matches | no structural link found | possible impact |
//! | member call without type information (`obj.method()`) | receiver type unknown | possible impact |
//!
//! Nothing here ever claims that a name match is a proven dependency, and
//! nothing claims that an *absence* of matches proves nothing depends on the
//! changed symbol — that belongs in the Unknowns report.

use std::collections::{HashMap, HashSet};

use crate::evidence::{
    AffectedSymbol, Confidence, Evidence, EvidenceKind, EvidenceLocation, Relationship, SymbolKind,
    SymbolRef,
};
use crate::lang::{Decl, Ref, RefKind};
use crate::workspace::{resolve_specifier, BindingKind, ImportEdge, Resolved, Workspace};

/// One resolved reference from `file` to (potentially) a symbol.
pub struct RefMatch {
    pub relationship: Relationship,
    pub confidence: Confidence,
    pub evidence: Vec<Evidence>,
}

/// `(target file, imported name)` → list of `(importer, line, local binding)`.
type ImporterIndex = HashMap<(String, String), Vec<(String, u32, String)>>;

const MAX_AFFECTED: usize = 500;
const MAX_EVIDENCE_PER_MATCH: usize = 8;

pub struct ImpactEngine<'a> {
    ws: &'a Workspace,
    /// simple name → call sites (file, ref)
    calls: HashMap<&'a str, Vec<(String, &'a Ref)>>,
    /// property name → member calls / new / namespace reads
    members: HashMap<&'a str, Vec<(String, &'a Ref)>>,
    /// type name → type references
    types: HashMap<&'a str, Vec<(String, &'a Ref)>>,
    /// base name → inheritance references
    inherits: HashMap<&'a str, Vec<(String, &'a Ref)>>,
    importers: ImporterIndex,
}

impl<'a> ImpactEngine<'a> {
    pub fn new(ws: &'a Workspace) -> Self {
        let mut calls: HashMap<&str, Vec<(String, &Ref)>> = HashMap::new();
        let mut members: HashMap<&str, Vec<(String, &Ref)>> = HashMap::new();
        let mut types: HashMap<&str, Vec<(String, &Ref)>> = HashMap::new();
        let mut inherits: HashMap<&str, Vec<(String, &Ref)>> = HashMap::new();
        let mut importers: ImporterIndex = HashMap::new();

        for (path, parsed) in &ws.files {
            for r in &parsed.refs {
                match r.kind {
                    RefKind::Call => calls
                        .entry(r.name.as_str())
                        .or_default()
                        .push((path.clone(), r)),
                    RefKind::MemberCall
                    | RefKind::New
                    | RefKind::NamespaceMember
                    | RefKind::ComputedMember => {
                        if let Some(p) = &r.property {
                            members
                                .entry(p.as_str())
                                .or_default()
                                .push((path.clone(), r));
                        }
                        // `new Foo()` also carries the name directly.
                        if r.kind == RefKind::New && r.property.is_none() {
                            members
                                .entry(r.name.as_str())
                                .or_default()
                                .push((path.clone(), r));
                        }
                    }
                    RefKind::TypeUse => types
                        .entry(r.name.as_str())
                        .or_default()
                        .push((path.clone(), r)),
                    RefKind::Inherit => inherits
                        .entry(r.name.as_str())
                        .or_default()
                        .push((path.clone(), r)),
                    RefKind::Import | RefKind::NamespaceImport => {
                        let local = &r.name;
                        if !local.is_empty() {
                            if let Some(target) = ws.bindings.get(path).and_then(|m| m.get(local)) {
                                let key = match &target.kind {
                                    BindingKind::Named(Some(n)) => n.clone(),
                                    BindingKind::Named(None) => "default".to_string(),
                                    BindingKind::Namespace => "*".to_string(),
                                };
                                importers
                                    .entry((target.file.clone(), key))
                                    .or_default()
                                    .push((path.clone(), r.line, local.clone()));
                            }
                        }
                    }
                    RefKind::DynamicImport | RefKind::Require => {
                        // Literal dynamic imports act as module edges; they
                        // are indexed as namespace-style importers.
                        // (`if let` tuple pattern: edition 2021 has no let-chains.)
                        if let (true, Some(spec)) = (r.literal_specifier, &r.module) {
                            let name = &r.name;
                            let known: HashSet<String> = ws.files.keys().cloned().collect();
                            if let Resolved::File(target) = resolve_specifier(path, spec, &known) {
                                let key = if r.kind == RefKind::Require && !name.is_empty() {
                                    "*".to_string()
                                } else {
                                    "default".to_string()
                                };
                                importers.entry((target, key)).or_default().push((
                                    path.clone(),
                                    r.line,
                                    name.clone(),
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Self {
            ws,
            calls,
            members,
            types,
            inherits,
            importers,
        }
    }

    // -- helpers -------------------------------------------------------------

    fn simple(name: &str) -> &str {
        name.rsplit('.').next().unwrap_or(name)
    }

    fn symbol_of(file: &str, decl: &Decl) -> SymbolRef {
        SymbolRef::new(file, decl.name.clone(), decl.kind)
    }

    fn decl_in(&self, file: &str, name: &str) -> Vec<&Decl> {
        self.ws
            .files
            .get(file)
            .map(|p| p.decls.iter().filter(|d| d.name == name).collect())
            .unwrap_or_default()
    }

    /// Resolve a local import binding to the symbol it points at.
    /// Returns `None` for namespace bindings (module-level targets).
    fn resolve_binding(&self, importer: &str, local: &str) -> Option<SymbolRef> {
        let target = self.ws.bindings.get(importer)?.get(local)?;
        let (file, imported) = match &target.kind {
            BindingKind::Named(n) => (
                target.file.clone(),
                n.clone().unwrap_or_else(|| "default".into()),
            ),
            BindingKind::Namespace => return None,
        };
        let (tfile, tlocal) = self.ws.resolve_export(&file, &imported, 0)?;
        let local_name = tlocal?;
        let decl = self.ws.files.get(&tfile)?.find_decl(&local_name)?;
        Some(Self::symbol_of(&tfile, decl))
    }

    /// The module a binding points at (for namespace and member checks).
    fn binding_target_file(&self, importer: &str, local: &str) -> Option<(String, BindingKind)> {
        let t = self.ws.bindings.get(importer)?.get(local)?;
        Some((t.file.clone(), t.kind.clone()))
    }

    /// Resolve an object expression to a module file when it is bound to an
    /// import (namespace or named), for member-call attribution.
    fn object_module(&self, file: &str, object: &str) -> Option<(String, BindingKind)> {
        self.binding_target_file(file, object)
    }

    // -- core matching -------------------------------------------------------

    /// Try to connect one reference to the changed symbol.
    pub fn match_ref(&self, sym: &SymbolRef, file: &str, r: &Ref) -> Option<RefMatch> {
        match r.kind {
            RefKind::Call => self.match_call(sym, file, r),
            RefKind::MemberCall | RefKind::New | RefKind::NamespaceMember => {
                self.match_member(sym, file, r)
            }
            RefKind::ComputedMember => None, // receiver unknowable; blind spot
            RefKind::TypeUse => self.match_type(sym, file, r),
            RefKind::Inherit => self.match_inherit(sym, file, r),
            RefKind::Import
            | RefKind::ReExport
            | RefKind::NamespaceImport
            | RefKind::DynamicImport
            | RefKind::Require => self.match_import(sym, file, r),
            RefKind::ExportSpecifier => None,
            // `eval(...)` / computed dispatch: target unknowable statically.
            RefKind::RuntimeDispatch => None,
        }
    }

    fn match_call(&self, sym: &SymbolRef, file: &str, r: &Ref) -> Option<RefMatch> {
        // 1. Same-file call to a locally declared symbol.
        if file == sym.file {
            let decls = self.decl_in(file, &r.name);
            if decls.len() == 1 {
                let d = decls[0];
                if d.name == sym.name {
                    return Some(RefMatch {
                        relationship: Relationship::DirectCaller,
                        confidence: Confidence::ConfirmedRelationship,
                        evidence: vec![Evidence::new(
                            EvidenceKind::CallSite,
                            EvidenceLocation::line(file, r.line).with_snippet(r.snippet.clone()),
                            format!(
                                "call to `{}` declared at {}:{}",
                                r.name, sym.file, d.start_line
                            ),
                        )],
                    });
                }
            } else if decls.len() > 1 && decls.iter().any(|d| d.name == sym.name) {
                // Same name declared multiple times: attribution ambiguous.
                return Some(RefMatch {
                    relationship: Relationship::DirectCaller,
                    confidence: Confidence::PossibleImpact,
                    evidence: vec![Evidence::new(
                        EvidenceKind::NameMatch,
                        EvidenceLocation::line(file, r.line),
                        format!(
                            "`{}` is declared {} times in this file; the call cannot be \
                             attributed to one declaration with certainty",
                            r.name,
                            decls.len()
                        ),
                    )],
                });
            }
        }

        // 2. Call through an import binding.
        if let Some(resolved) = self.resolve_binding(file, &r.name) {
            if resolved == *sym {
                let mut evidence = vec![Evidence::new(
                    EvidenceKind::CallSite,
                    EvidenceLocation::line(file, r.line).with_snippet(r.snippet.clone()),
                    format!("call to `{}`", r.name),
                )];
                if let Some(line) = self.import_line(file, &r.name) {
                    evidence.push(Evidence::new(
                        EvidenceKind::ImportEdge,
                        EvidenceLocation::line(file, line),
                        format!("`{}` is imported here", r.name),
                    ));
                }
                return Some(RefMatch {
                    relationship: Relationship::DirectCaller,
                    confidence: Confidence::ConfirmedRelationship,
                    evidence,
                });
            }
        }

        // 3. Name-only match: possible, never confirmed.
        if r.name == Self::simple(&sym.name) {
            return Some(RefMatch {
                relationship: Relationship::DirectCaller,
                confidence: Confidence::PossibleImpact,
                evidence: vec![Evidence::new(
                    EvidenceKind::NameMatch,
                    EvidenceLocation::line(file, r.line).with_snippet(r.snippet.clone()),
                    format!(
                        "call to `{}` matches the name of `{}` but no import or local \
                         declaration connects them",
                        r.name,
                        sym.id()
                    ),
                )],
            });
        }
        None
    }

    fn match_member(&self, sym: &SymbolRef, file: &str, r: &Ref) -> Option<RefMatch> {
        let prop = r.property.as_deref().unwrap_or(&r.name);
        if prop != Self::simple(&sym.name) {
            return None;
        }
        let object = r.object.as_deref().unwrap_or("");

        // `this.foo()` inside the class that declares the symbol.
        if object == "this" {
            if file == sym.file {
                if let Some(class) = &r.class_scope {
                    if sym.name == format!("{}.{}", class, prop) {
                        return Some(RefMatch {
                            relationship: Relationship::DirectCaller,
                            confidence: Confidence::ConfirmedRelationship,
                            evidence: vec![Evidence::new(
                                EvidenceKind::CallSite,
                                EvidenceLocation::line(file, r.line)
                                    .with_snippet(r.snippet.clone()),
                                format!("`this.{prop}` inside class `{class}`"),
                            )],
                        });
                    }
                }
            }
            // `this` in another file: no structural link.
            return Some(self.possible_member(
                file,
                r,
                prop,
                "receiver `this` is not type-resolved",
            ));
        }

        // Receiver bound to an import.
        if let Some((target_file, kind)) = self.object_module(file, object) {
            if target_file == sym.file {
                match kind {
                    BindingKind::Namespace => {
                        // `ns.foo()` — resolve the export.
                        if let Some((tf, tl)) = self.ws.resolve_export(&target_file, prop, 0) {
                            if let Some(d) = self
                                .ws
                                .files
                                .get(&tf)
                                .and_then(|p| p.find_decl(&tl.unwrap_or_default()))
                            {
                                if Self::symbol_of(&tf, d) == *sym {
                                    let mut evidence = vec![Evidence::new(
                                        EvidenceKind::CallSite,
                                        EvidenceLocation::line(file, r.line)
                                            .with_snippet(r.snippet.clone()),
                                        format!("`{object}.{prop}()` through a namespace import"),
                                    )];
                                    if let Some(line) = self.import_line(file, object) {
                                        evidence.push(Evidence::new(
                                            EvidenceKind::ImportEdge,
                                            EvidenceLocation::line(file, line),
                                            format!("`{object}` is imported here"),
                                        ));
                                    }
                                    return Some(RefMatch {
                                        relationship: Relationship::DirectCaller,
                                        confidence: Confidence::ConfirmedRelationship,
                                        evidence,
                                    });
                                }
                            }
                        }
                        return Some(self.possible_member(
                            file,
                            r,
                            prop,
                            &format!("`{object}` is a namespace import of `{}` but the export could not be tied to this symbol", target_file),
                        ));
                    }
                    BindingKind::Named(_) => {
                        // `router.handle()` where `router` is a default import:
                        // attribute if the symbol is a member of the imported class.
                        if let Some(base) = self.resolve_binding(file, object) {
                            if sym.name == format!("{}.{}", base.name, prop) {
                                let mut evidence = vec![Evidence::new(
                                    EvidenceKind::CallSite,
                                    EvidenceLocation::line(file, r.line)
                                        .with_snippet(r.snippet.clone()),
                                    format!(
                                        "`.{prop}()` on `{object}` (imported from `{}`)",
                                        base.file
                                    ),
                                )];
                                if let Some(line) = self.import_line(file, object) {
                                    evidence.push(Evidence::new(
                                        EvidenceKind::ImportEdge,
                                        EvidenceLocation::line(file, line),
                                        format!("`{object}` is imported here"),
                                    ));
                                }
                                return Some(RefMatch {
                                    relationship: Relationship::DirectCaller,
                                    confidence: Confidence::ConfirmedRelationship,
                                    evidence,
                                });
                            }
                        }
                        return Some(self.possible_member(
                            file,
                            r,
                            prop,
                            &format!(
                                "receiver `{object}` is imported but its member types are unknown"
                            ),
                        ));
                    }
                }
            }
        }

        // Unresolved receiver.
        if object.is_empty() {
            return None;
        }
        Some(self.possible_member(
            file,
            r,
            prop,
            &format!("receiver `{object}` has no statically known type"),
        ))
    }

    fn possible_member(&self, file: &str, r: &Ref, prop: &str, why: &str) -> RefMatch {
        RefMatch {
            relationship: Relationship::DirectCaller,
            confidence: Confidence::PossibleImpact,
            evidence: vec![Evidence::new(
                EvidenceKind::NameMatch,
                EvidenceLocation::line(file, r.line).with_snippet(r.snippet.clone()),
                format!("member `{prop}` matches by name; {why}"),
            )],
        }
    }

    fn match_type(&self, sym: &SymbolRef, file: &str, r: &Ref) -> Option<RefMatch> {
        if r.name != sym.name && r.name != Self::simple(&sym.name) {
            return None;
        }
        // Same-file type reference.
        if file == sym.file {
            if let Some(d) = self.ws.files.get(file).and_then(|p| p.find_decl(&r.name)) {
                if d.name == sym.name {
                    return Some(RefMatch {
                        relationship: Relationship::TypeConsumer,
                        confidence: Confidence::ConfirmedRelationship,
                        evidence: vec![Evidence::new(
                            EvidenceKind::TypeReference,
                            EvidenceLocation::line(file, r.line).with_snippet(r.snippet.clone()),
                            format!("type `{}` referenced here", r.name),
                        )],
                    });
                }
            }
        }
        if let Some(resolved) = self.resolve_binding(file, &r.name) {
            if resolved == *sym {
                return Some(RefMatch {
                    relationship: Relationship::TypeConsumer,
                    confidence: Confidence::ConfirmedRelationship,
                    evidence: vec![Evidence::new(
                        EvidenceKind::TypeReference,
                        EvidenceLocation::line(file, r.line).with_snippet(r.snippet.clone()),
                        format!("type `{}` referenced through an import", r.name),
                    )],
                });
            }
        }
        Some(RefMatch {
            relationship: Relationship::TypeConsumer,
            confidence: Confidence::PossibleImpact,
            evidence: vec![Evidence::new(
                EvidenceKind::NameMatch,
                EvidenceLocation::line(file, r.line),
                format!(
                    "type name `{}` matches, but no import connects it to `{}`",
                    r.name,
                    sym.id()
                ),
            )],
        })
    }

    fn match_inherit(&self, sym: &SymbolRef, file: &str, r: &Ref) -> Option<RefMatch> {
        if r.name != sym.name && r.name != Self::simple(&sym.name) {
            return None;
        }
        // Base may be `ns.Base` — resolve through bindings when simple.
        let base_name = Self::simple(&r.name);
        if file == sym.file {
            if let Some(d) = self.ws.files.get(file).and_then(|p| p.find_decl(base_name)) {
                if d.name == sym.name {
                    return Some(RefMatch {
                        relationship: Relationship::InheritsFrom,
                        confidence: Confidence::ConfirmedRelationship,
                        evidence: vec![Evidence::new(
                            EvidenceKind::InheritanceEdge,
                            EvidenceLocation::line(file, r.line).with_snippet(r.snippet.clone()),
                            format!("`extends {base_name}`"),
                        )],
                    });
                }
            }
        }
        if let Some(resolved) = self.resolve_binding(file, base_name) {
            if resolved == *sym {
                return Some(RefMatch {
                    relationship: Relationship::InheritsFrom,
                    confidence: Confidence::ConfirmedRelationship,
                    evidence: vec![Evidence::new(
                        EvidenceKind::InheritanceEdge,
                        EvidenceLocation::line(file, r.line).with_snippet(r.snippet.clone()),
                        format!("`extends {base_name}` (imported)"),
                    )],
                });
            }
        }
        Some(RefMatch {
            relationship: Relationship::InheritsFrom,
            confidence: Confidence::PossibleImpact,
            evidence: vec![Evidence::new(
                EvidenceKind::NameMatch,
                EvidenceLocation::line(file, r.line),
                format!(
                    "base name `{base_name}` matches, but no import connects it to `{}`",
                    sym.id()
                ),
            )],
        })
    }

    fn match_import(&self, sym: &SymbolRef, file: &str, r: &Ref) -> Option<RefMatch> {
        let local = if r.name.is_empty() {
            None
        } else {
            Some(r.name.as_str())
        };
        let local = local?;
        if let Some(resolved) = self.resolve_binding(file, local) {
            if resolved == *sym {
                return Some(RefMatch {
                    relationship: Relationship::ImportsSymbol,
                    confidence: Confidence::ConfirmedRelationship,
                    evidence: vec![Evidence::new(
                        EvidenceKind::ImportEdge,
                        EvidenceLocation::line(file, r.line).with_snippet(r.snippet.clone()),
                        format!(
                            "`{local}` imports `{}` from `{}`",
                            sym.name,
                            r.module.clone().unwrap_or_default()
                        ),
                    )],
                });
            }
        }
        // Namespace import of the symbol's own module: covers it loosely.
        if r.kind == RefKind::NamespaceImport || r.kind == RefKind::Require {
            if let Some(target) = self.binding_target_file(file, local) {
                if target.0 == sym.file
                    && matches!(target.1, BindingKind::Namespace)
                    && self.ws.export_exists(&sym.file, Self::simple(&sym.name))
                {
                    return Some(RefMatch {
                        relationship: Relationship::ImportsSymbol,
                        confidence: Confidence::PossibleImpact,
                        evidence: vec![Evidence::new(
                            EvidenceKind::ImportEdge,
                            EvidenceLocation::line(file, r.line),
                            format!(
                                "`{local}` imports the whole module `{}`; the import covers \
                                 `{}` but does not prove it is used",
                                sym.file, sym.name
                            ),
                        )],
                    });
                }
            }
        }
        None
    }

    fn import_line(&self, importer: &str, local: &str) -> Option<u32> {
        let parsed = self.ws.files.get(importer)?;
        parsed
            .refs
            .iter()
            .find(|r| {
                matches!(r.kind, RefKind::Import | RefKind::NamespaceImport) && r.name == local
            })
            .map(|r| r.line)
    }

    // -- public analysis entry points ---------------------------------------

    /// All direct (depth 1) relationships to `sym`, deduplicated to the
    /// strongest relationship per symbol.
    pub fn direct(&self, sym: &SymbolRef) -> Vec<AffectedSymbol> {
        let mut matches = self.match_all(sym);
        dedup_and_sort(&mut matches, sym)
    }

    /// Direct plus bounded transitive relationships.
    ///
    /// Returns `(direct, transitive)` where transitive chains start at the
    /// changed symbol. Confidence propagates as the weakest link on the path:
    /// an all-confirmed chain becomes *strongly supported*; a chain containing
    /// a name-only match stays *possible*.
    pub fn affected(
        &self,
        sym: &SymbolRef,
        max_depth: u32,
    ) -> (Vec<AffectedSymbol>, Vec<AffectedSymbol>) {
        let mut direct_matches = self.match_all(sym);
        let direct = dedup_and_sort(&mut direct_matches, sym);

        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(sym.id());
        let mut queue: Vec<(SymbolRef, Vec<SymbolRef>, u32, Confidence)> = Vec::new();
        for a in &direct {
            visited.insert(a.symbol.id());
            queue.push((
                a.symbol.clone(),
                vec![sym.clone(), a.symbol.clone()],
                1,
                a.confidence,
            ));
        }

        let mut transitive: Vec<AffectedSymbol> = Vec::new();
        while let Some((current, chain, depth, path_conf)) = queue.pop() {
            if depth >= max_depth || transitive.len() + direct.len() >= MAX_AFFECTED {
                continue;
            }
            let mut matches = self.match_all(&current);
            sort_matches(&mut matches, &current);
            for (target, m) in matches {
                if visited.contains(&target.id()) {
                    continue;
                }
                visited.insert(target.id());
                let propagated = propagate(path_conf, m.confidence, depth + 1);
                let mut chain = chain.clone();
                chain.push(target.clone());
                transitive.push(AffectedSymbol {
                    symbol: target.clone(),
                    relationship: if depth + 1 > 1 {
                        Relationship::Transitive
                    } else {
                        m.relationship
                    },
                    depth: depth + 1,
                    confidence: propagated,
                    evidence: m.evidence,
                    chain,
                });
                if transitive.len() + direct.len() >= MAX_AFFECTED {
                    break;
                }
                queue.push((
                    target,
                    {
                        // chain already includes target
                        transitive
                            .last()
                            .map(|a| a.chain.clone())
                            .unwrap_or_default()
                    },
                    depth + 1,
                    propagated,
                ));
            }
        }
        (direct, transitive)
    }

    /// Every match for `sym` across the workspace, keyed by affected symbol.
    fn match_all(&self, sym: &SymbolRef) -> Vec<(SymbolRef, RefMatch)> {
        let simple = Self::simple(&sym.name);
        let mut out: Vec<(SymbolRef, RefMatch)> = Vec::new();

        if let Some(list) = self.calls.get(simple) {
            for (file, r) in list {
                if let Some(m) = self.match_ref(sym, file, r) {
                    // The affected symbol is the *enclosing* declaration that
                    // contains the call, not the callee name itself.
                    let target = self
                        .affected_symbol_for_ref(sym, file, r)
                        .unwrap_or_else(|| {
                            SymbolRef::new(
                                file,
                                format!("{simple} (call site)"),
                                SymbolKind::Unknown,
                            )
                        });
                    out.push((target, m));
                }
            }
        }
        if let Some(list) = self.members.get(simple) {
            for (file, r) in list {
                if let Some(m) = self.match_ref(sym, file, r) {
                    let target = self
                        .affected_symbol_for_ref(sym, file, r)
                        .unwrap_or_else(|| {
                            SymbolRef::new(
                                file,
                                format!(
                                    "{} ({} member)",
                                    simple,
                                    r.object.clone().unwrap_or_default()
                                ),
                                SymbolKind::Unknown,
                            )
                        });
                    out.push((target, m));
                }
            }
        }
        if let Some(list) = self.types.get(simple) {
            for (file, r) in list {
                if let Some(m) = self.match_ref(sym, file, r) {
                    let target = self
                        .affected_symbol_for_ref(sym, file, r)
                        .unwrap_or_else(|| {
                            SymbolRef::new(
                                file,
                                format!("{simple} (type use)"),
                                SymbolKind::Unknown,
                            )
                        });
                    out.push((target, m));
                }
            }
        }
        if let Some(list) = self.inherits.get(simple) {
            for (file, r) in list {
                if let Some(m) = self.match_ref(sym, file, r) {
                    let target = self
                        .affected_symbol_for_ref(sym, file, r)
                        .unwrap_or_else(|| {
                            SymbolRef::new(
                                file,
                                format!("{simple} (subclass)"),
                                SymbolKind::Unknown,
                            )
                        });
                    out.push((target, m));
                }
            }
        }
        // Importers of the symbol's module that import *this* symbol.
        for key in [simple.to_string(), "default".to_string()] {
            if let Some(list) = self.importers.get(&(sym.file.clone(), key.clone())) {
                for (file, line, local) in list {
                    // Verify the binding really resolves to this symbol.
                    let Some(resolved) = self.resolve_binding(file, local) else {
                        continue;
                    };
                    if resolved != *sym {
                        continue;
                    }
                    out.push((
                        SymbolRef::new(file, local.clone(), SymbolKind::Module),
                        RefMatch {
                            relationship: Relationship::ImportsSymbol,
                            confidence: Confidence::ConfirmedRelationship,
                            evidence: vec![Evidence::new(
                                EvidenceKind::ImportEdge,
                                EvidenceLocation::line(file, *line),
                                format!("imports `{local}` from module `{}`", sym.file),
                            )],
                        },
                    ));
                }
            }
        }
        out
    }

    /// Which symbol inside this reference is the *affected* one (best effort).
    fn affected_symbol_for_ref(&self, sym: &SymbolRef, file: &str, r: &Ref) -> Option<SymbolRef> {
        // The affected symbol of a caller reference is the enclosing symbol.
        // We approximate with the declaration in `file` that contains the line.
        let parsed = self.ws.files.get(file)?;
        let mut best: Option<&Decl> = None;
        for d in &parsed.decls {
            let within = r.line >= d.start_line && r.line <= d.end_line;
            // Prefer the smallest enclosing declaration (innermost symbol).
            let tighter = best
                .map(|b| (b.end_line - b.start_line) >= (d.end_line - d.start_line))
                .unwrap_or(true);
            if within && tighter {
                best = Some(d);
            }
        }
        best.map(|d| Self::symbol_of(file, d))
            .filter(|s| *s != *sym)
    }

    /// Files importing `file` at module level (for file-scoped changes).
    pub fn module_importers(&self, file: &str) -> Vec<AffectedSymbol> {
        let mut by_from: HashMap<&str, Vec<&ImportEdge>> = HashMap::new();
        for e in &self.ws.edges {
            if e.to == file {
                by_from.entry(e.from.as_str()).or_default().push(e);
            }
        }
        let mut out: Vec<AffectedSymbol> = by_from
            .into_iter()
            .map(|(from, edges)| {
                let mut lines: Vec<u32> = edges.iter().map(|e| e.line).collect();
                lines.sort();
                lines.dedup();
                let first = lines[0];
                let spec = edges[0].specifier.clone();
                AffectedSymbol {
                    symbol: SymbolRef::new(from, "(module)", SymbolKind::Module),
                    relationship: Relationship::ImportsSymbol,
                    depth: 1,
                    confidence: Confidence::ConfirmedRelationship,
                    evidence: vec![Evidence::new(
                        EvidenceKind::ImportEdge,
                        EvidenceLocation::line(from, first),
                        format!("imports `{file}` via `{spec}`"),
                    )],
                    chain: vec![],
                }
            })
            .collect();
        out.sort_by(|a, b| a.symbol.file.cmp(&b.symbol.file));
        out
    }

    /// Transitive closure over *module* imports (file-scoped changes).
    pub fn module_importers_transitive(&self, file: &str, max_depth: u32) -> Vec<AffectedSymbol> {
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(file.to_string());
        let mut frontier: Vec<(String, u32)> = vec![(file.to_string(), 0)];
        let mut out = Vec::new();
        while let Some((current, depth)) = frontier.pop() {
            if depth >= max_depth {
                continue;
            }
            for a in self.module_importers(&current) {
                if visited.contains(&a.symbol.file) {
                    continue;
                }
                visited.insert(a.symbol.file.clone());
                let mut a = a;
                a.depth = depth + 1;
                a.relationship = if depth + 1 == 1 {
                    Relationship::ImportsSymbol
                } else {
                    Relationship::Transitive
                };
                a.confidence = if depth + 1 == 1 {
                    Confidence::ConfirmedRelationship
                } else {
                    Confidence::StronglySupportedImpact
                };
                out.push(a.clone());
                frontier.push((a.symbol.file.clone(), depth + 1));
            }
        }
        out
    }

    /// Evidence that `test_file` references `sym` (used for test discovery).
    pub fn test_evidence(&self, test_file: &str, sym: &SymbolRef) -> Option<Evidence> {
        let parsed = self.ws.files.get(test_file)?;
        let simple = Self::simple(&sym.name);
        for r in &parsed.refs {
            let name_match = match r.kind {
                RefKind::Call => r.name == simple,
                RefKind::MemberCall | RefKind::New | RefKind::NamespaceMember => {
                    r.property.as_deref() == Some(simple)
                }
                RefKind::TypeUse | RefKind::Inherit => r.name == simple || r.name == sym.name,
                RefKind::Import | RefKind::ReExport | RefKind::NamespaceImport => {
                    self.resolve_binding(test_file, &r.name).as_ref() == Some(sym)
                        || r.module.as_deref().map(|m| {
                            let known: HashSet<String> = self.ws.files.keys().cloned().collect();
                            matches!(resolve_specifier(test_file, m, &known), Resolved::File(f) if f == sym.file)
                        })
                        .unwrap_or(false)
                }
                RefKind::DynamicImport | RefKind::Require => {
                    r.literal_specifier
                        && r.module.as_deref().map(|m| {
                            let known: HashSet<String> = self.ws.files.keys().cloned().collect();
                            matches!(resolve_specifier(test_file, m, &known), Resolved::File(f) if f == sym.file)
                        })
                        .unwrap_or(false)
                }
                _ => false,
            };
            if name_match {
                return Some(Evidence::new(
                    EvidenceKind::TestReference,
                    EvidenceLocation::line(test_file, r.line).with_snippet(r.snippet.clone()),
                    format!("test file references `{}`", sym.name),
                ));
            }
        }
        None
    }
}

/// Compute the propagated confidence for a transitive hop.
fn propagate(path_conf: Confidence, link: Confidence, depth: u32) -> Confidence {
    let weakest = if path_conf.impact_rank() >= link.impact_rank() {
        path_conf
    } else {
        link
    };
    if depth > 1 && weakest == Confidence::ConfirmedRelationship {
        Confidence::StronglySupportedImpact
    } else {
        weakest
    }
}

fn relationship_rank(r: Relationship) -> u8 {
    match r {
        Relationship::DirectCaller => 0,
        Relationship::ImportsSymbol => 1,
        Relationship::InheritsFrom => 2,
        Relationship::TypeConsumer => 3,
        Relationship::UsesSymbol => 4,
        Relationship::Transitive => 5,
        Relationship::TestConsumer => 6,
    }
}

/// Merge duplicate matches (same affected symbol), keeping the strongest.
fn dedup_and_sort(
    matches: &mut Vec<(SymbolRef, RefMatch)>,
    _changed: &SymbolRef,
) -> Vec<AffectedSymbol> {
    let mut best: HashMap<String, (SymbolRef, RefMatch)> = HashMap::new();
    for (sym, m) in matches.drain(..) {
        let key = sym.id();
        match best.get_mut(&key) {
            None => {
                best.insert(key, (sym, m));
            }
            Some((existing_sym, existing_m)) => {
                let better = (
                    m.confidence.impact_rank(),
                    relationship_rank(m.relationship),
                ) < (
                    existing_m.confidence.impact_rank(),
                    relationship_rank(existing_m.relationship),
                );
                if better {
                    // Keep the stronger match but retain extra evidence.
                    let mut merged = m;
                    for e in &existing_m.evidence {
                        if !merged.evidence.contains(e)
                            && merged.evidence.len() < MAX_EVIDENCE_PER_MATCH
                        {
                            merged.evidence.push(e.clone());
                        }
                    }
                    *existing_sym = sym;
                    *existing_m = merged;
                } else {
                    for e in &m.evidence {
                        if !existing_m.evidence.contains(e)
                            && existing_m.evidence.len() < MAX_EVIDENCE_PER_MATCH
                        {
                            existing_m.evidence.push(e.clone());
                        }
                    }
                }
            }
        }
    }
    let mut out: Vec<AffectedSymbol> = best
        .into_values()
        .map(|(symbol, m)| AffectedSymbol {
            symbol,
            relationship: m.relationship,
            depth: 1,
            confidence: m.confidence,
            evidence: m.evidence,
            chain: vec![],
        })
        .collect();
    out.sort_by(|a, b| {
        (
            a.confidence.impact_rank(),
            relationship_rank(a.relationship),
            &a.symbol.file,
            &a.symbol.name,
        )
            .cmp(&(
                b.confidence.impact_rank(),
                relationship_rank(b.relationship),
                &b.symbol.file,
                &b.symbol.name,
            ))
    });
    out.truncate(MAX_AFFECTED);
    out
}

fn sort_matches(matches: &mut [(SymbolRef, RefMatch)], _sym: &SymbolRef) {
    matches.sort_by(|a, b| {
        (
            a.1.confidence.impact_rank(),
            relationship_rank(a.1.relationship),
            &a.0.file,
            &a.0.name,
        )
            .cmp(&(
                b.1.confidence.impact_rank(),
                relationship_rank(b.1.relationship),
                &b.0.file,
                &b.0.name,
            ))
    });
}
