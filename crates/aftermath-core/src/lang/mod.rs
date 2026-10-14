//! Language adapter layer.
//!
//! The evidence model is language-agnostic: a [`LanguageAdapter`] turns source
//! text into declarations ([`Decl`]) and references ([`Ref`]) that the impact
//! analyzer consumes. Adding a language means adding an adapter, not changing
//! the evidence model.

pub mod javascript;

use serde::{Deserialize, Serialize};

use crate::evidence::SymbolKind;

/// A declared parameter (used for counterfactual test suggestions).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    /// Type annotation text when present (never fabricated when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_text: Option<String>,
    pub optional: bool,
    pub rest: bool,
    /// Default value expression text, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// A declaration extracted from source.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Decl {
    /// Simple name; for class methods, `Class.method`.
    pub name: String,
    pub kind: SymbolKind,
    /// 1-based inclusive declaration span.
    pub start_line: u32,
    pub end_line: u32,
    /// Exported from its module (directly or via export specifier).
    pub exported: bool,
    /// Part of the module's public API (exported, or a public member of an
    /// exported class, or an interface/type export).
    pub public_api: bool,
    pub is_default_export: bool,
    pub is_async: bool,
    pub is_private: bool,
    pub params: Vec<Param>,
    /// Declared return type text, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_type: Option<String>,
    /// Number of branching points inside the body (if / switch case / ternary).
    pub branches: u32,
    /// Body contains try/catch/throw or an explicit failure return.
    pub has_error_handling: bool,
    /// Body contains an early `return` (helps suggest boundary tests).
    pub has_returns: bool,
    /// Body performs arithmetic or numeric comparison — a boundary-sensitivity
    /// signal even when parameters are untyped (plain JavaScript).
    pub uses_numeric_ops: bool,
}

impl Decl {
    /// Whether the declaration takes a boundary-sensitive numeric parameter.
    pub fn has_numeric_param(&self) -> bool {
        let typed_numeric = self
            .params
            .iter()
            .any(|p| p.type_text.as_deref().map(is_numeric_type).unwrap_or(false));
        typed_numeric || (self.uses_numeric_ops && self.takes_params())
    }

    /// Whether the declaration takes any parameter at all.
    pub fn takes_params(&self) -> bool {
        !self.params.is_empty()
    }
}

/// Word-boundary match of a type annotation against known numeric type names.
/// Matching is token-based so `Point` is not mistaken for `int`.
pub(crate) fn is_numeric_type(type_text: &str) -> bool {
    const NUMERIC: &[&str] = &[
        "number", "integer", "int", "uint", "float", "double", "decimal", "long", "short", "byte",
        "usize", "isize", "f32", "f64", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64",
    ];
    type_text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| NUMERIC.contains(&tok))
}

/// Kind of a reference (a *use* of something outside its declaration).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    /// `foo(...)`
    Call,
    /// `obj.foo(...)` — attribute is a name, receiver recorded separately.
    MemberCall,
    /// `new Foo(...)`
    New,
    /// `import { a } from "./m"` — local binding recorded.
    Import,
    /// `import * as ns from "./m"`
    NamespaceImport,
    /// `export { a as b } from "./m"` (or without `from`).
    ReExport,
    /// `export { a as b }`
    ExportSpecifier,
    /// A type position referencing a named type.
    TypeUse,
    /// `class X extends Y` / `interface I extends J` / `implements`.
    Inherit,
    /// `import("./m")` — dynamic import.
    DynamicImport,
    /// `require("./m")`.
    Require,
    /// `eval(...)` or computed property access: runtime dispatch.
    RuntimeDispatch,
    /// `obj[expr]` where expr is not a string literal.
    ComputedMember,
    /// Instantiation or reference through a namespace binding.
    NamespaceMember,
}

/// A reference extracted from source.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Ref {
    pub kind: RefKind,
    /// Primary name as written: callee name, local import binding, extended
    /// type, exported name, or dynamic-import specifier.
    pub name: String,
    /// For member calls: the property being accessed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    /// For member calls: receiver expression text (truncated), e.g. `this`,
    /// `utils`, `window.app`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// For imports/re-exports: module specifier as written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// For imports/re-exports: the name inside the source module
    /// (`None` means the default export).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imported: Option<String>,
    /// Enclosing class name when the reference sits inside a class body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class_scope: Option<String>,
    pub line: u32,
    /// Trimmed source line, secret-redacted downstream.
    pub snippet: String,
    /// For `import()`/`require()`: specifier was a literal (resolvable).
    pub literal_specifier: bool,
}

