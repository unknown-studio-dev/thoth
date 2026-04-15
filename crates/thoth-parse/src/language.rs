//! Language registry.
//!
//! Each enabled grammar is wrapped in a [`Language`] value that knows:
//!
//! - its canonical name
//! - the file extensions it claims
//! - the tree-sitter `Language` object
//! - which node kinds mark a symbol boundary and how to extract the name
//! - which node kinds are imports
//!
//! Everything is `Copy`/`Clone` so the registry can be moved between threads
//! cheaply.

use std::path::Path;

use crate::SymbolKind;

/// A registered language.
#[derive(Debug, Clone, Copy)]
pub struct Language {
    inner: LanguageKind,
}

#[derive(Debug, Clone, Copy)]
enum LanguageKind {
    #[cfg(feature = "lang-rust")]
    Rust,
    #[cfg(feature = "lang-python")]
    Python,
    #[cfg(feature = "lang-javascript")]
    JavaScript,
    #[cfg(feature = "lang-typescript")]
    TypeScript,
    #[cfg(feature = "lang-go")]
    Go,
    // Kept to ensure the enum is non-empty even with every feature disabled.
    #[allow(dead_code)]
    _Unreachable,
}

impl Language {
    /// Canonical name.
    pub fn name(self) -> &'static str {
        match self.inner {
            #[cfg(feature = "lang-rust")]
            LanguageKind::Rust => "rust",
            #[cfg(feature = "lang-python")]
            LanguageKind::Python => "python",
            #[cfg(feature = "lang-javascript")]
            LanguageKind::JavaScript => "javascript",
            #[cfg(feature = "lang-typescript")]
            LanguageKind::TypeScript => "typescript",
            #[cfg(feature = "lang-go")]
            LanguageKind::Go => "go",
            LanguageKind::_Unreachable => unreachable!(),
        }
    }

    /// Underlying tree-sitter grammar.
    ///
    /// tree-sitter 0.23 grammar crates expose a `LANGUAGE: LanguageFn`
    /// constant; `.into()` yields a `tree_sitter::Language`.
    pub fn tree_sitter(self) -> tree_sitter::Language {
        match self.inner {
            #[cfg(feature = "lang-rust")]
            LanguageKind::Rust => tree_sitter_rust::LANGUAGE.into(),
            #[cfg(feature = "lang-python")]
            LanguageKind::Python => tree_sitter_python::LANGUAGE.into(),
            #[cfg(feature = "lang-javascript")]
            LanguageKind::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            #[cfg(feature = "lang-typescript")]
            LanguageKind::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            #[cfg(feature = "lang-go")]
            LanguageKind::Go => tree_sitter_go::LANGUAGE.into(),
            LanguageKind::_Unreachable => unreachable!(),
        }
    }

    /// Map a tree-sitter node kind to a [`SymbolKind`] iff this node should
    /// become a chunk boundary.
    pub(crate) fn symbol_kind_for(self, node_kind: &str) -> Option<SymbolKind> {
        match self.inner {
            #[cfg(feature = "lang-rust")]
            LanguageKind::Rust => match node_kind {
                "function_item" | "function_signature_item" => Some(SymbolKind::Function),
                "impl_item" | "struct_item" | "enum_item" | "union_item" => Some(SymbolKind::Type),
                "trait_item" => Some(SymbolKind::Trait),
                "mod_item" => Some(SymbolKind::Module),
                "const_item" | "static_item" => Some(SymbolKind::Binding),
                _ => None,
            },
            #[cfg(feature = "lang-python")]
            LanguageKind::Python => match node_kind {
                "function_definition" => Some(SymbolKind::Function),
                "class_definition" => Some(SymbolKind::Type),
                _ => None,
            },
            #[cfg(feature = "lang-javascript")]
            LanguageKind::JavaScript => match node_kind {
                "function_declaration" | "method_definition" | "arrow_function" => {
                    Some(SymbolKind::Function)
                }
                "class_declaration" => Some(SymbolKind::Type),
                _ => None,
            },
            #[cfg(feature = "lang-typescript")]
            LanguageKind::TypeScript => match node_kind {
                "function_declaration" | "method_definition" => Some(SymbolKind::Function),
                "class_declaration" => Some(SymbolKind::Type),
                "interface_declaration" => Some(SymbolKind::Trait),
                _ => None,
            },
            #[cfg(feature = "lang-go")]
            LanguageKind::Go => match node_kind {
                "function_declaration" | "method_declaration" => Some(SymbolKind::Function),
                "type_declaration" => Some(SymbolKind::Type),
                _ => None,
            },
            LanguageKind::_Unreachable => None,
        }
    }

    /// Extract a human-readable name from a definition node.
    pub(crate) fn extract_name(self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        // Most grammars expose a `name` field on definitions.
        if let Some(name_node) = node.child_by_field_name("name")
            && let Ok(text) = name_node.utf8_text(source)
        {
            return Some(text.to_string());
        }
        // Fallback: scan named children for an identifier.
        let mut cursor = node.walk();
        for c in node.named_children(&mut cursor) {
            if (c.kind().contains("identifier") || c.kind() == "type_identifier")
                && let Ok(text) = c.utf8_text(source)
            {
                return Some(text.to_string());
            }
        }
        None
    }

    /// Whether this node kind represents an import / use.
    pub(crate) fn is_import_node(self, node_kind: &str) -> bool {
        match self.inner {
            #[cfg(feature = "lang-rust")]
            LanguageKind::Rust => {
                matches!(node_kind, "use_declaration" | "extern_crate_declaration")
            }
            #[cfg(feature = "lang-python")]
            LanguageKind::Python => matches!(
                node_kind,
                "import_statement" | "import_from_statement" | "future_import_statement"
            ),
            #[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
            LanguageKind::JavaScript | LanguageKind::TypeScript => {
                matches!(node_kind, "import_statement")
            }
            #[cfg(feature = "lang-go")]
            LanguageKind::Go => matches!(node_kind, "import_declaration"),
            _ => false,
        }
    }

    /// Whether this node kind represents a call / invocation site.
    ///
    /// Includes macro invocations (Rust `foo!(...)`) since those are the way
    /// a lot of "real" cross-module references surface in Rust code.
    pub(crate) fn is_call_node(self, node_kind: &str) -> bool {
        match self.inner {
            #[cfg(feature = "lang-rust")]
            LanguageKind::Rust => matches!(
                node_kind,
                "call_expression" | "method_call_expression" | "macro_invocation"
            ),
            #[cfg(feature = "lang-python")]
            LanguageKind::Python => node_kind == "call",
            #[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
            LanguageKind::JavaScript | LanguageKind::TypeScript => {
                node_kind == "call_expression"
            }
            #[cfg(feature = "lang-go")]
            LanguageKind::Go => node_kind == "call_expression",
            _ => false,
        }
    }

    /// Extract the callee's name from a call-site node.
    ///
    /// The returned string is whatever the grammar exposes as the "thing
    /// being called": for `foo()` it's `"foo"`; for `obj.bar()` it's
    /// `"bar"` (the last segment, since that's what the graph will match
    /// against `SymbolRow::fqn` simple names); for `a::b::c()` it's `"c"`;
    /// for `println!(..)` it's `"println"`.
    pub(crate) fn extract_callee(
        self,
        node: tree_sitter::Node<'_>,
        source: &[u8],
    ) -> Option<String> {
        let raw = match (self.inner, node.kind()) {
            #[cfg(feature = "lang-rust")]
            (LanguageKind::Rust, "call_expression") => {
                node.child_by_field_name("function")?.utf8_text(source).ok()?
            }
            #[cfg(feature = "lang-rust")]
            (LanguageKind::Rust, "method_call_expression") => {
                node.child_by_field_name("method")?.utf8_text(source).ok()?
            }
            #[cfg(feature = "lang-rust")]
            (LanguageKind::Rust, "macro_invocation") => {
                node.child_by_field_name("macro")?.utf8_text(source).ok()?
            }
            #[cfg(feature = "lang-python")]
            (LanguageKind::Python, "call") => {
                node.child_by_field_name("function")?.utf8_text(source).ok()?
            }
            #[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
            (LanguageKind::JavaScript | LanguageKind::TypeScript, "call_expression") => {
                node.child_by_field_name("function")?.utf8_text(source).ok()?
            }
            #[cfg(feature = "lang-go")]
            (LanguageKind::Go, "call_expression") => {
                node.child_by_field_name("function")?.utf8_text(source).ok()?
            }
            _ => return None,
        };
        Some(last_name_segment(raw))
    }
}

