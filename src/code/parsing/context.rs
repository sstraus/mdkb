//! Parser context for tracking scope during AST traversal.
//!
//! Provides scope tracking utilities that all language parsers use
//! to produce correct [`ScopeContext`] values for extracted symbols.

use crate::code::symbol::ScopeContext;
use crate::code::types::SymbolKind;

/// Scope types that parsers track during AST traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeType {
    /// Global scope (project-wide).
    Global,
    /// Module/file scope.
    Module,
    /// Function or method scope.
    Function {
        /// For JS/TS: whether this function's declarations are hoisted.
        hoisting: bool,
    },
    /// Class, struct, or similar type scope.
    Class,
    /// Block scope (if/for/while/etc).
    Block,
    /// Package or namespace scope.
    Package,
    /// Namespace scope (for languages that support it).
    Namespace,
}

impl ScopeType {
    /// Non-hoisting function scope (default for most languages).
    pub fn function() -> Self {
        ScopeType::Function { hoisting: false }
    }

    /// Hoisting function scope (for JavaScript/TypeScript).
    pub fn hoisting_function() -> Self {
        ScopeType::Function { hoisting: true }
    }
}

/// Tracks current scope during parsing to produce correct [`ScopeContext`] values.
#[derive(Debug, Clone)]
pub struct ParserContext {
    /// Stack of current scopes (innermost last).
    scope_stack: Vec<ScopeType>,
    /// Current class name (if inside a class).
    current_class: Option<String>,
    /// Current function name (if inside a function).
    current_function: Option<String>,
}

impl Default for ParserContext {
    fn default() -> Self {
        Self::new()
    }
}

impl ParserContext {
    /// Create a new parser context starting at module scope.
    pub fn new() -> Self {
        Self {
            scope_stack: vec![ScopeType::Module],
            current_class: None,
            current_function: None,
        }
    }

    /// Enter a new scope.
    pub fn enter_scope(&mut self, scope_type: ScopeType) {
        self.scope_stack.push(scope_type);
    }

    /// Exit the current scope. Never pops the root module scope.
    pub fn exit_scope(&mut self) {
        if self.scope_stack.len() > 1 {
            let exited = self.scope_stack.pop();
            if let Some(scope) = exited {
                match scope {
                    ScopeType::Class => self.current_class = None,
                    ScopeType::Function { .. } => self.current_function = None,
                    _ => {}
                }
            }
        }
    }

    /// Derive the [`ScopeContext`] for the current position.
    pub fn current_scope_context(&self) -> ScopeContext {
        for scope in self.scope_stack.iter().rev() {
            match scope {
                ScopeType::Function { hoisting } => {
                    let (parent_name, parent_kind) = if let Some(func_name) = &self.current_function
                    {
                        (Some(func_name.as_str().into()), Some(SymbolKind::Function))
                    } else if let Some(class_name) = &self.current_class {
                        (Some(class_name.as_str().into()), Some(SymbolKind::Class))
                    } else {
                        (None, None)
                    };
                    return ScopeContext::Local {
                        hoisted: *hoisting,
                        parent_name,
                        parent_kind,
                    };
                }
                ScopeType::Block => {
                    let (parent_name, parent_kind) = if let Some(func_name) = &self.current_function
                    {
                        (Some(func_name.as_str().into()), Some(SymbolKind::Function))
                    } else if let Some(class_name) = &self.current_class {
                        (Some(class_name.as_str().into()), Some(SymbolKind::Class))
                    } else {
                        (None, None)
                    };
                    return ScopeContext::Local {
                        hoisted: false,
                        parent_name,
                        parent_kind,
                    };
                }
                ScopeType::Class => {
                    return ScopeContext::ClassMember {
                        class_name: self.current_class.as_deref().map(Into::into),
                    };
                }
                ScopeType::Package | ScopeType::Namespace => {
                    return ScopeContext::Package;
                }
                ScopeType::Global => {
                    return ScopeContext::Global;
                }
                ScopeType::Module => {}
            }
        }
        ScopeContext::Module
    }

    /// Check if currently inside a class.
    pub fn is_in_class(&self) -> bool {
        self.scope_stack
            .iter()
            .any(|s| matches!(s, ScopeType::Class))
    }

    /// Check if currently inside a function.
    pub fn is_in_function(&self) -> bool {
        self.scope_stack
            .iter()
            .any(|s| matches!(s, ScopeType::Function { .. }))
    }