/// A parse-level problem inside an otherwise parsed file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ParseIssue {
    pub line: u32,
    pub message: String,
}

/// An export binding of a module.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExportBinding {
    /// Name as exported (`"default"` for default exports, `"*"` for star re-export).
    pub exported: String,
    /// Name in this module, when different from `exported`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    /// Source module when this export is a re-export.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// A discovered test declaration (`test(...)`, `it(...)`, `describe(...)` or a
/// `testX`/`itX`-style function). Recorded so the report can distinguish
/// "tests exist for this symbol" from "no test was identified".
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TestDecl {
    /// The literal test title when present (e.g. `adds numbers`).
    pub title: Option<String>,
    /// The enclosing function name for `function testX() {}` styles.
    pub name: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// Result of parsing one file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParsedFile {
    pub path: String,
    pub language: String,
    pub decls: Vec<Decl>,
    pub refs: Vec<Ref>,
    pub exports: Vec<ExportBinding>,
    pub tests: Vec<TestDecl>,
    pub issues: Vec<ParseIssue>,
    /// True when the syntax tree contains error nodes. Analysis proceeds
    /// (tree-sitter recovers) but the file is reported as a blind spot.
    pub has_syntax_errors: bool,
}

impl ParsedFile {
    pub fn empty(path: &str, language: &str) -> Self {
        Self {
            path: path.to_string(),
            language: language.to_string(),
            decls: Vec::new(),
            refs: Vec::new(),
            exports: Vec::new(),
            tests: Vec::new(),
            issues: Vec::new(),
            has_syntax_errors: false,
        }
    }

    /// Public declarations only.
    pub fn public_decls(&self) -> impl Iterator<Item = &Decl> {
        self.decls.iter().filter(|d| d.public_api)
    }

    /// Find a declaration by exact name.
    pub fn find_decl(&self, name: &str) -> Option<&Decl> {
        self.decls.iter().find(|d| d.name == name)
    }
}

/// Adapter for one language family.
pub trait LanguageAdapter: Send + Sync {
    fn language_name(&self) -> &'static str;
    fn extensions(&self) -> &'static [&'static str];
    fn parse(&self, path: &str, source: &str) -> ParsedFile;
}

/// File extensions handled by the built-in adapters.
pub const SUPPORTED_EXTENSIONS: &[&str] = &["js", "jsx", "mjs", "cjs", "ts", "tsx"];

/// Returns the adapter for a file path, if its extension is supported.
pub fn adapter_for(path: &str) -> Option<Box<dyn LanguageAdapter>> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "js" | "jsx" | "mjs" | "cjs" => Some(Box::new(javascript::JsAdapter::javascript())),
        "ts" => Some(Box::new(javascript::JsAdapter::typescript())),
        "tsx" => Some(Box::new(javascript::JsAdapter::tsx())),
        _ => None,
    }
}

/// True when the extension is analyzable at all.
pub fn is_supported(path: &str) -> bool {
    let Some(ext) = path.rsplit('.').next() else {
        return false;
    };
    SUPPORTED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// Parse `source` for `path` with the matching adapter. Returns `None` when
/// the extension has no adapter. `path` should be repository-relative so the
/// resulting [`ParsedFile::path`] is reproducible.
pub fn parse_file(path: &str, source: &str) -> Option<ParsedFile> {
    let adapter = adapter_for(path)?;
    Some(adapter.parse(path, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_dispatch_by_extension() {
        assert!(adapter_for("src/a.ts").is_some());
        assert!(adapter_for("src/a.tsx").is_some());
        assert!(adapter_for("src/a.js").is_some());
        assert!(adapter_for("src/a.jsx").is_some());
        assert!(adapter_for("src/a.mjs").is_some());
        assert!(adapter_for("src/a.cjs").is_some());
        assert!(adapter_for("src/a.py").is_none());
        assert!(adapter_for("Makefile").is_none());
        assert!(adapter_for("noextension").is_none());
    }

    #[test]
    fn supported_check_is_case_insensitive() {
        assert!(is_supported("src/A.TS"));
        assert!(!is_supported("src/A.json"));
    }
}
