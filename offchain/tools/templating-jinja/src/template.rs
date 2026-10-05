//! # `xyz.taluslabs.templating.jinja@1`
//!
//! Tool that renders templates using minijinja templating engine.

use {
    minijinja::Environment,
    nexus_sdk::{fqn, ToolFqn},
    nexus_toolkit::*,
    schemars::JsonSchema,
    serde::{Deserialize, Serialize},
    std::collections::HashMap,
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    /// The template string to render
    template: String,
    /// Template arguments as a HashMap<String, String>.
    /// Can be used together with name/value parameters.
    #[serde(default)]
    args: HashMap<String, String>,
    /// Optional single value to substitute. Must be used with 'name' parameter.
    /// Can be combined with 'args' parameter.
    value: Option<String>,
    /// Optional name for the single variable. Must be used with 'value' parameter.
    /// Can be combined with 'args' parameter.
    name: Option<String>,
}

/// Output model for the templating tool
#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Output {
    Ok { result: String },
    Err { reason: String },
}

const MAX_INPUT_BYTES: usize = 65_536;
const MAX_OUTPUT_BYTES: usize = 1_048_576;
const MAX_EXPRESSIONS: usize = 256;

// Check intermediate string sizes before MiniJinja evaluates an expression.
// Fuel and a bounded output buffer alone cannot stop `value * huge_number`
// from allocating its intermediate string.
fn expression_bound(
    expr: &minijinja::machinery::ast::Expr<'_>,
    args: &HashMap<String, String>,
    depth: usize,
) -> Result<Option<usize>, String> {
    use minijinja::machinery::ast::{BinOpKind, CallArg, Expr};
    if depth > 32 {
        return Err("Template expression is too deeply nested".into());
    }
    let bound = |expr: &Expr<'_>| expression_bound(expr, args, depth + 1);
    let size = match expr {
        Expr::Var(value) => args.get(value.id).map(String::len),
        Expr::Const(value) => Some(value.value.to_string().len()),
        Expr::BinOp(value) => {
            let left = bound(&value.left)?;
            let right = bound(&value.right)?;
            match value.op {
                BinOpKind::Concat | BinOpKind::Add => {
                    left.zip(right).map(|(a, b)| a.saturating_add(b))
                }
                BinOpKind::Mul => {
                    let count = |expr: &Expr<'_>| match expr {
                        Expr::Const(value) => {
                            value.value.as_i64().and_then(|n| usize::try_from(n).ok())
                        }
                        _ => None,
                    };
                    if let Some(n) = count(&value.right) {
                        left.map(|size| size.saturating_mul(n))
                    } else if let Some(n) = count(&value.left) {
                        right.map(|size| size.saturating_mul(n))
                    } else {
                        return Err("Repetition requires a nonnegative integer literal".into());
                    }
                }
                _ => return Err("Unsupported template operator".into()),
            }
        }
        Expr::Filter(filter) => {
            let Some(expr) = filter.expr.as_ref() else {
                return Err("Missing filter input".into());
            };
            let input = bound(expr)?;
            let mut arguments = Vec::new();
            for argument in &filter.args {
                let CallArg::Pos(expr) = argument else {
                    return Err("Only positional filter arguments are supported".into());
                };
                arguments.push(bound(expr)?);
            }
            match filter.name {
                "upper" | "lower" | "capitalize" | "title" if arguments.is_empty() => {
                    input.map(|n| n.saturating_mul(3))
                }
                "trim" if arguments.len() <= 1 => input,
                "length" | "count" if arguments.is_empty() => input.map(|_| 20),
                "escape" | "e" | "tojson" if arguments.is_empty() => {
                    input.map(|n| n.saturating_mul(6).saturating_add(2))
                }
                "default" | "d" if arguments.len() <= 2 => input
                    .zip(arguments.first().copied().flatten().or(Some(0)))
                    .map(|(a, b)| a.max(b)),
                "replace" if (2..=3).contains(&arguments.len()) => {
                    input.zip(arguments[1]).map(|(n, replacement)| {
                        n.saturating_add(1)
                            .saturating_mul(replacement.saturating_add(1))
                    })
                }
                _ => {
                    return Err(format!(
                        "Unsupported template filter or arguments: {}",
                        filter.name
                    ))
                }
            }
        }
        _ => return Err("Unsupported template expression".into()),
    };
    if size.is_some_and(|size| size > MAX_OUTPUT_BYTES) {
        return Err("Template expression exceeds the output limit".into());
    }
    Ok(size)
}

