//! Method call representation and per-file resolution data.

use crate::code::types::Range;
use std::collections::HashMap;

/// A method call with caller context, receiver, and source location.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodCall {
    /// Function containing this call.
    pub caller: String,
    /// Method being called.
    pub method_name: String,
    /// Receiver expression ("self", variable name, or type for static).
    pub receiver: Option<String>,
    /// Static call (e.g., `String::new`).
    pub is_static: bool,
    /// Call site location.
    pub range: Range,
    /// Definition range of the calling function.
    pub caller_range: Option<Range>,
}

impl MethodCall {
    pub fn new(caller: &str, method_name: &str, range: Range) -> Self {
        Self {
            caller: caller.to_string(),
            method_name: method_name.to_string(),
            receiver: None,
            is_static: false,
            range,
            caller_range: None,
        }
    }

    pub fn with_receiver(mut self, receiver: &str) -> Self {
        self.receiver = Some(receiver.to_string());
        self
    }

    pub fn static_method(mut self) -> Self {
        self.is_static = true;
        self
    }

    pub fn with_caller_range(mut self, range: Range) -> Self {
        self.caller_range = Some(range);
        self
    }

    pub fn is_self_call(&self) -> bool {
        self.receiver.as_deref() == Some("self")
    }

    pub fn is_function_call(&self) -> bool {
        self.receiver.is_none()
    }

    /// Fully qualified display name.
    pub fn qualified_name(&self) -> String {
        match (&self.receiver, self.is_static) {
            (Some(r), true) => format!("{r}::{}", self.method_name),
            (Some(r), false) => format!("{r}.{}", self.method_name),
            (None, _) => self.method_name.clone(),
        }
    }
}

/// Per-file storage for variable types and method calls.
///
/// Populated during parsing, consumed during relationship resolution.
#[derive(Debug, Default)]
pub struct MethodCallResolver {
    variable_types: HashMap<String, String>,
    method_calls: Vec<MethodCall>,
}

impl MethodCallResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_variable_type(&mut self, var: &str, type_name: &str) {
        self.variable_types
            .insert(var.to_string(), type_name.to_string());
    }

    /// Returns `true` if added, `false` if duplicate.
    pub fn add_method_call(&mut self, call: MethodCall) -> bool {
        let exists = self.method_calls.iter().any(|mc| {
            mc.caller == call.caller
                && mc.method_name == call.method_name
                && mc.range.start_line == call.range.start_line
                && mc.range.start_column == call.range.start_column
        });
        if exists {
            return false;
        }
        self.method_calls.push(call);
        true
    }

    pub fn variable_types(&self) -> &HashMap<String, String> {
        &self.variable_types
    }

    pub fn find_method_call(&self, caller: &str, method_name: &str) -> Option<&MethodCall> {
        self.method_calls
            .iter()
            .find(|mc| mc.caller == caller && mc.method_name == method_name)
    }

    pub fn method_calls(&self) -> &[MethodCall] {
        &self.method_calls
    }

    pub fn is_empty(&self) -> bool {
        self.method_calls.is_empty() && self.variable_types.is_empty()
    }

    pub fn clear(&mut self) {
        self.variable_types.clear();
        self.method_calls.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_range() -> Range {
        Range::new(10, 5, 10, 20)
    }

    #[test]
    fn test_method_call_basic() {
        let call = MethodCall::new("main", "process", test_range());
        assert_eq!(call.caller, "main");
        assert!(call.receiver.is_none());
        assert!(call.is_function_call());
    }

    #[test]
    fn test_method_call_with_receiver() {
        let call = MethodCall::new("handler", "clone", test_range()).with_receiver("data");
        assert_eq!(call.receiver.as_deref(), Some("data"));
        assert!(!call.is_function_call());
    }

    #[test]
    fn test_static_method() {
        let call = MethodCall::new("main", "new", test_range())
            .with_receiver("HashMap")
            .static_method();
        assert!(call.is_static);
        assert_eq!(call.qualified_name(), "HashMap::new");
    }

    #[test]
    fn test_self_call() {
        let call = MethodCall::new("foo", "bar", test_range()).with_receiver("self");
        assert!(call.is_self_call());
    }

    #[test]
    fn test_resolver_deduplication() {
        let mut resolver = MethodCallResolver::new();
        let call1 = MethodCall::new("process", "add", Range::new(5, 4, 5, 14));
        let call2 = MethodCall::new("process", "add", Range::new(5, 4, 5, 14));
        assert!(resolver.add_method_call(call1));
        assert!(!resolver.add_method_call(call2));
        assert_eq!(resolver.method_calls().len(), 1);
    }

    #[test]
    fn test_resolver_variable_types() {
        let mut resolver = MethodCallResolver::new();
        resolver.register_variable_type("calc", "Calculator");
        assert_eq!(
            resolver.variable_types().get("calc"),
            Some(&"Calculator".to_string())
        );
    }
}
