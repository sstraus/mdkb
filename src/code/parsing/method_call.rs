//! Method call representation and per-file resolution data.

use crate::code::types::Range;

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
}