fn render(template: &str, args: &HashMap<String, String>) -> Result<String, String> {
    use minijinja::machinery::{parse_expr, tokenize, Token};
    let input_size = args.iter().fold(template.len(), |size, (key, value)| {
        size.saturating_add(key.len()).saturating_add(value.len())
    });
    if input_size > MAX_INPUT_BYTES || args.len() > 128 {
        return Err("Template input exceeds the size limit".into());
    }
    let mut env = Environment::new();
    env.set_fuel(Some(10_000));
    env.set_recursion_limit(32);
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    let mut result = String::new();
    let mut expression_start = None;
    let mut expression_count = 0;
    for token in tokenize(template, false, Default::default(), Default::default()) {
        let (token, span) = token.map_err(|e| format!("Template syntax error: {e}"))?;
        let piece = match token {
            Token::TemplateData(text) => Some(text.to_string()),
            Token::VariableStart => {
                expression_start = Some((span.start_offset as usize, span.end_offset as usize));
                None
            }
            Token::VariableEnd => {
                let Some((start, inner)) = expression_start.take() else {
                    return Err("Unexpected end of expression".into());
                };
                expression_count += 1;
                if expression_count > MAX_EXPRESSIONS {
                    return Err("Too many template expressions".into());
                }
                let expression = &template[inner..span.start_offset as usize];
                let ast =
                    parse_expr(expression).map_err(|e| format!("Template syntax error: {e}"))?;
                if expression_bound(&ast, args, 0)?.is_none() {
                    Some(template[start..span.end_offset as usize].to_string())
                } else {
                    let expression = env
                        .compile_expression(expression)
                        .map_err(|e| format!("Template syntax error: {e}"))?;
                    Some(
                        expression
                            .eval(args)
                            .map_err(|e| format!("Template rendering failed: {e}"))?
                            .to_string(),
                    )
                }
            }
            Token::BlockStart => {
                return Err("Template blocks are unsupported; use variable expressions".into())
            }
            _ => None,
        };
        if let Some(piece) = piece {
            if result.len().saturating_add(piece.len()) > MAX_OUTPUT_BYTES {
                return Err("Template output exceeds the size limit".into());
            }
            result.push_str(&piece);
        }
    }
    if expression_start.is_some() {
        return Err("Unclosed template expression".into());
    }
    Ok(result)
}

pub(crate) struct TemplatingJinja;

impl NexusTool for TemplatingJinja {
    type Input = Input;
    type Output = Output;

    async fn new() -> Self {
        Self
    }

    fn fqn() -> ToolFqn {
        fqn!(concat!(
            "xyz.taluslabs.templating.jinja@",
            env!("TOOL_FQN_VERSION")
        ))
    }