    /// Check if at module level (not inside class or function).
    pub fn is_module_level(&self) -> bool {
        !self.is_in_class() && !self.is_in_function()
    }

    pub fn set_current_class(&mut self, name: Option<String>) {
        self.current_class = name;
    }

    pub fn set_current_function(&mut self, name: Option<String>) {
        self.current_function = name;
    }

    pub fn current_class(&self) -> Option<&str> {
        self.current_class.as_deref()
    }

    pub fn current_function(&self) -> Option<&str> {
        self.current_function.as_deref()
    }

    /// Scope context for a parameter symbol.
    pub fn parameter_scope_context() -> ScopeContext {
        ScopeContext::Parameter
    }

    /// Scope context for a global/builtin symbol.
    pub fn global_scope_context() -> ScopeContext {
        ScopeContext::Global
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_context() {
        let ctx = ParserContext::new();
        assert_eq!(ctx.current_scope_context(), ScopeContext::Module);
        assert!(ctx.is_module_level());
        assert!(!ctx.is_in_class());
        assert!(!ctx.is_in_function());
    }

    #[test]
    fn test_class_scope() {
        let mut ctx = ParserContext::new();
        ctx.enter_scope(ScopeType::Class);
        ctx.set_current_class(Some("MyClass".to_string()));

        // The name, not just the variant: a member of nothing is what this
        // used to record, and `matches!` could not tell the difference.
        assert_eq!(
            ctx.current_scope_context(),
            ScopeContext::ClassMember {
                class_name: Some("MyClass".into()),
            }
        );
        assert!(ctx.is_in_class());
        assert!(!ctx.is_module_level());
        assert_eq!(ctx.current_class(), Some("MyClass"));

        ctx.exit_scope();
        assert_eq!(ctx.current_scope_context(), ScopeContext::Module);
        assert!(ctx.is_module_level());
        assert_eq!(ctx.current_class(), None);
    }

    #[test]
    fn test_function_scope() {
        let mut ctx = ParserContext::new();
        ctx.enter_scope(ScopeType::Function { hoisting: false });
        ctx.set_current_function(Some("my_func".to_string()));

        assert_eq!(
            ctx.current_scope_context(),
            ScopeContext::Local {
                hoisted: false,
                parent_name: Some("my_func".into()),
                parent_kind: Some(SymbolKind::Function),
            }
        );
        assert!(ctx.is_in_function());
        assert!(!ctx.is_module_level());

        ctx.exit_scope();
        assert_eq!(ctx.current_scope_context(), ScopeContext::Module);
    }

    /// An anonymous type scope must report no owner rather than borrow the name
    /// of whatever type was open around it.
    #[test]
    fn a_class_scope_with_no_name_records_no_owner() {
        let mut ctx = ParserContext::new();
        ctx.enter_scope(ScopeType::Class);

        assert_eq!(
            ctx.current_scope_context(),
            ScopeContext::ClassMember { class_name: None }
        );
    }

    #[test]
    fn test_nested_scopes() {
        let mut ctx = ParserContext::new();
        ctx.enter_scope(ScopeType::Class);
        assert!(matches!(
            ctx.current_scope_context(),
            ScopeContext::ClassMember { .. }
        ));

        ctx.enter_scope(ScopeType::Function { hoisting: false });
        assert!(ctx.is_in_class());
        assert!(ctx.is_in_function());

        ctx.exit_scope();
        assert!(matches!(
            ctx.current_scope_context(),
            ScopeContext::ClassMember { .. }
        ));

        ctx.exit_scope();
        assert_eq!(ctx.current_scope_context(), ScopeContext::Module);
    }

    #[test]
    fn test_hoisted_function() {
        let mut ctx = ParserContext::new();
        ctx.enter_scope(ScopeType::Function { hoisting: true });

        assert_eq!(
            ctx.current_scope_context(),
            ScopeContext::Local {
                hoisted: true,
                parent_name: None,
                parent_kind: None,
            }
        );
    }

    #[test]
    fn test_package_scope() {
        let mut ctx = ParserContext::new();
        ctx.enter_scope(ScopeType::Package);
        assert_eq!(ctx.current_scope_context(), ScopeContext::Package);
    }

    #[test]
    fn test_global_scope() {
        let mut ctx = ParserContext::new();
        ctx.enter_scope(ScopeType::Global);
        assert_eq!(ctx.current_scope_context(), ScopeContext::Global);
    }
}
