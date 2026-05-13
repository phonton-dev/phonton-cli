use std::path::Path;

use anyhow::{anyhow, Context, Result};
use phonton_context::{ContextCompiler, ContextPlanRequest};
use phonton_types::{CodeSlice, ContextPlan};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
struct ContextEvalFixture {
    goal: String,
    #[serde(default)]
    candidate_slices: Vec<CodeSlice>,
    #[serde(default)]
    non_indexed_slices: Vec<CodeSlice>,
    #[serde(default)]
    system_tokens: u64,
    #[serde(default)]
    memory_tokens: u64,
    #[serde(default)]
    attachment_tokens: u64,
    #[serde(default)]
    retry_error_tokens: u64,
    #[serde(default)]
    mcp_tool_tokens: u64,
    target_tokens: Option<u64>,
    #[serde(default = "default_repo_map_items")]
    max_repo_map_items: usize,
}

#[derive(Debug, Clone, Serialize)]
struct ContextEvalReport {
    goal: String,
    selected_symbols: Vec<String>,
    selected_files: Vec<String>,
    plan: ContextPlan,
}

#[derive(Debug, Clone, Serialize)]
struct ContextDiffReport {
    goal: String,
    indexed: ContextEvalReport,
    non_indexed: ContextEvalReport,
    selected_code_token_delta: i64,
    omitted_candidate_token_delta: i64,
}

pub fn run(args: &[String]) -> Result<i32> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print_help();
        return Ok(0);
    }
    match args[0].as_str() {
        "eval" => run_eval(&args[1..]),
        "diff" => run_diff(&args[1..]),
        other => {
            eprintln!("phonton context: unknown command `{other}`");
            print_help();
            Ok(2)
        }
    }
}

fn run_eval(args: &[String]) -> Result<i32> {
    let fixture_path = parse_fixture_path(args)?;
    let fixture = read_fixture(fixture_path)?;
    let report = compile_report(&fixture, &fixture.candidate_slices);
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(0)
}

fn run_diff(args: &[String]) -> Result<i32> {
    let fixture_path = parse_fixture_path(args)?;
    let fixture = read_fixture(fixture_path)?;
    let indexed = compile_report(&fixture, &fixture.candidate_slices);
    let non_indexed = compile_report(&fixture, &fixture.non_indexed_slices);
    let report = ContextDiffReport {
        goal: fixture.goal.clone(),
        selected_code_token_delta: indexed.plan.selected_code_tokens as i64
            - non_indexed.plan.selected_code_tokens as i64,
        omitted_candidate_token_delta: indexed.plan.omitted_code_tokens as i64
            - non_indexed.plan.omitted_code_tokens as i64,
        indexed,
        non_indexed,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(0)
}

fn parse_fixture_path(args: &[String]) -> Result<&Path> {
    let mut path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--format" => {
                i += 1;
                let format = args
                    .get(i)
                    .map(String::as_str)
                    .ok_or_else(|| anyhow!("--format requires a value"))?;
                if format != "json" {
                    return Err(anyhow!("unsupported context output format `{format}`"));
                }
            }
            "--indexed" | "--non-indexed" => {}
            arg if arg.starts_with('-') => {
                return Err(anyhow!("unexpected context argument `{arg}`"));
            }
            value => path = Some(Path::new(value)),
        }
        i += 1;
    }
    path.ok_or_else(|| anyhow!("context command requires a fixture path"))
}

fn read_fixture(path: &Path) -> Result<ContextEvalFixture> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading context fixture {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing context fixture {}", path.display()))
}

fn compile_report(fixture: &ContextEvalFixture, slices: &[CodeSlice]) -> ContextEvalReport {
    let compiled = ContextCompiler::default().compile(ContextPlanRequest {
        goal: &fixture.goal,
        candidate_slices: slices,
        system_tokens: fixture.system_tokens,
        memory_tokens: fixture.memory_tokens,
        attachment_tokens: fixture.attachment_tokens,
        retry_error_tokens: fixture.retry_error_tokens,
        mcp_tool_tokens: fixture.mcp_tool_tokens,
        budget_limit: None,
        target_tokens: fixture.target_tokens,
        max_repo_map_items: fixture.max_repo_map_items,
    });

    ContextEvalReport {
        goal: fixture.goal.clone(),
        selected_symbols: compiled
            .selected_slices
            .iter()
            .map(|slice| slice.symbol_name.clone())
            .collect(),
        selected_files: compiled
            .selected_slices
            .iter()
            .map(|slice| slice.file_path.display().to_string())
            .collect(),
        plan: compiled.plan,
    }
}

fn default_repo_map_items() -> usize {
    12
}

fn print_help() {
    println!(
        "Usage:\n  phonton context eval <fixture.json> [--format json]\n  phonton context diff --indexed --non-indexed <fixture.json> [--format json]\n\nEvaluates deterministic context selection fixtures."
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use phonton_types::SliceOrigin;

    use super::*;

    fn slice(path: &str, symbol: &str, tokens: usize) -> CodeSlice {
        CodeSlice {
            file_path: PathBuf::from(path),
            symbol_name: symbol.into(),
            signature: format!("fn {symbol}()"),
            docstring: None,
            callsites: Vec::new(),
            token_count: tokens,
            origin: SliceOrigin::Semantic,
        }
    }

    #[test]
    fn context_eval_selects_ranked_slices_under_budget() {
        let fixture = ContextEvalFixture {
            goal: "fix alpha".into(),
            candidate_slices: vec![
                slice("src/a.rs", "alpha", 100),
                slice("src/b.rs", "beta", 900),
            ],
            non_indexed_slices: Vec::new(),
            system_tokens: 100,
            memory_tokens: 0,
            attachment_tokens: 0,
            retry_error_tokens: 0,
            mcp_tool_tokens: 0,
            target_tokens: Some(600),
            max_repo_map_items: 1,
        };

        let report = compile_report(&fixture, &fixture.candidate_slices);

        assert!(report.selected_symbols.contains(&"alpha".to_string()));
        assert!(report.plan.omitted_code_tokens > 0);
        assert_eq!(report.plan.target_tokens, 600);
    }
}