    fn path() -> &'static str {
        "/templating-jinja"
    }

    fn description() -> &'static str {
        "Tool that parses templates using Jinja2 templating engine with flexible input options."
    }

    async fn health(&self) -> AnyResult<StatusCode> {
        Ok(StatusCode::OK)
    }

    async fn invoke(&self, input: Self::Input) -> Self::Output {
        let mut all_args = input.args;

        // Validate: if name or value is provided, both must be provided
        match (&input.name, &input.value) {
            (None, None) => (),
            (Some(name), Some(value)) => {
                all_args.insert(name.clone(), value.clone());
            }
            _ => {
                return Output::Err {
                    reason: "name and value must both be provided or both be None".to_string(),
                };
            }
        }

        // Validate: at least one parameter must be provided
        if all_args.is_empty() {
            return Output::Err {
                reason: "Either 'args' or 'name'/'value' parameters must be provided".to_string(),
            };
        }

        match render(&input.template, &all_args) {
            Ok(result) => Output::Ok { result },
            Err(reason) => Output::Err { reason },
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn expressions_use_the_complete_context_once() {
        let args = HashMap::from([("a".into(), "A".into()), ("b".into(), "B".into())]);
        assert_eq!(render("{{ a ~ b }}", &args).unwrap(), "AB");
        for _ in 0..32 {
            let args = HashMap::from([("a".into(), "{{ b }}".into()), ("b".into(), "B".into())]);
            assert_eq!(render("{{ a }}", &args).unwrap(), "{{ b }}");
        }
    }

    #[test]
    fn resource_limits_apply_before_expression_allocation() {
        let args = HashMap::from([("value".into(), "x".repeat(256))]);
        assert!(render("{{ value * 1000000000000 }}", &args).is_err());
        assert!(render("{{ 'x' * 1000000000000 }}", &args).is_err());
        assert!(render("{{ value | center(1000000000000) }}", &args).is_err());
        assert!(render("{{ range(1000000000000) | list }}", &args).is_err());
        assert_eq!(
            render("{{ value * 4096 }}", &args).unwrap().len(),
            MAX_OUTPUT_BYTES
        );
        assert!(render("{{ value * 4096 }}x", &args).is_err());
    }

    #[test]
    fn quoted_delimiters_and_unicode_are_rendered_correctly() {
        let args = HashMap::from([("value".into(), "é".into())]);
        assert_eq!(render("{{ value ~ '}}' }}", &args).unwrap(), "é}}");
        assert_eq!(render("{{ value | tojson }}", &args).unwrap(), "\"é\"");
    }

    use super::*;

    #[tokio::test]
    async fn test_template_with_args_map() {
        let tool = TemplatingJinja::new().await;

        let input = Input {
            template: "Hello {{name}} from {{city}}!".to_string(),
            args: HashMap::from([
                ("name".to_string(), "Alice".to_string()),
                ("city".to_string(), "Paris".to_string()),
            ]),
            value: None,
            name: None,
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { result } => assert_eq!(result, "Hello Alice from Paris!"),
            Output::Err { reason } => panic!("Expected success, got error: {}", reason),
        }
    }

    #[tokio::test]
    async fn test_template_with_name_and_value() {
        let tool = TemplatingJinja::new().await;

        let input = Input {
            template: "Hello {{user}}!".to_string(),
            args: HashMap::new(),
            value: Some("Bob".to_string()),
            name: Some("user".to_string()),
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { result } => assert_eq!(result, "Hello Bob!"),
            Output::Err { reason } => panic!("Expected success, got error: {}", reason),
        }
    }

    #[tokio::test]
    async fn test_template_with_name_and_value_only() {
        let tool = TemplatingJinja::new().await;

        let input = Input {
            template: "Hello {{custom_var}}!".to_string(),
            args: HashMap::new(),
            value: Some("World".to_string()),
            name: Some("custom_var".to_string()),
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { result } => assert_eq!(result, "Hello World!"),
            Output::Err { reason } => panic!("Expected success, got error: {}", reason),
        }
    }

    #[tokio::test]
    async fn test_template_no_parameters_fails() {
        let tool = TemplatingJinja::new().await;

        // Test: No args, no name, no value should fail
        let input = Input {
            template: "Simple template without variables".to_string(),
            args: HashMap::new(),
            value: None,
            name: None,
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { .. } => panic!("Expected error for template with no parameters"),
            Output::Err { reason } => {
                assert!(
                    reason.contains("Either 'args' or 'name'/'value' parameters must be provided")
                )
            }
        }
    }

    #[tokio::test]
    async fn test_template_invalid_args_combination() {
        let tool = TemplatingJinja::new().await;

        // Test: value without name should fail
        let input = Input {
            template: "Hello {{name}}!".to_string(),
            args: HashMap::new(),
            value: Some("World".to_string()),
            name: None,
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { .. } => panic!("Expected error for invalid args combination"),
            Output::Err { reason } => {
                assert!(reason.contains("name and value must both be provided"))
            }
        }
    }

    #[tokio::test]
    async fn test_template_with_undefined_variable_preserves_placeholder() {
        let tool = TemplatingJinja::new().await;

        // Test: Template with undefined variable should preserve placeholder for chaining
        let input = Input {
            template: "Hi, this is {{name}}, from {{city}}".to_string(),
            args: HashMap::from([("name".to_string(), "Pavel".to_string())]),
            value: None,
            name: None,
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { result } => assert_eq!(result, "Hi, this is Pavel, from {{city}}"),
            Output::Err { reason } => panic!("Expected success, got error: {}", reason),
        }
    }

    #[tokio::test]
    async fn test_args_and_name_value_combined() {
        let tool = TemplatingJinja::new().await;

        // Test: args Map + name/value should work together
        let input = Input {
            template: "Hello {{name}} from {{city}}!".to_string(),
            args: HashMap::from([("city".to_string(), "Paris".to_string())]),
            value: Some("Alice".to_string()),
            name: Some("name".to_string()),
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { result } => assert_eq!(result, "Hello Alice from Paris!"),
            Output::Err { reason } => panic!("Expected success, got error: {}", reason),
        }
    }

    #[tokio::test]
    async fn test_value_without_name_fails() {
        let tool = TemplatingJinja::new().await;

        // Test: value without name should fail
        let input = Input {
            template: "Hello {{name}}!".to_string(),
            args: HashMap::from([("name".to_string(), "Alice".to_string())]),
            value: Some("Bob".to_string()),
            name: None,
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { .. } => panic!("Expected error for value without name"),
            Output::Err { reason } => {
                assert!(reason.contains("name and value must both be provided"))
            }
        }
    }

    #[tokio::test]
    async fn test_name_without_value_fails() {
        let tool = TemplatingJinja::new().await;

        // Test: name without value should fail
        let input = Input {
            template: "Hello {{name}}!".to_string(),
            args: HashMap::new(),
            value: None,
            name: Some("name".to_string()),
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { .. } => panic!("Expected error for name without value"),
            Output::Err { reason } => {
                assert!(reason.contains("name and value must both be provided"))
            }
        }
    }

    #[tokio::test]
    async fn test_empty_args_without_name_value_fails() {
        let tool = TemplatingJinja::new().await;

        // Test: empty args without name/value should fail
        let input = Input {
            template: "Hello {{name}}!".to_string(),
            args: HashMap::new(),
            value: None,
            name: None,
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { .. } => panic!("Expected error for empty args without name/value"),
            Output::Err { reason } => {
                assert!(
                    reason.contains("Either 'args' or 'name'/'value' parameters must be provided")
                )
            }
        }
    }

    #[tokio::test]
    async fn test_partial_variables_preserved_for_chaining() {
        let tool = TemplatingJinja::new().await;

        // Test: Multiple undefined variables should all be preserved
        let input = Input {
            template: "Hello {{name}} from {{city}}! Your age is {{age}}.".to_string(),
            args: HashMap::from([("name".to_string(), "Alice".to_string())]),
            value: None,
            name: None,
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { result } => {
                assert_eq!(result, "Hello Alice from {{city}}! Your age is {{age}}.")
            }
            Output::Err { reason } => panic!("Expected success, got error: {}", reason),
        }
    }

    #[tokio::test]
    async fn test_template_with_filters_on_defined_and_undefined_variables() {
        let tool = TemplatingJinja::new().await;

        // Test: Template with filters on both defined and undefined variables
        let input = Input {
            template: "Hello {{name | upper}}, Im from {{ city | upper }}!".to_string(),
            args: HashMap::from([("name".to_string(), "Alice".to_string())]),
            value: None,
            name: None,
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { result } => {
                assert_eq!(result, "Hello ALICE, Im from {{ city | upper }}!")
            }
            Output::Err { reason } => panic!("Expected success, got error: {}", reason),
        }
    }

    #[tokio::test]
    async fn test_template_with_filter_and_simple_undefined_variable() {
        let tool = TemplatingJinja::new().await;

        // Test: Template with filter on defined variable and simple undefined variable
        let input = Input {
            template: "Hello {{name | upper}}, Im from {{city}}!".to_string(),
            args: HashMap::from([("name".to_string(), "Alice".to_string())]),
            value: None,
            name: None,
        };

        let result = tool.invoke(input).await;
        match result {
            Output::Ok { result } => {
                assert_eq!(result, "Hello ALICE, Im from {{city}}!")
            }
            Output::Err { reason } => panic!("Expected success, got error: {}", reason),
        }
    }
}
