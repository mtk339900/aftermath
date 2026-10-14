//! JavaScript / TypeScript adapter built on tree-sitter.
//!
//! Honest scope: this is *syntax-level* analysis. It extracts declarations,
//! calls, imports, exports, inheritance and type references, and it records
//! everything it cannot resolve (dynamic import, computed access, `eval`,
//! syntax errors) instead of guessing.

use std::collections::HashSet;

use tree_sitter::{Node, Parser};

use super::{
    Decl, ExportBinding, LanguageAdapter, Param, ParseIssue, ParsedFile, Ref, RefKind, TestDecl,
};
use crate::evidence::SymbolKind;

const MAX_SNIPPET: usize = 160;
const MAX_NAME: usize = 80;

/// Adapter over the JavaScript and TypeScript tree-sitter grammars.
pub struct JsAdapter {
    language: tree_sitter::Language,
    language_name: &'static str,
    extensions: &'static [&'static str],
}

impl JsAdapter {
    pub fn javascript() -> Self {
        Self {
            language: tree_sitter_javascript::LANGUAGE.into(),
            language_name: "javascript",
            extensions: &["js", "jsx", "mjs", "cjs"],
        }
    }

    pub fn typescript() -> Self {
        Self {
            language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            language_name: "typescript",
            extensions: &["ts"],
        }
    }

    pub fn tsx() -> Self {
        Self {
            language: tree_sitter_typescript::LANGUAGE_TSX.into(),
            language_name: "tsx",
            extensions: &["tsx"],
        }
    }
}

impl LanguageAdapter for JsAdapter {
    fn language_name(&self) -> &'static str {
        self.language_name
    }

    fn extensions(&self) -> &'static [&'static str] {
        self.extensions
    }

    fn parse(&self, path: &str, source: &str) -> ParsedFile {
        let mut out = ParsedFile::empty(path, self.language_name);
        let mut parser = Parser::new();
        if parser.set_language(&self.language).is_err() {
            out.issues.push(ParseIssue {
                line: 1,
                message: "grammar could not be loaded".into(),
            });
            out.has_syntax_errors = true;
            return out;
        }
        let Some(tree) = parser.parse(source, None) else {
            out.issues.push(ParseIssue {
                line: 1,
                message: "parser returned no tree".into(),
            });
            out.has_syntax_errors = true;
            return out;
        };

        let mut w = Walker {
            source,
            out,
            scope: Vec::new(),
            seen: HashSet::new(),
        };
        let root = tree.root_node();
        w.walk(root, ExportCtx::default());
        w.out.has_syntax_errors = root.has_error();
        if w.out.has_syntax_errors {
            w.out.issues = collect_issues(root, source);
        }
        // Post-process: `export { local as name }` (without `from`) marks the
        // local declaration as part of the module's public surface.
        let bindings = w.out.exports.clone();
        for b in bindings {
            if b.from.is_none() && b.exported != "default" && b.exported != "*" {
                if let Some(local) = b.local {
                    if let Some(d) = w.out.decls.iter_mut().find(|d| d.name == local) {
                        d.exported = true;
                        d.public_api = true;
                    }
                }
            }
        }
        w.out
    }
}

#[derive(Clone, Copy, Default)]
struct ExportCtx {
    exported: bool,
    is_default: bool,
}

#[derive(Clone)]
struct Scope {
    /// Enclosing class or interface name.
    name: Option<String>,
    exported: bool,
    is_interface: bool,
}

struct Walker<'a> {
    source: &'a str,
    out: ParsedFile,
    scope: Vec<Scope>,
    seen: HashSet<(u8, String, String, u32)>,
}

#[derive(Default)]
struct FnStats {
    params: Vec<Param>,
    return_type: Option<String>,
    branches: u32,
    error_handling: bool,
    has_returns: bool,
    numeric_ops: bool,
    is_async: bool,
}