/// Take the last identifier segment of a path-ish expression.
///
/// Examples:
/// - `foo::bar::baz` → `baz`
/// - `obj.method`   → `method`
/// - `foo`          → `foo`
/// - `Vec::new`     → `new`
///
/// Stops at the first `<` or `(` so turbofish / argument lists don't leak
/// into the callee name, then returns the last non-empty segment split by
/// `.` or `:`. Empty segments are skipped so `foo::bar` (which tokenises as
/// `["foo", "", "bar"]` when splitting on `:`) still collapses to `bar`.
fn last_name_segment(raw: &str) -> String {
    let s = raw.trim();
    // Strip anything inside `<...>` so turbofish doesn't swallow the real
    // trailing identifier (`Vec::<i32>::new` must collapse to `new`).
    let mut no_gen = String::with_capacity(s.len());
    let mut depth = 0i32;
    for ch in s.chars() {
        match ch {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => no_gen.push(ch),
            _ => {}
        }
    }
    // Drop argument list if the callee text happened to include it.
    let head = no_gen.split('(').next().unwrap_or(&no_gen);
    // Last non-empty segment split by `.` or `:` — skipping the empty
    // pieces `foo::bar` produces when tokenised on single `:`.
    head.rsplit(['.', ':'])
        .find(|p| !p.is_empty())
        .unwrap_or(head)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::last_name_segment;

    #[test]
    fn last_segment_handles_common_forms() {
        assert_eq!(last_name_segment("foo"), "foo");
        assert_eq!(last_name_segment("foo::bar::baz"), "baz");
        assert_eq!(last_name_segment("obj.method"), "method");
        assert_eq!(last_name_segment("Vec::<i32>::new"), "new");
        assert_eq!(last_name_segment("println"), "println");
        assert_eq!(last_name_segment("a::b("), "b");
    }
}

/// Registry of enabled languages. Detection is extension-based.
#[derive(Debug, Default, Clone)]
pub struct LanguageRegistry {
    _private: (),
}

impl LanguageRegistry {
    /// Construct a registry with every compile-enabled language active.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Detect the language of a given path based on its extension.
    ///
    /// Returns `None` if the extension is unknown or the relevant grammar
    /// feature is disabled.
    pub fn detect(&self, path: &Path) -> Option<Language> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            #[cfg(feature = "lang-rust")]
            "rs" => Language {
                inner: LanguageKind::Rust,
            },
            #[cfg(feature = "lang-python")]
            "py" | "pyi" => Language {
                inner: LanguageKind::Python,
            },
            #[cfg(feature = "lang-javascript")]
            "js" | "mjs" | "cjs" | "jsx" => Language {
                inner: LanguageKind::JavaScript,
            },
            #[cfg(feature = "lang-typescript")]
            "ts" | "tsx" => Language {
                inner: LanguageKind::TypeScript,
            },
            #[cfg(feature = "lang-go")]
            "go" => Language {
                inner: LanguageKind::Go,
            },
            _ => return None,
        })
    }
}