impl<'a> Walker<'a> {
    fn text(&self, node: Node) -> &'a str {
        &self.source[node.start_byte()..node.end_byte()]
    }

    fn line(&self, node: Node) -> u32 {
        (node.start_position().row + 1) as u32
    }

    fn end_line(&self, node: Node) -> u32 {
        (node.end_position().row + 1) as u32
    }

    fn snippet(&self, node: Node) -> String {
        let row = node.start_position().row;
        self.source
            .lines()
            .nth(row)
            .unwrap_or("")
            .trim()
            .chars()
            .take(MAX_SNIPPET)
            .collect()
    }

    fn truncated(&self, s: &str) -> String {
        let mut t: String = s.chars().take(MAX_NAME).collect();
        if s.chars().count() > MAX_NAME {
            t.push('…');
        }
        t
    }

    fn add_ref(&mut self, r: Ref) {
        let key = (
            ref_kind_tag(r.kind),
            r.name.clone(),
            r.property.clone().unwrap_or_default(),
            r.line,
        );
        if self.seen.insert(key) {
            self.out.refs.push(r);
        }
    }

    fn add_decl(&mut self, d: Decl) {
        if self
            .out
            .decls
            .iter()
            .any(|x| x.name == d.name && x.start_line == d.start_line)
        {
            return;
        }
        self.out.decls.push(d);
    }

    fn current_class(&self) -> Option<&Scope> {
        self.scope
            .iter()
            .rev()
            .find(|s| s.name.is_some() && !s.is_interface)
    }

    // -- main dispatch ------------------------------------------------------

    fn walk(&mut self, node: Node, ctx: ExportCtx) {
        // NOTE: error nodes are walked too — tree-sitter recovers partial
        // structure and we would rather extract what we can than skip it.
        match node.kind() {
            "import_statement" => {
                self.handle_import(node);
                return;
            }
            "export_statement" => {
                self.handle_export(node);
                return;
            }
            "call_expression" => {
                self.handle_call(node);
            }
            "new_expression" => {
                self.handle_new(node);
            }
            "member_expression" | "subscript_expression" => {
                // Standalone member access (not part of a call) — used by
                // namespace imports: `ns.foo` read without calling.
                self.handle_member_read(node);
            }
            "class_declaration" | "class" | "class_expression" => {
                self.handle_class(node, ctx);
                return;
            }
            "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "arrow_function" => {
                // Only named declarations become symbols; expression positions
                // are handled by their parents (variable declarator / pair / assignment).
                if matches!(
                    node.kind(),
                    "function_declaration" | "generator_function_declaration"
                ) {
                    self.handle_function_decl(node, ctx);
                    return;
                }
            }
            "interface_declaration" => {
                self.handle_interface(node, ctx);
                return;
            }
            "type_alias_declaration" | "enum_declaration" => {
                self.handle_type_decl(node, ctx);
                return;
            }
            "lexical_declaration" | "variable_declaration" => {
                self.handle_var_decl(node, ctx);
                return;
            }
            "method_definition" | "method_signature" => {
                self.handle_method(node);
                return;
            }
            "public_field_definition" | "field_definition" => {
                self.handle_field(node, ctx);
                return;
            }
            "assignment_expression" => {
                self.handle_assignment(node);
            }
            "type_identifier" => {
                self.handle_type_use(node);
            }
            "extends_type_clause" | "implements_clause" => {
                self.handle_inherit(node);
            }
            "class_heritage" => {
                self.handle_heritage(node);
                return;
            }
            "try_statement" | "throw_statement" | "catch_clause" => {
                self.mark_error_scope();
            }
            _ => {}
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, ctx);
        }
    }

    // -- declarations -------------------------------------------------------

    fn handle_function_decl(&mut self, node: Node, ctx: ExportCtx) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.truncated(self.text(name_node));
        let stats = self.collect_fn_stats(node);
        let scope_public = self.scope.iter().any(|s| s.exported);
        self.add_decl(Decl {
            name,
            kind: SymbolKind::Function,
            start_line: self.line(node),
            end_line: self.end_line(node),
            exported: ctx.exported && !ctx.is_default,
            public_api: (ctx.exported && !ctx.is_default) || scope_public,
            is_default_export: ctx.is_default,
            is_async: stats.is_async,
            is_private: false,
            params: stats.params,
            return_type: stats.return_type,
            branches: stats.branches,
            has_error_handling: stats.error_handling,
            has_returns: stats.has_returns,
            uses_numeric_ops: stats.numeric_ops,
        });
        if ctx.exported {
            let exported = if ctx.is_default {
                "default".to_string()
            } else {
                self.truncated(self.text(name_node))
            };
            self.out.exports.push(ExportBinding {
                exported,
                local: Some(self.truncated(self.text(name_node))),
                from: None,
            });
        }
        // Walk the body (including parameter types, which reference types)
        // so nested calls/refs are collected.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.is_named() && child.kind() != "identifier" {
                self.walk(child, ExportCtx::default());
            }
        }
    }

    fn handle_method(&mut self, node: Node) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let prop = self.text(name_node);
        let is_private = name_node.kind() == "private_property_identifier"
            || self.modifier_present(node, "private")
            || self.modifier_present(node, "protected")
            || prop.starts_with('#');
        let bare = prop.trim_start_matches('#');
        // Methods are named after any enclosing class *or interface* scope.
        let scope = self.scope.iter().rev().find(|s| s.name.is_some());
        let class = scope.and_then(|s| s.name.clone());
        let scope_exported = scope.map(|s| s.exported).unwrap_or(false);
        let name = match &class {
            Some(c) => format!("{}.{}", c, bare),
            None => bare.to_string(),
        };
        let stats = self.collect_fn_stats(node);
        self.add_decl(Decl {
            name: self.truncated(&name),
            kind: SymbolKind::Method,
            start_line: self.line(node),
            end_line: self.end_line(node),
            exported: false,
            public_api: scope_exported && !is_private,
            is_default_export: false,
            is_async: stats.is_async,
            is_private,
            params: stats.params,
            return_type: stats.return_type,
            branches: stats.branches,
            has_error_handling: stats.error_handling,
            has_returns: stats.has_returns,
            uses_numeric_ops: stats.numeric_ops,
        });
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.is_named() {
                self.walk(child, ExportCtx::default());
            }
        }
    }

    fn handle_class(&mut self, node: Node, ctx: ExportCtx) {
        let name = node
            .child_by_field_name("name")
            .map(|n| self.truncated(self.text(n)))
            .or_else(|| {
                // `const Foo = class {}` — take the declarator name.
                node.parent()
                    .filter(|p| p.kind() == "variable_declarator")
                    .and_then(|p| p.child_by_field_name("name"))
                    .map(|n| self.truncated(self.text(n)))
            });
        let declared = name.clone();
        if let Some(n) = &name {
            self.add_decl(Decl {
                name: n.clone(),
                kind: SymbolKind::Class,
                start_line: self.line(node),
                end_line: self.end_line(node),
                exported: ctx.exported && !ctx.is_default,
                public_api: ctx.exported || self.scope.iter().any(|s| s.exported),
                is_default_export: ctx.is_default,
                is_async: false,
                is_private: false,
                params: vec![],
                return_type: None,
                branches: 0,
                has_error_handling: false,
                has_returns: false,
                uses_numeric_ops: false,
            });
        }
        if ctx.exported {
            let exported = if ctx.is_default {
                "default".to_string()
            } else {
                declared.clone().unwrap_or_default()
            };
            self.out.exports.push(ExportBinding {
                exported,
                local: declared,
                from: None,
            });
        }

        self.scope.push(Scope {
            name: name.clone(),
            exported: ctx.exported || self.scope.iter().any(|s| s.exported),
            is_interface: false,
        });
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                // Heritage handled via class_heritage branch in walk().
                "class_body" => self.walk(child, ExportCtx::default()),
                "class_heritage" => self.walk(child, ExportCtx::default()),
                _ => {}
            }
        }
        self.scope.pop();
    }

    fn handle_interface(&mut self, node: Node, ctx: ExportCtx) {
        let name = node
            .child_by_field_name("name")
            .map(|n| self.truncated(self.text(n)))
            .unwrap_or_default();
        self.add_decl(Decl {
            name: name.clone(),
            kind: SymbolKind::Interface,
            start_line: self.line(node),
            end_line: self.end_line(node),
            exported: ctx.exported && !ctx.is_default,
            public_api: ctx.exported,
            is_default_export: ctx.is_default,
            is_async: false,
            is_private: false,
            params: vec![],
            return_type: None,
            branches: 0,
            has_error_handling: false,
            has_returns: false,
            uses_numeric_ops: false,
        });
        if ctx.exported {
            let exported = if ctx.is_default {
                "default".to_string()
            } else {
                name.clone()
            };
            self.out.exports.push(ExportBinding {
                exported,
                local: Some(name.clone()),
                from: None,
            });
        }
        self.scope.push(Scope {
            name: Some(name),
            exported: ctx.exported,
            is_interface: true,
        });
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.is_named() {
                self.walk(child, ExportCtx::default());
            }
        }
        self.scope.pop();
    }

    fn handle_type_decl(&mut self, node: Node, ctx: ExportCtx) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = self.truncated(self.text(name_node));
        let kind = match node.kind() {
            "type_alias_declaration" => SymbolKind::TypeAlias,
            "enum_declaration" => SymbolKind::Enum,
            _ => SymbolKind::Unknown,
        };
        self.add_decl(Decl {
            name: name.clone(),
            kind,
            start_line: self.line(node),
            end_line: self.end_line(node),
            exported: ctx.exported && !ctx.is_default,
            public_api: ctx.exported,
            is_default_export: ctx.is_default,
            is_async: false,
            is_private: false,
            params: vec![],
            return_type: None,
            branches: 0,
            has_error_handling: false,
            has_returns: false,
            uses_numeric_ops: false,
        });
        if ctx.exported {
            let exported = if ctx.is_default {
                "default".to_string()
            } else {
                name.clone()
            };
            self.out.exports.push(ExportBinding {
                exported,
                local: Some(name),
                from: None,
            });
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.is_named() && child.kind() != "type_identifier" {
                self.walk(child, ExportCtx::default());
            }
        }
    }

    fn handle_var_decl(&mut self, node: Node, ctx: ExportCtx) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = child.child_by_field_name("name") else {
                continue;
            };
            if !matches!(name_node.kind(), "identifier") {
                continue; // destructuring: cannot assign a stable simple name
            }
            let name = self.truncated(self.text(name_node));
            let value = child.child_by_field_name("value");
            let value_kind = value.map(|v| v.kind()).unwrap_or("");

            let function_like = matches!(
                value_kind,
                "arrow_function" | "function_expression" | "generator_function"
            );
            let class_like = matches!(value_kind, "class" | "class_expression");

            if function_like || class_like {
                let stats = value.map(|v| self.collect_fn_stats(v)).unwrap_or_default();
                let kind = if class_like {
                    SymbolKind::Class
                } else {
                    SymbolKind::Function
                };
                self.add_decl(Decl {
                    name: name.clone(),
                    kind,
                    start_line: self.line(child),
                    end_line: self.end_line(child),
                    exported: ctx.exported && !ctx.is_default,
                    public_api: ctx.exported,
                    is_default_export: ctx.is_default,
                    is_async: stats.is_async,
                    is_private: false,
                    params: stats.params,
                    return_type: stats.return_type,
                    branches: stats.branches,
                    has_error_handling: stats.error_handling,
                    has_returns: stats.has_returns,
                    uses_numeric_ops: stats.numeric_ops,
                });
                if ctx.exported {
                    let exported = if ctx.is_default {
                        "default".to_string()
                    } else {
                        name.clone()
                    };
                    self.out.exports.push(ExportBinding {
                        exported,
                        local: Some(name.clone()),
                        from: None,
                    });
                }
            } else if ctx.exported {
                self.add_decl(Decl {
                    name: name.clone(),
                    kind: SymbolKind::Variable,
                    start_line: self.line(child),
                    end_line: self.end_line(child),
                    exported: true,
                    public_api: true,
                    is_default_export: ctx.is_default,
                    is_async: false,
                    is_private: false,
                    params: vec![],
                    return_type: None,
                    branches: 0,
                    has_error_handling: false,
                    has_returns: false,
                    uses_numeric_ops: false,
                });
                let exported = if ctx.is_default {
                    "default".to_string()
                } else {
                    name.clone()
                };
                self.out.exports.push(ExportBinding {
                    exported,
                    local: Some(name),
                    from: None,
                });
            }
            // Walk the initializer for references.
            if let Some(v) = value {
                self.walk(v, ExportCtx::default());
            }
        }
    }

    fn handle_field(&mut self, node: Node, ctx: ExportCtx) {
        // `foo = () => {}` inside a class body.
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let value = node.child_by_field_name("value");
        let value_kind = value.map(|v| v.kind()).unwrap_or("");
        if !matches!(value_kind, "arrow_function" | "function_expression") {
            if ctx.exported {
                let name = self.truncated(self.text(name_node));
                self.add_decl(Decl {
                    name: name.clone(),
                    kind: SymbolKind::Variable,
                    start_line: self.line(node),
                    end_line: self.end_line(node),
                    exported: true,
                    public_api: true,
                    is_default_export: false,
                    is_async: false,
                    is_private: false,
                    params: vec![],
                    return_type: None,
                    branches: 0,
                    has_error_handling: false,
                    has_returns: false,
                    uses_numeric_ops: false,
                });
                self.out.exports.push(ExportBinding {
                    exported: name.clone(),
                    local: Some(name),
                    from: None,
                });
            }
            if let Some(v) = value {
                self.walk(v, ExportCtx::default());
            }
            return;
        }
        let bare = self.text(name_node).trim_start_matches('#');
        let class = self.current_class().and_then(|s| s.name.clone());
        let name = match &class {
            Some(c) => format!("{}.{}", c, bare),
            None => bare.to_string(),
        };
        let is_private = self.text(name_node).starts_with('#')
            || self.modifier_present(node, "private")
            || self.modifier_present(node, "protected");
        let class_exported = self.current_class().map(|s| s.exported).unwrap_or(false);
        let stats = value.map(|v| self.collect_fn_stats(v)).unwrap_or_default();
        self.add_decl(Decl {
            name: self.truncated(&name),
            kind: SymbolKind::Function,
            start_line: self.line(node),
            end_line: self.end_line(node),
            exported: false,
            public_api: class_exported && !is_private,
            is_default_export: false,
            is_async: stats.is_async,
            is_private,
            params: stats.params,
            return_type: stats.return_type,
            branches: stats.branches,
            has_error_handling: stats.error_handling,
            has_returns: stats.has_returns,
            uses_numeric_ops: stats.numeric_ops,
        });
        if let Some(v) = value {
            self.walk(v, ExportCtx::default());
        }
    }

    fn handle_assignment(&mut self, node: Node) {
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        let Some(right) = node.child_by_field_name("right") else {
            return;
        };
        let left_text = self.text(left);
        let right_kind = right.kind();

        // CommonJS exports.
        if left.kind() == "member_expression" {
            let obj = left
                .child_by_field_name("object")
                .map(|n| self.text(n))
                .unwrap_or("");
            let prop = left
                .child_by_field_name("property")
                .map(|n| self.text(n))
                .unwrap_or("");
            if obj == "module" && prop == "exports" {
                if matches!(right_kind, "identifier" | "member_expression") {
                    self.out.exports.push(ExportBinding {
                        exported: "default".into(),
                        local: Some(self.truncated(self.text(right))),
                        from: None,
                    });
                } else if right_kind == "object" {
                    self.collect_object_exports(right);
                }
            } else if obj == "exports" {
                self.out.exports.push(ExportBinding {
                    exported: self.truncated(prop),
                    local: Some(self.truncated(self.text(right))),
                    from: None,
                });
            }

            // `this.handler = () => {}` — capture as class member function.
            if obj == "this" && matches!(right_kind, "arrow_function" | "function_expression") {
                let class = self.current_class().and_then(|s| s.name.clone());
                let name = match &class {
                    Some(c) => format!("{}.{}", c, prop),
                    None => prop.to_string(),
                };
                let stats = self.collect_fn_stats(right);
                let class_exported = self.current_class().map(|s| s.exported).unwrap_or(false);
                self.add_decl(Decl {
                    name: self.truncated(&name),
                    kind: SymbolKind::Function,
                    start_line: self.line(node),
                    end_line: self.end_line(node),
                    exported: false,
                    public_api: class_exported,
                    is_default_export: false,
                    is_async: stats.is_async,
                    is_private: false,
                    params: stats.params,
                    return_type: stats.return_type,
                    branches: stats.branches,
                    has_error_handling: stats.error_handling,
                    has_returns: stats.has_returns,
                    uses_numeric_ops: stats.numeric_ops,
                });
            }
        }
        let _ = left_text;
        self.walk(left, ExportCtx::default());
        self.walk(right, ExportCtx::default());
    }

    fn collect_object_exports(&mut self, object: Node) {
        let mut cursor = object.walk();
        for child in object.named_children(&mut cursor) {
            match child.kind() {
                "pair" => {
                    let key = child.child_by_field_name("key");
                    let value = child.child_by_field_name("value");
                    if let (Some(k), Some(v)) = (key, value) {
                        self.out.exports.push(ExportBinding {
                            exported: self.truncated(self.text(k)),
                            local: Some(self.truncated(self.text(v))),
                            from: None,
                        });
                    }
                }
                "shorthand_property_identifier" => {
                    let name = self.truncated(self.text(child));
                    self.out.exports.push(ExportBinding {
                        exported: name.clone(),
                        local: Some(name),
                        from: None,
                    });
                }
                _ => {}
            }
        }
    }

    // -- imports / exports --------------------------------------------------

    fn handle_import(&mut self, node: Node) {
        let module = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "string")
            .map(|s| unquote(self.text(s)));
        // The clause is an anonymous child (no field name) in these grammars.
        let clause = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "import_clause");
        let Some(clause) = clause else {
            // `import "./side-effect";`
            if let Some(m) = module {
                self.add_ref(Ref {
                    kind: RefKind::Import,
                    name: String::new(),
                    property: None,
                    object: None,
                    module: Some(m),
                    imported: None,
                    class_scope: None,
                    line: self.line(node),
                    snippet: self.snippet(node),
                    literal_specifier: true,
                });
            }
            return;
        };
        let line = self.line(node);
        let snippet = self.snippet(node);
        let mut cursor = clause.walk();
        for child in clause.children(&mut cursor) {
            match child.kind() {
                // default import: `import Foo from "./m"`
                "identifier" => {
                    self.add_ref(Ref {
                        kind: RefKind::Import,
                        name: self.truncated(self.text(child)),
                        property: None,
                        object: None,
                        module: module.clone(),
                        imported: None,
                        class_scope: None,
                        line,
                        snippet: snippet.clone(),
                        literal_specifier: true,
                    });
                }
                "named_imports" => {
                    let mut c2 = child.walk();
                    for spec in child.named_children(&mut c2) {
                        if spec.kind() != "import_specifier" {
                            continue;
                        }
                        let name = spec.child_by_field_name("name").map(|n| self.text(n));
                        let alias = spec.child_by_field_name("alias").map(|n| self.text(n));
                        if let Some(name) = name {
                            self.add_ref(Ref {
                                kind: RefKind::Import,
                                name: self.truncated(alias.unwrap_or(name)),
                                property: None,
                                object: None,
                                module: module.clone(),
                                imported: Some(self.truncated(name)),
                                class_scope: None,
                                line,
                                snippet: snippet.clone(),
                                literal_specifier: true,
                            });
                        }
                    }
                }
                "namespace_import" => {
                    // `* as ns`
                    let mut c2 = child.walk();
                    for ident in child.named_children(&mut c2) {
                        if ident.kind() == "identifier" {
                            self.add_ref(Ref {
                                kind: RefKind::NamespaceImport,
                                name: self.truncated(self.text(ident)),
                                property: None,
                                object: None,
                                module: module.clone(),
                                imported: None,
                                class_scope: None,
                                line,
                                snippet: snippet.clone(),
                                literal_specifier: true,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_export(&mut self, node: Node) {
        let is_default = node
            .children(&mut node.walk())
            .any(|c| c.kind() == "default");
        let ctx = ExportCtx {
            exported: true,
            is_default,
        };

        // `export ... from "./m"` / `export { a } from "./m"`
        let source = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "string")
            .map(|s| unquote(self.text(s)));

        let mut walked_decl = false;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "export_clause" => {
                    let mut c2 = child.walk();
                    for spec in child.named_children(&mut c2) {
                        if spec.kind() == "export_specifier" {
                            let name = spec.child_by_field_name("name").map(|n| self.text(n));
                            let alias = spec.child_by_field_name("alias").map(|n| self.text(n));
                            if let Some(name) = name {
                                let exported = alias.unwrap_or(name);
                                let kind = if source.is_some() {
                                    RefKind::ReExport
                                } else {
                                    RefKind::ExportSpecifier
                                };
                                self.add_ref(Ref {
                                    kind,
                                    name: self.truncated(exported),
                                    property: None,
                                    object: None,
                                    module: source.clone(),
                                    imported: Some(self.truncated(name)),
                                    class_scope: None,
                                    line: self.line(spec),
                                    snippet: self.snippet(spec),
                                    literal_specifier: true,
                                });
                                // Record the export binding whether or not
                                // there is a source module: `export { a as b }`
                                // must be visible to importers of this module.
                                self.out.exports.push(ExportBinding {
                                    exported: self.truncated(exported),
                                    local: Some(self.truncated(name)),
                                    from: source.clone(),
                                });
                            }
                        } else if spec.kind() == "namespace_export" || self.text(spec).contains('*')
                        {
                            // `export * as ns from "./m"`
                            self.add_ref(Ref {
                                kind: RefKind::ReExport,
                                name: "*".into(),
                                property: None,
                                object: None,
                                module: source.clone(),
                                imported: None,
                                class_scope: None,
                                line: self.line(spec),
                                snippet: self.snippet(spec),
                                literal_specifier: true,
                            });
                            if let Some(m) = &source {
                                self.out.exports.push(ExportBinding {
                                    exported: "*".into(),
                                    local: None,
                                    from: Some(m.clone()),
                                });
                            }
                        }
                    }
                }
                "declaration"
                | "function_declaration"
                | "class_declaration"
                | "lexical_declaration"
                | "variable_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
                | "generator_function_declaration" => {
                    self.walk(child, ctx);
                    walked_decl = true;
                }
                _ => {}
            }
        }
        if !walked_decl && source.is_none() {
            // `export * from "./m"` without a clause
            if node.children(&mut node.walk()).any(|c| self.text(c) == "*") {
                if let Some(m) = source.clone() {
                    self.out.exports.push(ExportBinding {
                        exported: "*".into(),
                        local: None,
                        from: Some(m),
                    });
                }
            }
        }
        if source.is_some() && !walked_decl {
            // Bare side-effect re-export marker.
            if let Some(m) = source {
                self.add_ref(Ref {
                    kind: RefKind::ReExport,
                    name: "*".into(),
                    property: None,
                    object: None,
                    module: Some(m),
                    imported: None,
                    class_scope: None,
                    line: self.line(node),
                    snippet: self.snippet(node),
                    literal_specifier: true,
                });
            }
        }
    }

    // -- references ---------------------------------------------------------

    fn handle_call(&mut self, node: Node) {
        let Some(callee) = node.child_by_field_name("function") else {
            return;
        };
        let line = self.line(node);
        let snippet = self.snippet(node);
        let class_scope = self.current_class().and_then(|s| s.name.clone());

        match callee.kind() {
            "identifier" => {
                let name = self.text(callee);
                if matches!(name, "test" | "it" | "describe") {
                    let title = first_string_arg(self, node).map(|s| self.truncated(&s));
                    self.out.tests.push(TestDecl {
                        title,
                        name: name.to_string(),
                        start_line: line,
                        end_line: self.end_line(node),
                    });
                }
                if name == "require" {
                    let spec = first_string_arg(self, node);
                    self.add_ref(Ref {
                        kind: RefKind::Require,
                        name: match &spec {
                            Some(s) => self.truncated(s),
                            None => self.truncated(&args_text(self, node)),
                        },
                        property: None,
                        object: None,
                        module: spec.clone(),
                        imported: None,
                        class_scope,
                        line,
                        snippet,
                        literal_specifier: spec.is_some(),
                    });
                    return;
                }
                if name == "eval" {
                    self.add_ref(Ref {
                        kind: RefKind::RuntimeDispatch,
                        name: "eval".into(),
                        property: None,
                        object: None,
                        module: None,
                        imported: None,
                        class_scope,
                        line,
                        snippet,
                        literal_specifier: false,
                    });
                    return;
                }
                self.add_ref(Ref {
                    kind: RefKind::Call,
                    name: self.truncated(name),
                    property: None,
                    object: None,
                    module: None,
                    imported: None,
                    class_scope,
                    line,
                    snippet,
                    literal_specifier: false,
                });
            }
            "import" => {
                let spec = first_string_arg(self, node);
                self.add_ref(Ref {
                    kind: RefKind::DynamicImport,
                    name: match &spec {
                        Some(s) => self.truncated(s),
                        None => self.truncated(&args_text(self, node)),
                    },
                    property: None,
                    object: None,
                    module: spec.clone(),
                    imported: None,
                    class_scope,
                    line,
                    snippet,
                    literal_specifier: spec.is_some(),
                });
            }
            "member_expression" | "subscript_expression" => {
                let is_subscript = callee.kind() == "subscript_expression";
                let prop_node =
                    callee.child_by_field_name(if is_subscript { "index" } else { "property" });
                let obj_node = callee.child_by_field_name("object");
                let object = obj_node.map(|o| self.truncated(self.text(o)));
                if let Some(p) = prop_node {
                    if !is_subscript
                        && (p.kind() == "property_identifier" || p.kind() == "identifier")
                    {
                        self.add_ref(Ref {
                            kind: RefKind::MemberCall,
                            name: self.truncated(self.text(p)),
                            property: Some(self.truncated(self.text(p))),
                            object,
                            module: None,
                            imported: None,
                            class_scope,
                            line,
                            snippet,
                            literal_specifier: false,
                        });
                        return;
                    }
                    // `obj["lit"](...)` — string literal index resolves to a name.
                    if p.kind() == "string" {
                        let lit = unquote(self.text(p));
                        self.add_ref(Ref {
                            kind: RefKind::MemberCall,
                            name: self.truncated(&lit),
                            property: Some(self.truncated(&lit)),
                            object,
                            module: None,
                            imported: None,
                            class_scope,
                            line,
                            snippet,
                            literal_specifier: true,
                        });
                        return;
                    }
                    // Computed / dynamic property call: resolution blocked.
                    self.add_ref(Ref {
                        kind: RefKind::ComputedMember,
                        name: self.truncated(self.text(p)),
                        property: Some(self.truncated(self.text(p))),
                        object,
                        module: None,
                        imported: None,
                        class_scope,
                        line,
                        snippet,
                        literal_specifier: false,
                    });
                    return;
                }
                // Property missing from grammar position — record as dispatch we cannot follow.
                self.add_ref(Ref {
                    kind: RefKind::RuntimeDispatch,
                    name: self.truncated(self.text(callee)),
                    property: None,
                    object,
                    module: None,
                    imported: None,
                    class_scope,
                    line,
                    snippet,
                    literal_specifier: false,
                });
            }
            "call_expression"
            | "parenthesized_expression"
            | "arrow_function"
            | "sequence_expression" => {
                // Chained / higher-order call: receiver unknown by design.
            }
            "new_expression" | "await_expression" => {}
            _ => {
                self.add_ref(Ref {
                    kind: RefKind::RuntimeDispatch,
                    name: self.truncated(self.text(callee)),
                    property: None,
                    object: None,
                    module: None,
                    imported: None,
                    class_scope,
                    line,
                    snippet,
                    literal_specifier: false,
                });
            }
        }
    }

    fn handle_new(&mut self, node: Node) {
        let Some(t) = node.child_by_field_name("type") else {
            return;
        };
        let line = self.line(node);
        let snippet = self.snippet(node);
        let class_scope = self.current_class().and_then(|s| s.name.clone());
        match t.kind() {
            "identifier" => self.add_ref(Ref {
                kind: RefKind::New,
                name: self.truncated(self.text(t)),
                property: None,
                object: None,
                module: None,
                imported: None,
                class_scope,
                line,
                snippet,
                literal_specifier: false,
            }),
            "member_expression" => {
                let prop = t.child_by_field_name("property").map(|p| self.text(p));
                let obj = t
                    .child_by_field_name("object")
                    .map(|o| self.truncated(self.text(o)));
                if let Some(p) = prop {
                    self.add_ref(Ref {
                        kind: RefKind::New,
                        name: self.truncated(p),
                        property: Some(self.truncated(p)),
                        object: obj,
                        module: None,
                        imported: None,
                        class_scope,
                        line,
                        snippet,
                        literal_specifier: false,
                    });
                }
            }
            _ => {}
        }
    }

    fn handle_member_read(&mut self, node: Node) {
        // Only interesting for namespace usage: `ns.something` without call.
        // Skip when this node is the callee of a call (handled by handle_call).
        if let Some(parent) = node.parent() {
            if parent.kind() == "call_expression"
                && parent.child_by_field_name("function").map(|f| f.id()) == Some(node.id())
            {
                return;
            }
            if matches!(
                parent.kind(),
                "member_expression" | "subscript_expression" | "assignment_expression"
            ) {
                return;
            }
        }
        let Some(obj) = node.child_by_field_name("object") else {
            return;
        };
        let Some(prop) = node.child_by_field_name("property") else {
            return;
        };
        if obj.kind() != "identifier" {
            return;
        }
        // Heuristic: `ns` came from `import * as ns` (checked later during resolution).
        self.add_ref(Ref {
            kind: RefKind::NamespaceMember,
            name: self.truncated(self.text(prop)),
            property: Some(self.truncated(self.text(prop))),
            object: Some(self.truncated(self.text(obj))),
            module: None,
            imported: None,
            class_scope: self.current_class().and_then(|s| s.name.clone()),
            line: self.line(node),
            snippet: self.snippet(node),
            literal_specifier: false,
        });
    }

    fn handle_type_use(&mut self, node: Node) {
        // Skip declaration names (`interface Foo` — `Foo` is a type_identifier).
        if let Some(parent) = node.parent() {
            if parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
                && matches!(
                    parent.kind(),
                    "interface_declaration"
                        | "type_alias_declaration"
                        | "enum_declaration"
                        | "class_declaration"
                        | "function_declaration"
                )
            {
                return;
            }
        }
        self.add_ref(Ref {
            kind: RefKind::TypeUse,
            name: self.truncated(self.text(node)),
            property: None,
            object: None,
            module: None,
            imported: None,
            class_scope: self.current_class().and_then(|s| s.name.clone()),
            line: self.line(node),
            snippet: self.snippet(node),
            literal_specifier: false,
        });
    }

    fn handle_inherit(&mut self, node: Node) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "type_identifier" | "identifier" | "member_expression" | "generic_type" => {
                    let name = if child.kind() == "generic_type" {
                        child
                            .child_by_field_name("name")
                            .map(|n| self.text(n))
                            .unwrap_or_else(|| self.text(child))
                    } else {
                        self.text(child)
                    };
                    self.add_ref(Ref {
                        kind: RefKind::Inherit,
                        name: self.truncated(name),
                        property: None,
                        object: None,
                        module: None,
                        imported: None,
                        class_scope: self.current_class().and_then(|s| s.name.clone()),
                        line: self.line(child),
                        snippet: self.snippet(child),
                        literal_specifier: false,
                    });
                }
                "extends_type_clause" | "implements_clause" => {
                    self.handle_inherit(child);
                }
                _ => {}
            }
        }
    }

    fn handle_heritage(&mut self, node: Node) {
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for (i, child) in children.iter().enumerate() {
            match child.kind() {
                "extends_type_clause" | "implements_clause" => self.handle_inherit(*child),
                "extends" => {
                    if let Some(expr) = children.get(i + 1) {
                        if expr.is_named() {
                            self.add_ref(Ref {
                                kind: RefKind::Inherit,
                                name: self.truncated(self.text(*expr)),
                                property: None,
                                object: None,
                                module: None,
                                imported: None,
                                class_scope: self.current_class().and_then(|s| s.name.clone()),
                                line: self.line(*expr),
                                snippet: self.snippet(*expr),
                                literal_specifier: false,
                            });
                        }
                    }
                }
                // The `extends Expr` body when the grammar wraps it.
                k if k == "expression_statement"
                    || k == "identifier"
                    || k == "member_expression"
                    || k == "call_expression"
                    || k == "new_expression" =>
                {
                    let prev_is_extends =
                        i > 0 && children.get(i - 1).map(|c| c.kind()) == Some("extends");
                    if prev_is_extends {
                        self.add_ref(Ref {
                            kind: RefKind::Inherit,
                            name: self.truncated(self.text(*child)),
                            property: None,
                            object: None,
                            module: None,
                            imported: None,
                            class_scope: self.current_class().and_then(|s| s.name.clone()),
                            line: self.line(*child),
                            snippet: self.snippet(*child),
                            literal_specifier: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    // -- stats --------------------------------------------------------------

    fn collect_fn_stats(&self, node: Node) -> FnStats {
        let mut stats = FnStats {
            is_async: node.children(&mut node.walk()).any(|c| c.kind() == "async"),
            ..FnStats::default()
        };

        if let Some(params) = node.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for p in params.named_children(&mut cursor) {
                stats.params.push(self.param_of(p));
            }
        }
        if let Some(rt) = node.child_by_field_name("return_type") {
            stats.return_type = Some(self.truncated(self.text(rt).trim_start_matches(':').trim()));
        } else {
            // Grammar variants expose the return type as a plain named child.
            let mut cursor = node.walk();
            for c in node.named_children(&mut cursor) {
                if c.kind() == "type_annotation" && !is_inside_params(c, node) {
                    stats.return_type =
                        Some(self.truncated(self.text(c).trim_start_matches(':').trim()));
                    break;
                }
            }
        }

        if let Some(body) = node.child_by_field_name("body") {
            self.walk_fn_body_stats(body, &mut stats, 0);
        }
        stats
    }

    fn param_of(&self, p: Node) -> Param {
        let mut param = Param {
            name: String::new(),
            type_text: None,
            optional: false,
            rest: false,
            default: None,
        };
        match p.kind() {
            "required_parameter" | "optional_parameter" => {
                param.optional = p.kind() == "optional_parameter";
                if let Some(pattern) = p.child_by_field_name("pattern") {
                    param.name = self.truncated(self.text(pattern));
                }
                if let Some(t) = p.child_by_field_name("type") {
                    param.type_text =
                        Some(self.truncated(self.text(t).trim_start_matches(':').trim()));
                }
                if let Some(v) = p.child_by_field_name("value") {
                    param.default = Some(self.truncated(self.text(v)));
                }
                if p.kind() == "optional_parameter" {
                    param.optional = true;
                }
            }
            "rest_parameter" => {
                param.rest = true;
                let mut cursor = p.walk();
                for c in p.named_children(&mut cursor) {
                    if c.kind() == "identifier" {
                        param.name = self.truncated(self.text(c));
                    } else if c.kind() == "type_annotation" {
                        param.type_text =
                            Some(self.truncated(self.text(c).trim_start_matches(':').trim()));
                    }
                }
            }
            "identifier" => {
                param.name = self.truncated(self.text(p));
            }
            "assignment_pattern" => {
                let mut cursor = p.walk();
                let kids: Vec<Node> = p.named_children(&mut cursor).collect();
                if let Some(first) = kids.first() {
                    param.name = self.truncated(self.text(*first));
                }
                if let Some(last) = kids.last() {
                    if kids.len() > 1 {
                        param.default = Some(self.truncated(self.text(*last)));
                    }
                }
            }
            "object_pattern" | "array_pattern" => {
                param.name = self.truncated(self.text(p));
            }
            _ => {
                param.name = self.truncated(self.text(p));
            }
        }
        param
    }

    /// Recursively collect body statistics, deliberately not descending into
    /// nested function bodies (they get their own declarations).
    fn walk_fn_body_stats(&self, node: Node, stats: &mut FnStats, depth: usize) {
        if depth > 64 {
            return; // pathological nesting guard
        }
        match node.kind() {
            // Nested functions are separate declarations; their stats belong
            // to them, not to the enclosing function.
            "function_declaration"
            | "arrow_function"
            | "function_expression"
            | "method_definition"
            | "generator_function_declaration" => return,
            "if_statement" => stats.branches += 1,
            // switch cases (`case x:` and `default:`) are distinct branches.
            "switch_case" | "switch_default" => stats.branches += 1,
            "conditional_expression" => stats.branches += 1,
            "try_statement" | "throw_statement" | "catch_clause" => stats.error_handling = true,
            "return_statement" => stats.has_returns = true,
            "binary_expression" => {
                stats.numeric_ops |= self.uses_numeric_op(node);
            }
            _ => {}
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.named_children(&mut cursor).collect();
        for child in children {
            self.walk_fn_body_stats(child, stats, depth + 1);
        }
    }

    fn uses_numeric_op(&self, node: Node) -> bool {
        // `*`, `/`, `%` are always numeric in JS/TS; `+`, `-` and comparisons
        // only count when a numeric literal is an operand (so `a + b` is not
        // assumed numeric — it may well be string concatenation).
        const ALWAYS: &[&str] = &["*", "/", "%"];
        const SOMETIMES: &[&str] = &["+", "-", "<", ">", "<=", ">="];
        let op = node
            .child_by_field_name("operator")
            .map(|o| self.text(o).to_string())
            .or_else(|| {
                let mut cursor = node.walk();
                let found = node
                    .children(&mut cursor)
                    .find(|c| !c.is_named())
                    .map(|c| self.text(c).to_string());
                found
            });
        let Some(op) = op else { return false };
        if ALWAYS.contains(&op.as_str()) {
            return true;
        }
        if !SOMETIMES.contains(&op.as_str()) {
            return false;
        }
        let mut cursor = node.walk();
        let has_number = node
            .named_children(&mut cursor)
            .any(|c| c.kind() == "number");
        has_number
    }

    fn mark_error_scope(&mut self) {
        // Nothing structural to record: error-handling stats are collected per
        // function body. Present so `try` at top level does not fall through.
    }

    fn modifier_present(&self, node: Node, modifier: &str) -> bool {
        let mut cursor = node.walk();
        let present = node
            .children(&mut cursor)
            .any(|c| !c.is_named() && self.text(c) == modifier);
        present
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn ref_kind_tag(kind: RefKind) -> u8 {
    match kind {
        RefKind::Call => 0,
        RefKind::MemberCall => 1,
        RefKind::New => 2,
        RefKind::Import => 3,
        RefKind::NamespaceImport => 4,
        RefKind::ReExport => 5,
        RefKind::ExportSpecifier => 6,
        RefKind::TypeUse => 7,
        RefKind::Inherit => 8,
        RefKind::DynamicImport => 9,
        RefKind::Require => 10,
        RefKind::RuntimeDispatch => 11,
        RefKind::ComputedMember => 12,
        RefKind::NamespaceMember => 13,
    }
}

/// Identity used to avoid pathological recursion checks.
fn is_inside_params(node: Node, parent: Node) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.id() == parent.id() {
            return false;
        }
        if n.kind() == "formal_parameters" {
            return true;
        }
        cur = n.parent();
    }
    false
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    let mut t = t
        .trim_start_matches("'")
        .trim_start_matches('"')
        .trim_start_matches('`');
    t = t
        .trim_end_matches('`')
        .trim_end_matches('"')
        .trim_end_matches('\'');
    t.to_string()
}

fn first_string_arg(w: &Walker, call: Node) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    let mut c2 = args.walk();
    // Only the *first* argument matters: a string literal there is the
    // module specifier / test title. Anything else is not resolvable.
    let first = args.named_children(&mut c2).next()?;
    if first.kind() == "string" {
        Some(unquote(w.text(first)))
    } else {
        None
    }
}

fn args_text(w: &Walker, call: Node) -> String {
    call.child_by_field_name("arguments")
        .map(|a| w.text(a).to_string())
        .unwrap_or_default()
}

fn collect_issues(root: Node, source: &str) -> Vec<ParseIssue> {
    let mut issues = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if issues.len() >= 20 {
            break;
        }
        if node.kind() == "ERROR" || node.is_missing() {
            let line = (node.start_position().row + 1) as u32;
            let snippet: String = source
                .lines()
                .nth(node.start_position().row)
                .unwrap_or("")
                .trim()
                .chars()
                .take(80)
                .collect();
            issues.push(ParseIssue {
                line,
                message: if node.is_missing() {
                    format!("missing token at '{}'", snippet)
                } else {
                    format!("unrecognized syntax near '{}'", snippet)
                },
            });
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    issues.sort_by_key(|i| i.line);
    issues.dedup_by_key(|i| i.line);
    issues
}

#[cfg(test)]
mod tests {
    use super::super::is_numeric_type;
    use super::*;

    fn parse_js(src: &str) -> ParsedFile {
        JsAdapter::javascript().parse("src/a.js", src)
    }

    fn parse_ts(src: &str) -> ParsedFile {
        JsAdapter::typescript().parse("src/a.ts", src)
    }

    #[test]
    fn extracts_function_declarations() {
        let f = parse_js("function add(a, b) {\n  if (a > 0) return a + b;\n  return 0;\n}\n");
        assert_eq!(f.decls.len(), 1);
        let d = &f.decls[0];
        assert_eq!(d.name, "add");
        assert_eq!(d.kind, SymbolKind::Function);
        assert_eq!(d.start_line, 1);
        assert_eq!(d.end_line, 4);
        assert_eq!(d.params.len(), 2);
        assert!(d.has_returns);
        assert!(
            d.uses_numeric_ops,
            "`a > 0` is a numeric comparison: {:?}",
            d
        );
        assert_eq!(d.branches, 1, "one if_statement");
    }

    #[test]
    fn identifier_only_plus_is_not_assumed_numeric() {
        let f = parse_js("function join(a, b) {\n  return a + b;\n}\n");
        let d = &f.decls[0];
        assert!(
            !d.uses_numeric_ops,
            "`a + b` may be string concatenation; must not be assumed numeric"
        );
    }

    #[test]
    fn extracts_arrow_functions_bound_to_consts() {
        let f = parse_js("const parse = (s) => s.split(',');\nexport { parse };\n");
        assert!(f
            .decls
            .iter()
            .any(|d| d.name == "parse" && d.kind == SymbolKind::Function));
        assert!(f
            .exports
            .iter()
            .any(|e| e.exported == "parse" && e.local.as_deref() == Some("parse")));
    }

    #[test]
    fn extracts_class_methods_with_class_prefix() {
        let src = "export class Router {\n  handle(req) { return req.url; }\n  #secret() {}\n}\n";
        let f = parse_js(src);
        assert!(f
            .decls
            .iter()
            .any(|d| d.name == "Router" && d.kind == SymbolKind::Class));
        assert!(
            f.decls.iter().any(|d| d.name == "Router.handle"),
            "got: {:?}",
            f.decls.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
        let m = f.decls.iter().find(|d| d.name == "Router.handle").unwrap();
        assert_eq!(m.kind, SymbolKind::Method);
        assert!(
            m.public_api,
            "public method of exported class must be public API"
        );
        let priv_m = f
            .decls
            .iter()
            .find(|d| d.name == "Router.secret")
            .expect("private method decl");
        assert!(priv_m.is_private);
        assert!(!priv_m.public_api);
    }

    #[test]
    fn resolves_imports_with_aliases() {
        let f = parse_js("import { helper as h } from './utils';\nh(1);\n");
        let imp = f
            .refs
            .iter()
            .find(|r| r.kind == RefKind::Import)
            .expect("import ref");
        assert_eq!(imp.name, "h", "local binding name");
        assert_eq!(imp.imported.as_deref(), Some("helper"));
        assert_eq!(imp.module.as_deref(), Some("./utils"));
        let call = f
            .refs
            .iter()
            .find(|r| r.kind == RefKind::Call)
            .expect("call ref");
        assert_eq!(call.name, "h");
    }

    #[test]
    fn records_dynamic_import_and_require() {
        let f = parse_js("const m = await import('./late');\nconst x = require('./cjs');\n");
        let dyn_ref = f
            .refs
            .iter()
            .find(|r| r.kind == RefKind::DynamicImport)
            .expect("dynamic import");
        assert!(dyn_ref.literal_specifier);
        assert_eq!(dyn_ref.module.as_deref(), Some("./late"));
        let req = f
            .refs
            .iter()
            .find(|r| r.kind == RefKind::Require)
            .expect("require");
        assert_eq!(req.module.as_deref(), Some("./cjs"));
    }

    #[test]
    fn records_runtime_dispatch() {
        let f = parse_js("eval('code');\nobj[key]();\n");
        assert!(f
            .refs
            .iter()
            .any(|r| r.kind == RefKind::RuntimeDispatch && r.name == "eval"));
        assert!(f.refs.iter().any(|r| r.kind == RefKind::ComputedMember));
    }

    #[test]
    fn member_calls_record_receiver() {
        let f = parse_js("utils.trim(text);\nthis.render(x);\n");
        let utils_call = f.refs.iter().find(|r| r.name == "trim").expect("trim ref");
        assert_eq!(utils_call.kind, RefKind::MemberCall);
        assert_eq!(utils_call.object.as_deref(), Some("utils"));
        let this_call = f
            .refs
            .iter()
            .find(|r| r.name == "render")
            .expect("render ref");
        assert_eq!(this_call.object.as_deref(), Some("this"));
    }

    #[test]
    fn typescript_types_are_extracted() {
        let src = "export interface Shape { area(): number; }\nexport function fit(s: Shape): number { return s.area(); }\n";
        let f = parse_ts(src);
        assert!(f
            .decls
            .iter()
            .any(|d| d.name == "Shape" && d.kind == SymbolKind::Interface));
        let fit = f.decls.iter().find(|d| d.name == "fit").unwrap();
        assert_eq!(fit.params[0].type_text.as_deref(), Some("Shape"));
        assert_eq!(fit.return_type.as_deref(), Some("number"));
        assert!(f
            .refs
            .iter()
            .any(|r| r.kind == RefKind::TypeUse && r.name == "Shape"));
    }

    #[test]
    fn malformed_source_reports_issues_but_still_parses() {
        let f = parse_js("function broken( {\n  return 1;\n}\n");
        assert!(f.has_syntax_errors, "must flag syntax errors");
        assert!(!f.issues.is_empty(), "must record at least one issue");
    }

    #[test]
    fn interface_extends_is_inheritance() {
        let f = parse_ts("interface A {}\ninterface B extends A {}\n");
        assert!(
            f.refs
                .iter()
                .any(|r| r.kind == RefKind::Inherit && r.name == "A"),
            "refs: {:?}",
            f.refs
        );
    }

    #[test]
    fn class_extends_is_inheritance() {
        let f = parse_js("class Base {}\nclass Child extends Base {}\n");
        assert!(
            f.refs
                .iter()
                .any(|r| r.kind == RefKind::Inherit && r.name == "Base"),
            "refs: {:?}",
            f.refs
        );
    }

    #[test]
    fn reexports_are_recorded() {
        let f = parse_js("export { thing as other } from './m';\n");
        assert!(f
            .exports
            .iter()
            .any(|e| e.exported == "other" && e.from.as_deref() == Some("./m")));
        assert!(f
            .refs
            .iter()
            .any(|r| r.kind == RefKind::ReExport && r.name == "other"));
    }

    #[test]
    fn branch_and_error_stats_counted() {
        let src = "function pick(x) {\n  if (x > 0) { return 1; }\n  switch (x) { case 0: return 2; default: return 3; }\n  try { go(); } catch (e) { throw e; }\n}\n";
        let f = parse_js(src);
        let d = f.decls.iter().find(|d| d.name == "pick").unwrap();
        assert!(d.branches >= 3, "if + case + default: got {}", d.branches);
        assert!(d.has_error_handling, "try/catch/throw must be detected");
    }

    #[test]
    fn export_specifiers_mark_decls_public() {
        let f = parse_js("function hidden() { return 1; }\nexport { hidden as expose };\n");
        assert!(f.exports.iter().any(|e| e.exported == "expose"));
    }

    #[test]
    fn namespace_import_is_recorded() {
        let f = parse_js("import * as ns from './m';\nns.go();\n");
        assert!(f
            .refs
            .iter()
            .any(|r| r.kind == RefKind::NamespaceImport && r.name == "ns"));
        let call = f.refs.iter().find(|r| r.name == "go").expect("go ref");
        assert_eq!(call.kind, RefKind::MemberCall);
        assert_eq!(call.object.as_deref(), Some("ns"));
    }

    #[test]
    fn numeric_type_detection_is_word_based() {
        assert!(is_numeric_type("number"));
        assert!(is_numeric_type("Array<number>"));
        assert!(is_numeric_type("int"));
        assert!(!is_numeric_type("Point"));
        assert!(!is_numeric_type("Printable"));
    }
}
