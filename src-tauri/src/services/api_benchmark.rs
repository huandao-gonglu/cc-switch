//! OpenAI-compatible API benchmark service.
//!
//! This module ports the standalone Python benchmark into CC Switch's backend.
//! It keeps provider extraction and metric calculation testable without making
//! live network calls.

use crate::app_config::AppType;
use crate::error::AppError;
use crate::provider::Provider;
use futures::StreamExt;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Notify;
use tokio::task::JoinSet;

const CODE_QUESTIONS_JSON: &str = include_str!("api_benchmark_questions.json");
const CODE_ANSWERS_JSON: &str = include_str!("api_benchmark_answers.json");
const DEFAULT_CODE_BENCHMARK_CONCURRENCY: usize = 2;
const MAX_CODE_BENCHMARK_CONCURRENCY: usize = 5;

static ACTIVE_BENCHMARK_RUNS: Lazy<StdMutex<HashMap<String, Arc<ApiBenchmarkRunController>>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

struct ApiBenchmarkRunController {
    concurrency: AtomicUsize,
    notify: Notify,
}

impl ApiBenchmarkRunController {
    fn new(max_concurrency: usize) -> Self {
        Self {
            concurrency: AtomicUsize::new(normalize_code_benchmark_concurrency(max_concurrency)),
            notify: Notify::new(),
        }
    }

    fn current_concurrency(&self) -> usize {
        normalize_code_benchmark_concurrency(self.concurrency.load(Ordering::SeqCst))
    }

    fn update_concurrency(&self, max_concurrency: usize) {
        self.concurrency.store(
            normalize_code_benchmark_concurrency(max_concurrency),
            Ordering::SeqCst,
        );
        self.notify.notify_one();
    }

    async fn wait_for_concurrency_change(&self) {
        self.notify.notified().await;
    }
}

fn register_active_benchmark_run(run_id: &str, controller: Arc<ApiBenchmarkRunController>) {
    if let Ok(mut runs) = ACTIVE_BENCHMARK_RUNS.lock() {
        runs.insert(run_id.to_string(), controller);
    }
}

fn active_benchmark_run(run_id: &str) -> Option<Arc<ApiBenchmarkRunController>> {
    ACTIVE_BENCHMARK_RUNS
        .lock()
        .ok()
        .and_then(|runs| runs.get(run_id).cloned())
}

fn unregister_active_benchmark_run(run_id: &str) {
    if let Ok(mut runs) = ACTIVE_BENCHMARK_RUNS.lock() {
        runs.remove(run_id);
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BenchmarkPromptKind {
    Short,
    Medium,
    Code,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiBenchmarkEntry {
    pub index: usize,
    pub app_type: String,
    pub provider_id: String,
    pub provider_name: String,
    pub model: String,
    pub base_url: String,
    pub api_key_present: bool,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct ApiBenchmarkTarget {
    pub entry: ApiBenchmarkEntry,
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiBenchmarkOptions {
    pub prompt: BenchmarkPromptKind,
    pub runs: u32,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub extra_body: Option<Value>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    #[serde(default)]
    pub progress_run_id: Option<String>,
}

impl Default for ApiBenchmarkOptions {
    fn default() -> Self {
        Self {
            prompt: BenchmarkPromptKind::Medium,
            runs: 1,
            task_id: None,
            extra_body: None,
            timeout_secs: default_timeout_secs(),
            max_concurrency: default_max_concurrency(),
            progress_run_id: None,
        }
    }
}

fn default_timeout_secs() -> u64 {
    180
}

fn default_max_concurrency() -> usize {
    DEFAULT_CODE_BENCHMARK_CONCURRENCY
}

fn normalize_code_benchmark_concurrency(max_concurrency: usize) -> usize {
    max_concurrency.clamp(1, MAX_CODE_BENCHMARK_CONCURRENCY)
}

fn code_benchmark_concurrency(options: &ApiBenchmarkOptions) -> usize {
    normalize_code_benchmark_concurrency(options.max_concurrency)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiBenchmarkResultRow {
    pub row_key: Option<String>,
    pub entry_index: usize,
    pub prompt_kind: BenchmarkPromptKind,
    pub task_id: Option<String>,
    pub task_title: Option<String>,
    pub prompt: String,
    pub response_text: String,
    pub time: String,
    pub label: String,
    pub model_requested: String,
    pub model_returned: String,
    pub service_tier: String,
    pub system_fingerprint: String,
    pub status_code: Option<u16>,
    pub request_id: String,
    pub ttft_sec: Option<f64>,
    pub total_time_sec: f64,
    pub gen_time_sec: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub tokens_per_sec: Option<f64>,
    pub chars: usize,
    pub chars_per_sec: Option<f64>,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_evaluation: Option<CodeEvaluationResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeEvaluationResult {
    pub task_id: Option<String>,
    pub task_title: Option<String>,
    pub code_extracted: bool,
    pub syntax_ok: bool,
    pub runnable: bool,
    pub passed_tests: usize,
    pub total_tests: usize,
    pub case_results: Vec<CodeEvaluationCaseResult>,
    pub score: f64,
    pub execution_time_sec: Option<f64>,
    pub failure_reason: Option<String>,
    pub extracted_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeEvaluationCaseResult {
    pub case_id: String,
    pub passed: bool,
    pub stdin: String,
    pub stdout: String,
    pub stderr: String,
    pub expected_stdout: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiBenchmarkSummary {
    pub label: String,
    pub success: usize,
    pub ttft_sec_median: Option<f64>,
    pub ttft_sec_p90: Option<f64>,
    pub total_time_sec_median: Option<f64>,
    pub total_time_sec_p90: Option<f64>,
    pub tokens_per_sec_median: Option<f64>,
    pub tokens_per_sec_p90: Option<f64>,
    pub chars_per_sec_median: Option<f64>,
    pub chars_per_sec_p90: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiBenchmarkRunResult {
    pub rows: Vec<ApiBenchmarkResultRow>,
    pub summaries: Vec<ApiBenchmarkSummary>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiBenchmarkProgressEvent {
    pub run_id: String,
    pub event: ApiBenchmarkProgressEventKind,
    pub row_key: String,
    pub row: ApiBenchmarkResultRow,
    pub completed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ApiBenchmarkProgressEventKind {
    #[serde(rename = "rowQueued")]
    Queued,
    #[serde(rename = "rowStarted")]
    Started,
    #[serde(rename = "rowCompleted")]
    Completed,
}

pub type ApiBenchmarkProgressEmitter = Arc<dyn Fn(ApiBenchmarkProgressEvent) + Send + Sync>;

pub struct ApiBenchmarkService;

impl ApiBenchmarkService {
    pub fn list_entries(
        app_type: &AppType,
        providers: &IndexMap<String, Provider>,
    ) -> Vec<ApiBenchmarkEntry> {
        Self::targets_for_providers(app_type, providers)
            .into_iter()
            .map(|target| target.entry)
            .enumerate()
            .map(|(index, mut entry)| {
                entry.index = index;
                entry
            })
            .collect()
    }

    pub fn resolve_targets(
        app_type: &AppType,
        providers: &IndexMap<String, Provider>,
        indices: &[usize],
    ) -> Result<Vec<ApiBenchmarkTarget>, AppError> {
        let targets = Self::targets_for_providers(app_type, providers);
        let mut selected = Vec::new();

        for index in indices {
            let mut target = targets.get(*index).cloned().ok_or_else(|| {
                AppError::Message(format!("API benchmark index {index} does not exist"))
            })?;
            target.entry.index = *index;
            selected.push(target);
        }

        Ok(selected)
    }

    pub fn resolve_targets_for_provider(
        app_type: &AppType,
        providers: &IndexMap<String, Provider>,
        provider_id: &str,
    ) -> Result<Vec<ApiBenchmarkTarget>, AppError> {
        let targets: Vec<ApiBenchmarkTarget> = Self::targets_for_providers(app_type, providers)
            .into_iter()
            .filter(|target| target.entry.provider_id == provider_id)
            .collect();
        if targets.is_empty() {
            return Err(AppError::Message(format!(
                "Provider `{provider_id}` does not have testable API benchmark targets"
            )));
        }
        Ok(targets)
    }

    pub fn update_concurrency(run_id: &str, max_concurrency: usize) -> bool {
        if let Some(controller) = active_benchmark_run(run_id) {
            log::info!(
                "[ApiBenchmark] update concurrency run_id={run_id} max_concurrency={}",
                normalize_code_benchmark_concurrency(max_concurrency)
            );
            controller.update_concurrency(max_concurrency);
            true
        } else {
            log::warn!(
                "[ApiBenchmark] update concurrency ignored run_id={run_id} reason=not_found"
            );
            false
        }
    }

    pub async fn run_targets(
        targets: Vec<ApiBenchmarkTarget>,
        options: ApiBenchmarkOptions,
    ) -> ApiBenchmarkRunResult {
        Self::run_targets_with_progress(targets, options, None).await
    }

    pub async fn run_targets_with_progress(
        targets: Vec<ApiBenchmarkTarget>,
        options: ApiBenchmarkOptions,
        progress: Option<ApiBenchmarkProgressEmitter>,
    ) -> ApiBenchmarkRunResult {
        let runs = options.runs.max(1);
        let mut rows = Vec::new();
        let total = total_rows_for_targets(&targets, &options, runs);
        let completed = Arc::new(AtomicUsize::new(0));
        let run_controller = if options.prompt == BenchmarkPromptKind::Code {
            Some(Arc::new(ApiBenchmarkRunController::new(
                code_benchmark_concurrency(&options),
            )))
        } else {
            None
        };
        let active_run_id = match (options.progress_run_id.as_deref(), run_controller.as_ref()) {
            (Some(run_id), Some(controller)) => {
                register_active_benchmark_run(run_id, controller.clone());
                Some(run_id.to_string())
            }
            _ => None,
        };

        for target in targets {
            if options.prompt == BenchmarkPromptKind::Code {
                let tasks = code_tasks_for_options(&options)
                    .map_err(AppError::Message)
                    .unwrap_or_default();
                let Some(controller) = run_controller.as_ref().cloned() else {
                    continue;
                };
                rows.extend(
                    Self::run_code_tasks_for_target(
                        target,
                        options.clone(),
                        tasks,
                        progress.clone(),
                        completed.clone(),
                        total,
                        controller,
                    )
                    .await,
                );
            } else {
                for run_index in 0..runs {
                    let row_key = benchmark_row_key(
                        target.entry.index,
                        options.prompt,
                        &format!("run-{run_index}"),
                    );
                    emit_progress(
                        progress.as_ref(),
                        options.progress_run_id.as_deref(),
                        ApiBenchmarkProgressEventKind::Started,
                        row_key.clone(),
                        pending_result_row(&target, &options, None, row_key.clone()),
                        completed.load(Ordering::SeqCst),
                        total,
                    );
                    let row = Self::run_once(&target, &options, None, row_key.clone()).await;
                    let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
                    emit_progress(
                        progress.as_ref(),
                        options.progress_run_id.as_deref(),
                        ApiBenchmarkProgressEventKind::Completed,
                        row_key,
                        row.clone(),
                        done,
                        total,
                    );
                    rows.push(row);
                }
            }
        }

        if let Some(run_id) = active_run_id.as_deref() {
            unregister_active_benchmark_run(run_id);
        }

        let summaries = summarize_rows(&rows);
        ApiBenchmarkRunResult { rows, summaries }
    }

    async fn run_code_tasks_for_target(
        target: ApiBenchmarkTarget,
        options: ApiBenchmarkOptions,
        tasks: Vec<CodeTaskDefinition>,
        progress: Option<ApiBenchmarkProgressEmitter>,
        completed: Arc<AtomicUsize>,
        total: usize,
        controller: Arc<ApiBenchmarkRunController>,
    ) -> Vec<ApiBenchmarkResultRow> {
        let mut queued_tasks = Vec::new();
        for task in tasks {
            let row_key =
                benchmark_row_key(target.entry.index, options.prompt, task.task_id.as_str());
            let queued_row = pending_result_row(&target, &options, Some(&task), row_key.clone());
            emit_progress(
                progress.as_ref(),
                options.progress_run_id.as_deref(),
                ApiBenchmarkProgressEventKind::Queued,
                row_key.clone(),
                queued_row,
                completed.load(Ordering::SeqCst),
                total,
            );
            queued_tasks.push((task, row_key));
        }

        let mut next_task_index = 0usize;
        let mut running_count = 0usize;
        let mut running = JoinSet::<(usize, ApiBenchmarkResultRow)>::new();
        let mut rows = Vec::new();

        loop {
            while next_task_index < queued_tasks.len()
                && running_count < controller.current_concurrency()
            {
                let task_index = next_task_index;
                let (task, row_key) = queued_tasks[task_index].clone();
                let target = target.clone();
                let options = options.clone();
                let progress = progress.clone();
                let completed = completed.clone();

                running.spawn(async move {
                    let running_row =
                        pending_result_row(&target, &options, Some(&task), row_key.clone());
                    emit_progress(
                        progress.as_ref(),
                        options.progress_run_id.as_deref(),
                        ApiBenchmarkProgressEventKind::Started,
                        row_key.clone(),
                        running_row,
                        completed.load(Ordering::SeqCst),
                        total,
                    );
                    let row = Self::run_once(&target, &options, Some(task), row_key.clone()).await;
                    let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
                    emit_progress(
                        progress.as_ref(),
                        options.progress_run_id.as_deref(),
                        ApiBenchmarkProgressEventKind::Completed,
                        row_key,
                        row.clone(),
                        done,
                        total,
                    );
                    (task_index, row)
                });

                running_count += 1;
                next_task_index += 1;
            }

            if next_task_index >= queued_tasks.len() && running_count == 0 {
                break;
            }

            tokio::select! {
                result = running.join_next(), if running_count > 0 => {
                    running_count = running_count.saturating_sub(1);
                    if let Some(Ok(row)) = result {
                        rows.push(row);
                    }
                }
                _ = controller.wait_for_concurrency_change(), if next_task_index < queued_tasks.len() => {}
            }
        }

        rows.sort_by_key(|(task_index, _)| *task_index);
        rows.into_iter().map(|(_, row)| row).collect()
    }

    pub async fn rerun_python(code: String, task_id: Option<String>) -> CodeEvaluationResult {
        evaluate_generated_python(&code, task_id.as_deref()).await
    }

    fn targets_for_providers(
        app_type: &AppType,
        providers: &IndexMap<String, Provider>,
    ) -> Vec<ApiBenchmarkTarget> {
        let mut targets = Vec::new();

        for provider in providers.values() {
            match app_type {
                AppType::Codex => {
                    if let Some(target) = codex_target(provider, app_type.as_str()) {
                        targets.push(target);
                    }
                }
                AppType::OpenClaw | AppType::OpenCode => {
                    targets.extend(openai_compatible_targets(provider, app_type.as_str()));
                }
                _ => {}
            }
        }

        for (index, target) in targets.iter_mut().enumerate() {
            target.entry.index = index;
        }

        targets
    }

    async fn run_once(
        target: &ApiBenchmarkTarget,
        options: &ApiBenchmarkOptions,
        code_task: Option<CodeTaskDefinition>,
        row_key: String,
    ) -> ApiBenchmarkResultRow {
        let url = chat_completions_url(&target.entry.base_url);
        let timeout = Duration::from_secs(options.timeout_secs.max(1));
        let client = crate::proxy::http_client::get();
        let task_id = code_task.as_ref().map(|task| task.task_id.clone());
        let task_title = code_task.as_ref().map(|task| task.title.clone());
        let task_difficulty = code_task.as_ref().map(|task| task.difficulty.clone());
        let prompt_text = benchmark_prompt_for_task(options.prompt, code_task.as_ref());
        let row_key_for_log = row_key.clone();

        log::info!(
            "[ApiBenchmark] start run_id={} row_key={} label=\"{}\" prompt={} model=\"{}\" task_id={} task_title={} task_difficulty={} timeout_secs={} max_concurrency={} base_url={}",
            options.progress_run_id.as_deref().unwrap_or("-"),
            row_key_for_log,
            target.entry.label,
            benchmark_prompt_key(options.prompt),
            target.entry.model,
            task_id.as_deref().unwrap_or("-"),
            task_title.as_deref().unwrap_or("-"),
            task_difficulty.as_deref().unwrap_or("-"),
            timeout.as_secs(),
            code_benchmark_concurrency(options),
            target.entry.base_url,
        );

        let mut body = json!({
            "model": target.entry.model,
            "messages": [
                {
                    "role": "system",
                    "content": "You are participating in a latency benchmark. Follow the user request exactly."
                },
                {
                    "role": "user",
                    "content": prompt_text
                }
            ],
            "temperature": 0,
            "top_p": 1,
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_tokens": 1200
        });

        if let Some(extra) = options.extra_body.as_ref().and_then(|v| v.as_object()) {
            if let Some(body_obj) = body.as_object_mut() {
                for (key, value) in extra {
                    body_obj.insert(key.clone(), value.clone());
                }
            }
        }

        let start = Instant::now();
        let mut sse = SseAccumulator {
            output: String::new(),
            usage: None,
            returned_model: String::new(),
            service_tier: String::new(),
            system_fingerprint: String::new(),
            first_token_time: None,
            start,
        };
        let mut status_code = None;
        let mut request_id = String::new();
        let mut error = String::new();

        let response_result = client
            .post(url)
            .timeout(timeout)
            .bearer_auth(&target.api_key)
            .json(&body)
            .send()
            .await;

        match response_result {
            Ok(response) => {
                status_code = Some(response.status().as_u16());
                request_id = response
                    .headers()
                    .get("x-request-id")
                    .or_else(|| response.headers().get("openai-request-id"))
                    .or_else(|| response.headers().get("cf-ray"))
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                if response.status().is_client_error() || response.status().is_server_error() {
                    let status = response.status().as_u16();
                    let text = response.text().await.unwrap_or_default();
                    error = format!(
                        "HTTP {status}: {}",
                        text.chars().take(1000).collect::<String>()
                    );
                } else {
                    let mut stream = response.bytes_stream();
                    let mut pending = String::new();

                    while let Some(chunk_result) = stream.next().await {
                        match chunk_result {
                            Ok(chunk) => {
                                pending.push_str(&String::from_utf8_lossy(&chunk));
                                while let Some(line_end) = pending.find('\n') {
                                    let line =
                                        pending[..line_end].trim_end_matches('\r').to_string();
                                    pending = pending[line_end + 1..].to_string();
                                    if let Some(done) = handle_sse_line(&line, &mut sse) {
                                        if done {
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                error = err.to_string();
                                break;
                            }
                        }
                    }
                }
            }
            Err(err) => {
                error = err.to_string();
            }
        }

        let end = Instant::now();
        let code_evaluation = if options.prompt == BenchmarkPromptKind::Code && error.is_empty() {
            Some(evaluate_generated_python(&sse.output, task_id.as_deref()).await)
        } else {
            None
        };
        let row = build_result_row(ResultRowParts {
            target,
            prompt_kind: options.prompt,
            task_id,
            task_title,
            prompt: prompt_text,
            start,
            first_token_time: sse.first_token_time,
            end,
            output: sse.output,
            usage: sse.usage.as_ref(),
            returned_model: sse.returned_model,
            service_tier: sse.service_tier,
            system_fingerprint: sse.system_fingerprint,
            status_code,
            request_id,
            error,
            code_evaluation,
            row_key,
        });

        let evaluation_failure_reason = row
            .code_evaluation
            .as_ref()
            .and_then(|evaluation| evaluation.failure_reason.as_deref());
        let has_error = !row.error.is_empty() || evaluation_failure_reason.is_some();

        log::info!(
            "[ApiBenchmark] end run_id={} row_key={} label=\"{}\" status={} status_code={:?} request_id={} total_time_sec={:.3} ttft_sec={} code_pass={}",
            options.progress_run_id.as_deref().unwrap_or("-"),
            row.row_key.as_deref().unwrap_or(&row_key_for_log),
            row.label,
            if has_error { "failed" } else { "ok" },
            row.status_code,
            row.request_id,
            row.total_time_sec,
            row.ttft_sec
                .map(|value| format!("{value:.3}"))
                .unwrap_or_else(|| "--".to_string()),
            row.code_evaluation
                .as_ref()
                .map(|evaluation| format!("{}/{}", evaluation.passed_tests, evaluation.total_tests))
                .unwrap_or_else(|| "--".to_string()),
        );

        if let Some(reason) = evaluation_failure_reason {
            log::error!(
                "[ApiBenchmark] error run_id={} row_key={} label=\"{}\" kind=code_evaluation reason={}",
                options.progress_run_id.as_deref().unwrap_or("-"),
                row.row_key.as_deref().unwrap_or(&row_key_for_log),
                row.label,
                reason,
            );
        } else if !row.error.is_empty() {
            log::error!(
                "[ApiBenchmark] error run_id={} row_key={} label=\"{}\" kind=request reason={}",
                options.progress_run_id.as_deref().unwrap_or("-"),
                row.row_key.as_deref().unwrap_or(&row_key_for_log),
                row.label,
                row.error,
            );
        }

        row
    }
}

#[cfg(test)]
fn benchmark_prompt(kind: BenchmarkPromptKind) -> String {
    let task = if kind == BenchmarkPromptKind::Code {
        load_code_task_definition().ok()
    } else {
        None
    };
    benchmark_prompt_for_task(kind, task.as_ref())
}

fn benchmark_prompt_for_task(
    kind: BenchmarkPromptKind,
    task: Option<&CodeTaskDefinition>,
) -> String {
    match kind {
        BenchmarkPromptKind::Short => "只回答 OK。".to_string(),
        BenchmarkPromptKind::Medium => {
            "请生成一段 800 字左右的中文技术说明，主题是：\n“如何设计一个稳定的 API 性能基准测试系统”。\n\n要求：\n1. 不要使用列表\n2. 不要使用 Markdown 表格\n3. 输出长度尽量接近 800 字\n4. 直接开始正文".to_string()
        }
        BenchmarkPromptKind::Code => {
            task.map(|task| task.statement.clone()).unwrap_or_else(|| {
                "请用 Python 写一个完整脚本，实现：\n1. 读取 CSV\n2. 按 symbol 分组\n3. 计算每组 close 的 5 日、20 日移动平均\n4. 输出新的 CSV\n\n要求：\n- 代码完整可运行\n- 包含 argparse\n- 包含异常处理\n- 不要解释，只输出代码"
                    .to_string()
            })
        }
    }
}

pub fn chat_completions_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/chat/completions")
    } else if origin_only(trimmed) {
        format!("{trimmed}/v1/chat/completions")
    } else {
        let mut url = format!("{trimmed}/v1/chat/completions");
        while url.contains("/v1/v1/") {
            url = url.replace("/v1/v1/", "/v1/");
        }
        url
    }
}

fn origin_only(url: &str) -> bool {
    match url.split_once("://") {
        Some((_scheme, rest)) => !rest.contains('/'),
        None => !url.contains('/'),
    }
}

struct SseAccumulator {
    output: String,
    usage: Option<Value>,
    returned_model: String,
    service_tier: String,
    system_fingerprint: String,
    first_token_time: Option<Instant>,
    start: Instant,
}

fn handle_sse_line(line: &str, sse: &mut SseAccumulator) -> Option<bool> {
    if !line.starts_with("data: ") {
        return None;
    }

    let data = line[6..].trim();
    if data == "[DONE]" {
        return Some(true);
    }

    let Ok(obj) = serde_json::from_str::<Value>(data) else {
        return None;
    };

    if let Some(model) = obj.get("model").and_then(|v| v.as_str()) {
        sse.returned_model = model.to_string();
    }
    if let Some(tier) = obj.get("service_tier").and_then(|v| v.as_str()) {
        sse.service_tier = tier.to_string();
    }
    if let Some(fingerprint) = obj.get("system_fingerprint").and_then(|v| v.as_str()) {
        sse.system_fingerprint = fingerprint.to_string();
    }
    if let Some(value) = obj.get("usage") {
        sse.usage = Some(value.clone());
    }
    if let Some(content) = obj
        .pointer("/choices/0/delta/content")
        .and_then(|v| v.as_str())
    {
        if !content.is_empty() {
            if sse.first_token_time.is_none() {
                sse.first_token_time = Some(Instant::now().max(sse.start));
            }
            sse.output.push_str(content);
        }
    }

    Some(false)
}

fn total_rows_for_targets(
    targets: &[ApiBenchmarkTarget],
    options: &ApiBenchmarkOptions,
    runs: u32,
) -> usize {
    if options.prompt != BenchmarkPromptKind::Code {
        return targets.len() * runs.max(1) as usize;
    }
    let task_count = code_tasks_for_options(options)
        .map(|tasks| tasks.len())
        .unwrap_or_default();
    targets.len() * task_count
}

fn benchmark_row_key(entry_index: usize, prompt_kind: BenchmarkPromptKind, suffix: &str) -> String {
    format!(
        "{entry_index}:{}:{suffix}",
        benchmark_prompt_key(prompt_kind)
    )
}

fn benchmark_prompt_key(prompt_kind: BenchmarkPromptKind) -> &'static str {
    match prompt_kind {
        BenchmarkPromptKind::Short => "short",
        BenchmarkPromptKind::Medium => "medium",
        BenchmarkPromptKind::Code => "code",
    }
}

fn pending_result_row(
    target: &ApiBenchmarkTarget,
    options: &ApiBenchmarkOptions,
    code_task: Option<&CodeTaskDefinition>,
    row_key: String,
) -> ApiBenchmarkResultRow {
    let task_id = code_task.map(|task| task.task_id.clone());
    let task_title = code_task.map(|task| task.title.clone());
    let prompt = benchmark_prompt_for_task(options.prompt, code_task);

    ApiBenchmarkResultRow {
        row_key: Some(row_key),
        entry_index: target.entry.index,
        prompt_kind: options.prompt,
        task_id,
        task_title,
        prompt,
        response_text: String::new(),
        time: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        label: target.entry.label.clone(),
        model_requested: target.entry.model.clone(),
        model_returned: String::new(),
        service_tier: String::new(),
        system_fingerprint: String::new(),
        status_code: None,
        request_id: String::new(),
        ttft_sec: None,
        total_time_sec: 0.0,
        gen_time_sec: None,
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        tokens_per_sec: None,
        chars: 0,
        chars_per_sec: None,
        error: String::new(),
        code_evaluation: None,
    }
}

fn emit_progress(
    progress: Option<&ApiBenchmarkProgressEmitter>,
    run_id: Option<&str>,
    event: ApiBenchmarkProgressEventKind,
    row_key: String,
    row: ApiBenchmarkResultRow,
    completed: usize,
    total: usize,
) {
    let (Some(progress), Some(run_id)) = (progress, run_id) else {
        return;
    };
    progress(ApiBenchmarkProgressEvent {
        run_id: run_id.to_string(),
        event,
        row_key,
        row,
        completed,
        total,
    });
}

struct ResultRowParts<'a> {
    target: &'a ApiBenchmarkTarget,
    prompt_kind: BenchmarkPromptKind,
    task_id: Option<String>,
    task_title: Option<String>,
    prompt: String,
    start: Instant,
    first_token_time: Option<Instant>,
    end: Instant,
    output: String,
    usage: Option<&'a Value>,
    returned_model: String,
    service_tier: String,
    system_fingerprint: String,
    status_code: Option<u16>,
    request_id: String,
    error: String,
    code_evaluation: Option<CodeEvaluationResult>,
    row_key: String,
}

fn build_result_row(parts: ResultRowParts<'_>) -> ApiBenchmarkResultRow {
    let ResultRowParts {
        target,
        prompt_kind,
        task_id,
        task_title,
        prompt,
        start,
        first_token_time,
        end,
        output,
        usage,
        returned_model,
        service_tier,
        system_fingerprint,
        status_code,
        request_id,
        error,
        code_evaluation,
        row_key,
    } = parts;
    let total_time = end.duration_since(start).as_secs_f64();
    let ttft = first_token_time.map(|t| t.duration_since(start).as_secs_f64());
    let gen_time = first_token_time.map(|t| end.duration_since(t).as_secs_f64());

    let input_tokens = usage.and_then(input_tokens_from_usage);
    let output_tokens = usage.and_then(output_tokens_from_usage);
    let total_tokens = usage.and_then(total_tokens_from_usage);
    let response_text = if output.is_empty() && !error.is_empty() {
        error.clone()
    } else {
        output.clone()
    };
    let chars = response_text.chars().count();

    let tokens_per_sec = match (output_tokens, gen_time) {
        (Some(tokens), Some(seconds)) if seconds > 0.0 => Some(tokens as f64 / seconds),
        _ => None,
    };
    let chars_per_sec = match gen_time {
        Some(seconds) if chars > 0 && seconds > 0.0 => Some(chars as f64 / seconds),
        _ => None,
    };

    ApiBenchmarkResultRow {
        row_key: Some(row_key),
        entry_index: target.entry.index,
        prompt_kind,
        task_id,
        task_title,
        prompt,
        response_text,
        time: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        label: target.entry.label.clone(),
        model_requested: target.entry.model.clone(),
        model_returned: returned_model,
        service_tier,
        system_fingerprint,
        status_code,
        request_id,
        ttft_sec: ttft,
        total_time_sec: total_time,
        gen_time_sec: gen_time,
        input_tokens,
        output_tokens,
        total_tokens,
        tokens_per_sec,
        chars,
        chars_per_sec,
        error,
        code_evaluation,
    }
}

fn extract_python_code(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(code) = extract_fenced_code(trimmed, "python")
        .or_else(|| extract_fenced_code(trimmed, "py"))
        .or_else(|| extract_fenced_code(trimmed, ""))
    {
        return Some(code);
    }

    Some(trimmed.to_string())
}

fn extract_fenced_code(text: &str, language: &str) -> Option<String> {
    let fence = if language.is_empty() {
        "```".to_string()
    } else {
        format!("```{language}")
    };
    let start = text.find(&fence)?;
    let after_start = &text[start + fence.len()..];
    let after_start = after_start.strip_prefix('\r').unwrap_or(after_start);
    let after_start = after_start.strip_prefix('\n').unwrap_or(after_start);
    let end = after_start.find("```")?;
    let code = after_start[..end].trim();
    (!code.is_empty()).then(|| code.to_string())
}

async fn evaluate_generated_python(
    response_text: &str,
    task_id: Option<&str>,
) -> CodeEvaluationResult {
    let task = match select_code_task_by_id_or_first(task_id) {
        Ok(task) => task,
        Err(err) => {
            return code_eval_failure(CodeEvaluationFailure {
                code_extracted: false,
                syntax_ok: false,
                runnable: false,
                score: 0.0,
                total_tests: 0,
                task_id: None,
                task_title: None,
                extracted_code: None,
                reason: format!("Failed to load code task: {err}"),
            });
        }
    };
    let task_id = task.task_id.clone();
    let task_title = task.title.clone();
    let fixtures = match load_code_fixtures(&task) {
        Ok(fixtures) => fixtures,
        Err(err) => {
            return code_eval_failure(CodeEvaluationFailure {
                code_extracted: false,
                syntax_ok: false,
                runnable: false,
                score: 0.0,
                total_tests: 0,
                task_id: Some(task_id),
                task_title: Some(task_title),
                extracted_code: None,
                reason: format!("Failed to load code fixtures: {err}"),
            });
        }
    };
    let total_tests = fixtures.len();

    let Some(code) = extract_python_code(response_text) else {
        return CodeEvaluationResult {
            task_id: Some(task_id),
            task_title: Some(task_title),
            code_extracted: false,
            syntax_ok: false,
            runnable: false,
            passed_tests: 0,
            total_tests,
            case_results: Vec::new(),
            score: 0.0,
            execution_time_sec: None,
            failure_reason: Some("No Python code could be extracted".to_string()),
            extracted_code: None,
        };
    };

    let started = Instant::now();
    let temp = match tempfile::Builder::new()
        .prefix("cc-switch-api-benchmark-")
        .tempdir()
    {
        Ok(temp) => temp,
        Err(err) => {
            return code_eval_failure(CodeEvaluationFailure {
                code_extracted: true,
                syntax_ok: false,
                runnable: false,
                score: 10.0,
                total_tests,
                task_id: Some(task_id),
                task_title: Some(task_title),
                extracted_code: Some(code),
                reason: format!("Failed to create temp dir: {err}"),
            });
        }
    };
    let script_path = temp.path().join("solution.py");
    if let Err(err) = tokio::fs::write(&script_path, &code).await {
        return code_eval_failure(CodeEvaluationFailure {
            code_extracted: true,
            syntax_ok: false,
            runnable: false,
            score: 10.0,
            total_tests,
            task_id: Some(task_id),
            task_title: Some(task_title),
            extracted_code: Some(code),
            reason: format!("Failed to write generated code: {err}"),
        });
    }

    let syntax = run_command_with_input(
        "python3",
        &["-m", "py_compile", script_path.to_string_lossy().as_ref()],
        "",
        10,
    )
    .await;

    if !syntax.success {
        return CodeEvaluationResult {
            task_id: Some(task_id),
            task_title: Some(task_title),
            code_extracted: true,
            syntax_ok: false,
            runnable: false,
            passed_tests: 0,
            total_tests,
            case_results: Vec::new(),
            score: 10.0,
            execution_time_sec: Some(started.elapsed().as_secs_f64()),
            failure_reason: Some(format!("Syntax check failed: {}", syntax.combined_output())),
            extracted_code: Some(code),
        };
    }

    let mut passed = 0usize;
    let mut first_failure = None;
    let mut case_results = Vec::new();
    for fixture in &fixtures {
        let run = run_command_with_input(
            "python3",
            &[script_path.to_string_lossy().as_ref()],
            &fixture.stdin,
            fixture.timeout_secs,
        )
        .await;

        let case_passed = run.success
            && normalize_stdout(&run.stdout) == normalize_stdout(&fixture.expected_stdout);
        if case_passed {
            passed += 1;
        } else if first_failure.is_none() {
            let reason = if !run.success {
                format!("Runtime failed: {}", run.combined_output())
            } else {
                format!(
                    "Wrong output. Expected `{}`, got `{}`",
                    normalize_stdout(&fixture.expected_stdout),
                    normalize_stdout(&run.stdout)
                )
            };
            first_failure = Some(reason);
        }

        case_results.push(CodeEvaluationCaseResult {
            case_id: fixture.case_id.clone(),
            passed: case_passed,
            stdin: fixture.stdin.clone(),
            stdout: run.stdout,
            stderr: run.stderr,
            expected_stdout: fixture.expected_stdout.clone(),
        });
    }

    let runnable = first_failure
        .as_ref()
        .map(|reason| !reason.starts_with("Runtime failed"))
        .unwrap_or(true);
    let pass_score = if fixtures.is_empty() {
        0.0
    } else {
        60.0 * passed as f64 / fixtures.len() as f64
    };

    CodeEvaluationResult {
        task_id: Some(task_id),
        task_title: Some(task_title),
        code_extracted: true,
        syntax_ok: true,
        runnable,
        passed_tests: passed,
        total_tests,
        case_results,
        score: 10.0 + 15.0 + if runnable { 15.0 } else { 0.0 } + pass_score,
        execution_time_sec: Some(started.elapsed().as_secs_f64()),
        failure_reason: first_failure,
        extracted_code: Some(code),
    }
}

fn code_eval_failure(failure: CodeEvaluationFailure) -> CodeEvaluationResult {
    CodeEvaluationResult {
        task_id: failure.task_id,
        task_title: failure.task_title,
        code_extracted: failure.code_extracted,
        syntax_ok: failure.syntax_ok,
        runnable: failure.runnable,
        passed_tests: 0,
        total_tests: failure.total_tests,
        case_results: Vec::new(),
        score: failure.score,
        execution_time_sec: None,
        failure_reason: Some(failure.reason),
        extracted_code: failure.extracted_code,
    }
}

struct CodeEvaluationFailure {
    code_extracted: bool,
    syntax_ok: bool,
    runnable: bool,
    score: f64,
    total_tests: usize,
    task_id: Option<String>,
    task_title: Option<String>,
    extracted_code: Option<String>,
    reason: String,
}

#[derive(Debug)]
struct CommandRunResult {
    success: bool,
    stdout: String,
    stderr: String,
}

impl CommandRunResult {
    fn combined_output(&self) -> String {
        let combined = format!("{}{}", self.stdout, self.stderr);
        combined.trim().chars().take(1000).collect()
    }
}

async fn run_command_with_input(
    program: &str,
    args: &[&str],
    stdin_text: &str,
    timeout_secs: u64,
) -> CommandRunResult {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return CommandRunResult {
                success: false,
                stdout: String::new(),
                stderr: format!("Failed to spawn {program}: {err}"),
            };
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let input = stdin_text.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
        });
    }

    match tokio::time::timeout(
        Duration::from_secs(timeout_secs.max(1)),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(output)) => CommandRunResult {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        },
        Ok(Err(err)) => CommandRunResult {
            success: false,
            stdout: String::new(),
            stderr: format!("Failed to wait for {program}: {err}"),
        },
        Err(_) => CommandRunResult {
            success: false,
            stdout: String::new(),
            stderr: format!("{program} timed out after {timeout_secs}s"),
        },
    }
}

fn normalize_stdout(text: &str) -> String {
    text.trim().replace("\r\n", "\n")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeQuestionBank {
    tasks: Vec<CodeTaskDefinition>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeTaskDefinition {
    task_id: String,
    title: String,
    difficulty: String,
    statement: String,
    timeout_sec: u64,
    tests: Vec<CodeTestInput>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeTestInput {
    case_id: String,
    stdin: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeAnswerBank {
    answers: Vec<CodeAnswer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeAnswer {
    task_id: String,
    case_id: String,
    expected_stdout: String,
}

#[derive(Debug, Clone)]
struct CodeFixture {
    case_id: String,
    stdin: String,
    expected_stdout: String,
    timeout_secs: u64,
}

fn load_code_question_bank() -> Result<CodeQuestionBank, String> {
    serde_json::from_str(CODE_QUESTIONS_JSON)
        .map_err(|err| format!("Invalid code question JSON: {err}"))
}

#[cfg(test)]
fn load_code_task_definition() -> Result<CodeTaskDefinition, String> {
    select_code_task(0)
}

#[cfg(test)]
fn select_code_task(index: usize) -> Result<CodeTaskDefinition, String> {
    let bank: CodeQuestionBank = serde_json::from_str(CODE_QUESTIONS_JSON)
        .map_err(|err| format!("Invalid code question JSON: {err}"))?;
    if bank.tasks.is_empty() {
        return Err("Code question JSON does not contain any tasks".to_string());
    }
    Ok(bank.tasks[index % bank.tasks.len()].clone())
}

fn select_code_task_by_id_or_first(task_id: Option<&str>) -> Result<CodeTaskDefinition, String> {
    let bank = load_code_question_bank()?;
    if bank.tasks.is_empty() {
        return Err("Code question JSON does not contain any tasks".to_string());
    }
    if let Some(task_id) = task_id {
        if let Some(task) = bank.tasks.iter().find(|task| task.task_id == task_id) {
            return Ok(task.clone());
        }
    }
    Ok(bank.tasks[0].clone())
}

fn code_tasks_for_options(
    options: &ApiBenchmarkOptions,
) -> Result<Vec<CodeTaskDefinition>, String> {
    if options.prompt != BenchmarkPromptKind::Code {
        return Ok(Vec::new());
    }
    let bank = load_code_question_bank()?;
    if bank.tasks.is_empty() {
        return Err("Code question JSON does not contain any tasks".to_string());
    }
    if let Some(task_id) = options.task_id.as_deref() {
        let task = bank
            .tasks
            .into_iter()
            .find(|task| task.task_id == task_id)
            .ok_or_else(|| format!("Code task `{task_id}` does not exist"))?;
        return Ok(vec![task]);
    }
    Ok(bank.tasks)
}

fn load_code_answer_bank() -> Result<CodeAnswerBank, String> {
    serde_json::from_str(CODE_ANSWERS_JSON)
        .map_err(|err| format!("Invalid code answer JSON: {err}"))
}

fn load_code_fixtures(task: &CodeTaskDefinition) -> Result<Vec<CodeFixture>, String> {
    let answers = load_code_answer_bank()?;
    let answer_by_case: HashMap<String, String> = answers
        .answers
        .into_iter()
        .filter(|answer| answer.task_id == task.task_id)
        .map(|answer| (answer.case_id, answer.expected_stdout))
        .collect();

    task.tests
        .clone()
        .into_iter()
        .map(|test| {
            let expected_stdout = answer_by_case.get(&test.case_id).cloned().ok_or_else(|| {
                format!(
                    "Code answer JSON is missing answer for task `{}` case `{}`",
                    task.task_id, test.case_id
                )
            })?;
            Ok(CodeFixture {
                case_id: test.case_id,
                stdin: test.stdin,
                expected_stdout,
                timeout_secs: task.timeout_sec,
            })
        })
        .collect()
}

fn input_tokens_from_usage(usage: &Value) -> Option<u64> {
    usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(|v| v.as_u64())
}

fn output_tokens_from_usage(usage: &Value) -> Option<u64> {
    usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(|v| v.as_u64())
}

fn total_tokens_from_usage(usage: &Value) -> Option<u64> {
    usage.get("total_tokens").and_then(|v| v.as_u64())
}

fn row_succeeded_for_summary(row: &ApiBenchmarkResultRow) -> bool {
    if !row.error.is_empty() {
        return false;
    }
    if row.prompt_kind != BenchmarkPromptKind::Code {
        return true;
    }

    let Some(evaluation) = row.code_evaluation.as_ref() else {
        return false;
    };
    evaluation.code_extracted
        && evaluation.syntax_ok
        && evaluation.runnable
        && evaluation.total_tests > 0
        && evaluation.passed_tests == evaluation.total_tests
}

fn summarize_rows(rows: &[ApiBenchmarkResultRow]) -> Vec<ApiBenchmarkSummary> {
    let mut labels = Vec::<String>::new();
    for row in rows {
        if !labels.contains(&row.label) {
            labels.push(row.label.clone());
        }
    }

    labels
        .into_iter()
        .map(|label| {
            let part: Vec<&ApiBenchmarkResultRow> = rows
                .iter()
                .filter(|row| row.label == label && row_succeeded_for_summary(row))
                .collect();
            ApiBenchmarkSummary {
                label,
                success: part.len(),
                ttft_sec_median: median(values(&part, |row| row.ttft_sec)),
                ttft_sec_p90: p90(values(&part, |row| row.ttft_sec)),
                total_time_sec_median: median(values(&part, |row| Some(row.total_time_sec))),
                total_time_sec_p90: p90(values(&part, |row| Some(row.total_time_sec))),
                tokens_per_sec_median: median(values(&part, |row| row.tokens_per_sec)),
                tokens_per_sec_p90: p90(values(&part, |row| row.tokens_per_sec)),
                chars_per_sec_median: median(values(&part, |row| row.chars_per_sec)),
                chars_per_sec_p90: p90(values(&part, |row| row.chars_per_sec)),
            }
        })
        .collect()
}

fn values<F>(rows: &[&ApiBenchmarkResultRow], get: F) -> Vec<f64>
where
    F: Fn(&ApiBenchmarkResultRow) -> Option<f64>,
{
    rows.iter().filter_map(|row| get(row)).collect()
}

fn median(mut vals: Vec<f64>) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(vals[vals.len() / 2])
}

fn p90(mut vals: Vec<f64>) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((vals.len() as f64 * 0.9).floor() as isize - 1)
        .max(0)
        .min(vals.len() as isize - 1) as usize;
    Some(vals[idx])
}

fn codex_target(provider: &Provider, app_type: &str) -> Option<ApiBenchmarkTarget> {
    let api_key = string_at_any(
        &provider.settings_config,
        &[
            "/auth/OPENAI_API_KEY",
            "/env/OPENAI_API_KEY",
            "/apiKey",
            "/api_key",
            "/config/api_key",
            "/config/apiKey",
        ],
    )?;
    let config_text = provider
        .settings_config
        .get("config")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let parsed = parse_codex_toml_config(config_text);
    let base_url = parsed.base_url.or_else(|| {
        string_at_any(
            &provider.settings_config,
            &["/base_url", "/baseURL", "/config/base_url"],
        )
    })?;
    let model = parsed
        .model
        .or_else(|| string_at_any(&provider.settings_config, &["/model"]))?;

    Some(target_from_parts(
        provider,
        app_type,
        normalize_base_url(&base_url),
        api_key,
        model,
    ))
}

fn openai_compatible_targets(provider: &Provider, app_type: &str) -> Vec<ApiBenchmarkTarget> {
    let Some(api_key) = string_at_any(
        &provider.settings_config,
        &[
            "/apiKey",
            "/api_key",
            "/auth/OPENAI_API_KEY",
            "/env/OPENAI_API_KEY",
        ],
    ) else {
        return Vec::new();
    };
    let Some(base_url) = string_at_any(
        &provider.settings_config,
        &["/baseUrl", "/base_url", "/baseURL"],
    ) else {
        return Vec::new();
    };

    let mut models = models_from_settings(&provider.settings_config);
    if models.is_empty() {
        if let Some(model) = string_at_any(&provider.settings_config, &["/model"]) {
            models.push(model);
        }
    }

    models
        .into_iter()
        .map(|model| {
            target_from_parts(
                provider,
                app_type,
                normalize_base_url(&base_url),
                api_key.clone(),
                model,
            )
        })
        .collect()
}

fn target_from_parts(
    provider: &Provider,
    app_type: &str,
    base_url: String,
    api_key: String,
    model: String,
) -> ApiBenchmarkTarget {
    let label = format!("{} / {}", provider.name, model);
    ApiBenchmarkTarget {
        entry: ApiBenchmarkEntry {
            index: 0,
            app_type: app_type.to_string(),
            provider_id: provider.id.clone(),
            provider_name: provider.name.clone(),
            model,
            base_url,
            api_key_present: !api_key.is_empty(),
            label,
        },
        api_key,
    }
}

#[derive(Default)]
struct ParsedCodexTomlConfig {
    model: Option<String>,
    base_url: Option<String>,
}

fn parse_codex_toml_config(config_text: &str) -> ParsedCodexTomlConfig {
    let Ok(value) = config_text.parse::<toml::Value>() else {
        return ParsedCodexTomlConfig::default();
    };

    let model = value
        .get("model")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let provider_id = value.get("model_provider").and_then(|v| v.as_str());
    let base_url = provider_id
        .and_then(|id| {
            value
                .get("model_providers")
                .and_then(|providers| providers.get(id))
                .and_then(|provider| provider.get("base_url"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| value.get("base_url").and_then(|v| v.as_str()))
        .map(ToString::to_string);

    ParsedCodexTomlConfig { model, base_url }
}

fn string_at_any(value: &Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|path| {
        value
            .pointer(path)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
    })
}

fn models_from_settings(settings: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(models) = settings.get("models").and_then(|v| v.as_array()) {
        for model in models {
            let id = model
                .get("id")
                .or_else(|| model.get("name"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(id) = id {
                let id = id.to_string();
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
    }
    out
}

fn normalize_base_url(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use axum::extract::State;
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::Router;
    use log::{LevelFilter, Log, Metadata, Record};
    use serde_json::json;
    use serial_test::serial;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex, Once};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::time::Duration;

    struct CapturingLogger {
        messages: Mutex<Vec<String>>,
    }

    static TEST_LOGGER: CapturingLogger = CapturingLogger {
        messages: Mutex::new(Vec::new()),
    };
    static INIT_TEST_LOGGER: Once = Once::new();

    impl Log for CapturingLogger {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.target().contains("api_benchmark")
        }

        fn log(&self, record: &Record<'_>) {
            if self.enabled(record.metadata()) {
                self.messages
                    .lock()
                    .expect("lock captured logs")
                    .push(format!("{} {}", record.level(), record.args()));
            }
        }

        fn flush(&self) {}
    }

    fn init_test_logger() {
        INIT_TEST_LOGGER.call_once(|| {
            log::set_logger(&TEST_LOGGER).expect("install test logger");
            log::set_max_level(LevelFilter::Trace);
        });
        TEST_LOGGER
            .messages
            .lock()
            .expect("clear captured logs")
            .clear();
    }

    fn captured_logs() -> Vec<String> {
        TEST_LOGGER
            .messages
            .lock()
            .expect("read captured logs")
            .clone()
    }

    fn provider(id: &str, name: &str, settings_config: Value) -> Provider {
        Provider {
            id: id.to_string(),
            name: name.to_string(),
            settings_config,
            website_url: None,
            category: None,
            created_at: None,
            sort_index: None,
            notes: None,
            meta: None,
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    #[test]
    fn prompt_lookup_matches_python_benchmark() {
        assert_eq!(benchmark_prompt(BenchmarkPromptKind::Short), "只回答 OK。");
        assert!(benchmark_prompt(BenchmarkPromptKind::Medium).contains("800 字左右"));
        assert!(benchmark_prompt(BenchmarkPromptKind::Code).contains("读取 CSV"));
    }

    #[test]
    fn code_question_and_answer_json_build_fixtures() {
        let task = load_code_task_definition().expect("code task should load from question JSON");
        assert_eq!(task.task_id, "csv_moving_average");
        assert!(task.statement.contains("读取 CSV"));
        assert_eq!(task.tests.len(), 2);
        assert!(task
            .tests
            .iter()
            .all(|test| !test.stdin.contains("ma5,ma20")));

        let fixtures =
            load_code_fixtures(&task).expect("fixtures should join questions and answers");
        assert_eq!(fixtures.len(), task.tests.len());
        assert!(fixtures[0].stdin.starts_with("symbol,date,close"));
        assert!(fixtures[0]
            .expected_stdout
            .contains("symbol,date,close,ma5,ma20"));
    }

    #[test]
    fn oa_question_bank_has_unique_tasks_and_complete_answers() {
        let bank = load_code_question_bank().expect("question bank should parse");
        let answers = load_code_answer_bank().expect("answer bank should parse");
        assert!(bank.tasks.len() >= 10);

        let mut task_ids = std::collections::HashSet::new();
        for task in &bank.tasks {
            assert!(task_ids.insert(task.task_id.clone()));
            assert!(task.difficulty == "medium" || task.difficulty == "medium-high");
            assert!(task.tests.len() >= 2);
            assert!(task.statement.contains("stdin"));

            for test in &task.tests {
                assert!(answers.answers.iter().any(|answer| {
                    answer.task_id == task.task_id && answer.case_id == test.case_id
                }));
            }
        }
    }

    #[test]
    fn code_prompt_without_task_selects_entire_question_bank() {
        let options = ApiBenchmarkOptions {
            prompt: BenchmarkPromptKind::Code,
            runs: 1,
            extra_body: None,
            timeout_secs: 180,
            task_id: None,
            max_concurrency: default_max_concurrency(),
            progress_run_id: None,
        };
        let tasks = code_tasks_for_options(&options).expect("code tasks should load");
        let bank = load_code_question_bank().expect("question bank should parse");
        assert_eq!(tasks.len(), bank.tasks.len());
        assert!(tasks.len() >= 10);
    }

    #[test]
    fn code_prompt_with_task_selects_only_that_task() {
        let options = ApiBenchmarkOptions {
            prompt: BenchmarkPromptKind::Code,
            runs: 1,
            extra_body: None,
            timeout_secs: 180,
            task_id: Some("payment_reconciliation".to_string()),
            max_concurrency: default_max_concurrency(),
            progress_run_id: None,
        };
        let tasks = code_tasks_for_options(&options).expect("code tasks should load");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "payment_reconciliation");
    }

    #[test]
    fn code_prompt_concurrency_is_conservative_and_clamped() {
        let mut options = ApiBenchmarkOptions {
            prompt: BenchmarkPromptKind::Code,
            runs: 1,
            extra_body: None,
            timeout_secs: 180,
            task_id: None,
            progress_run_id: None,
            max_concurrency: 0,
        };
        assert_eq!(code_benchmark_concurrency(&options), 1);

        options.max_concurrency = 2;
        assert_eq!(code_benchmark_concurrency(&options), 2);

        options.max_concurrency = 99;
        assert_eq!(code_benchmark_concurrency(&options), 5);
    }

    #[tokio::test]
    #[serial]
    async fn code_benchmark_does_not_repeat_completed_tasks_when_concurrency_changes() {
        #[derive(Clone)]
        struct ServerState {
            request_count: Arc<AtomicUsize>,
        }

        async fn handler(State(state): State<ServerState>) -> impl IntoResponse {
            state
                .request_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\ndata: [DONE]\n\n",
            )
        }

        let request_count = Arc::new(AtomicUsize::new(0));
        let state = ServerState {
            request_count: request_count.clone(),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("server addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .expect("serve test server");
        });

        let target = ApiBenchmarkTarget {
            entry: ApiBenchmarkEntry {
                index: 0,
                app_type: "codex".to_string(),
                provider_id: "provider-a".to_string(),
                provider_name: "Provider A".to_string(),
                model: "model-a".to_string(),
                base_url: format!("http://{addr}"),
                api_key_present: true,
                label: "Provider A / model-a".to_string(),
            },
            api_key: "sk-test".to_string(),
        };
        let options = ApiBenchmarkOptions {
            prompt: BenchmarkPromptKind::Code,
            runs: 1,
            extra_body: None,
            timeout_secs: 30,
            task_id: None,
            max_concurrency: 1,
            progress_run_id: Some("test-run".to_string()),
        };
        let task_count = code_tasks_for_options(&options)
            .expect("code tasks should load")
            .len();
        let (tx, mut rx) = mpsc::unbounded_channel::<ApiBenchmarkProgressEvent>();
        let progress: ApiBenchmarkProgressEmitter = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let run = tokio::spawn(async move {
            ApiBenchmarkService::run_targets_with_progress(vec![target], options, Some(progress))
                .await
        });

        let first_completion = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = rx.recv().await {
                if matches!(event.event, ApiBenchmarkProgressEventKind::Completed) {
                    return event;
                }
            }
            panic!("progress stream ended before the first completion event");
        })
        .await
        .expect("first completion event should arrive");

        assert_eq!(first_completion.completed, 1);
        assert!(ApiBenchmarkService::update_concurrency("test-run", 2));
        assert!(ApiBenchmarkService::update_concurrency("test-run", 3));

        let result = tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("benchmark should finish")
            .expect("benchmark task should not panic");
        assert_eq!(result.rows.len(), task_count);
        assert_eq!(
            request_count.load(std::sync::atomic::Ordering::SeqCst),
            task_count
        );

        server.abort();
    }

    #[tokio::test]
    #[serial]
    async fn run_once_logs_start_end_and_error_for_http_failures() {
        init_test_logger();

        async fn handler() -> impl IntoResponse {
            (StatusCode::SERVICE_UNAVAILABLE, "temporary overload")
        }

        let app = Router::new().route("/v1/chat/completions", post(handler));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("server addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .expect("serve test server");
        });

        let target = ApiBenchmarkTarget {
            entry: ApiBenchmarkEntry {
                index: 0,
                app_type: "codex".to_string(),
                provider_id: "provider-a".to_string(),
                provider_name: "Provider A".to_string(),
                model: "model-a".to_string(),
                base_url: format!("http://{addr}"),
                api_key_present: true,
                label: "Provider A / model-a".to_string(),
            },
            api_key: "sk-test".to_string(),
        };
        let options = ApiBenchmarkOptions {
            prompt: BenchmarkPromptKind::Medium,
            runs: 1,
            task_id: None,
            extra_body: None,
            timeout_secs: 30,
            max_concurrency: 2,
            progress_run_id: Some("log-test-run".to_string()),
        };

        let row =
            ApiBenchmarkService::run_once(&target, &options, None, "0:medium:run-0".to_string())
                .await;

        assert_eq!(row.status_code, Some(503));
        assert!(row.error.contains("HTTP 503"));

        let logs = captured_logs().join("\n");
        assert!(logs.contains("[ApiBenchmark] start run_id=log-test-run"));
        assert!(logs.contains("row_key=0:medium:run-0"));
        assert!(logs.contains("[ApiBenchmark] end run_id=log-test-run"));
        assert!(logs.contains("status=failed"));
        assert!(logs.contains("[ApiBenchmark] error run_id=log-test-run"));
        assert!(logs.contains("kind=request"));
        assert!(logs.contains("HTTP 503"));

        server.abort();
    }

    #[test]
    fn provider_id_resolution_filters_targets() {
        let mut providers = IndexMap::new();
        providers.insert(
            "local".to_string(),
            provider(
                "local",
                "Local",
                json!({
                    "baseUrl": "http://127.0.0.1:8000/v1",
                    "apiKey": "none",
                    "models": [
                        {"id": "model-a"},
                        {"id": "model-b"}
                    ]
                }),
            ),
        );

        let targets = ApiBenchmarkService::resolve_targets_for_provider(
            &AppType::OpenClaw,
            &providers,
            "local",
        )
        .expect("provider should resolve");
        assert_eq!(targets.len(), 2);
        assert!(targets
            .iter()
            .all(|target| target.entry.provider_id == "local"));
    }

    #[test]
    fn chat_completion_url_appends_openai_path() {
        assert_eq!(
            chat_completions_url("https://api.example.com"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://api.example.com/v1"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("http://127.0.0.1:8000/v1/"),
            "http://127.0.0.1:8000/v1/chat/completions"
        );
    }

    #[test]
    fn usage_parser_accepts_chat_and_responses_field_names() {
        let chat = json!({"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30});
        assert_eq!(input_tokens_from_usage(&chat), Some(10));
        assert_eq!(output_tokens_from_usage(&chat), Some(20));
        assert_eq!(total_tokens_from_usage(&chat), Some(30));

        let responses = json!({"input_tokens": 11, "output_tokens": 22});
        assert_eq!(input_tokens_from_usage(&responses), Some(11));
        assert_eq!(output_tokens_from_usage(&responses), Some(22));
    }

    #[test]
    fn summaries_use_successful_rows_only() {
        let rows = vec![
            ApiBenchmarkResultRow {
                row_key: Some("0:code:correct-output".to_string()),
                entry_index: 0,
                prompt_kind: BenchmarkPromptKind::Code,
                task_id: Some("task-a".to_string()),
                task_title: Some("Task A".to_string()),
                prompt: "prompt".to_string(),
                response_text: "response".to_string(),
                time: "t".to_string(),
                label: "A".to_string(),
                model_requested: "m".to_string(),
                model_returned: "m".to_string(),
                service_tier: String::new(),
                system_fingerprint: String::new(),
                status_code: Some(200),
                request_id: String::new(),
                ttft_sec: Some(1.0),
                total_time_sec: 2.0,
                gen_time_sec: Some(1.0),
                input_tokens: Some(1),
                output_tokens: Some(10),
                total_tokens: Some(11),
                tokens_per_sec: Some(10.0),
                chars: 20,
                chars_per_sec: Some(20.0),
                error: String::new(),
                code_evaluation: Some(CodeEvaluationResult {
                    task_id: Some("task-a".to_string()),
                    task_title: Some("Task A".to_string()),
                    code_extracted: true,
                    syntax_ok: true,
                    runnable: true,
                    passed_tests: 2,
                    total_tests: 2,
                    case_results: Vec::new(),
                    score: 100.0,
                    execution_time_sec: Some(0.1),
                    failure_reason: None,
                    extracted_code: Some("print('ok')".to_string()),
                }),
            },
            ApiBenchmarkResultRow {
                row_key: Some("0:code:wrong-output".to_string()),
                entry_index: 0,
                prompt_kind: BenchmarkPromptKind::Code,
                task_id: Some("task-a".to_string()),
                task_title: Some("Task A".to_string()),
                prompt: "prompt".to_string(),
                response_text: "print('bad')".to_string(),
                time: "t".to_string(),
                label: "A".to_string(),
                model_requested: "m".to_string(),
                model_returned: "m".to_string(),
                service_tier: String::new(),
                system_fingerprint: String::new(),
                status_code: Some(200),
                request_id: String::new(),
                ttft_sec: Some(0.5),
                total_time_sec: 1.0,
                gen_time_sec: Some(1.0),
                input_tokens: None,
                output_tokens: Some(100),
                total_tokens: None,
                tokens_per_sec: Some(100.0),
                chars: 0,
                chars_per_sec: None,
                error: String::new(),
                code_evaluation: Some(CodeEvaluationResult {
                    task_id: Some("task-a".to_string()),
                    task_title: Some("Task A".to_string()),
                    code_extracted: true,
                    syntax_ok: true,
                    runnable: true,
                    passed_tests: 1,
                    total_tests: 2,
                    case_results: Vec::new(),
                    score: 70.0,
                    execution_time_sec: Some(0.1),
                    failure_reason: Some("Wrong output".to_string()),
                    extracted_code: Some("print('bad')".to_string()),
                }),
            },
        ];

        let summaries = summarize_rows(&rows);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].success, 1);
        assert_eq!(summaries[0].tokens_per_sec_median, Some(10.0));
    }

    #[test]
    fn result_row_keeps_prompt_and_response_for_details() {
        let target = ApiBenchmarkTarget {
            entry: ApiBenchmarkEntry {
                index: 7,
                app_type: "codex".to_string(),
                provider_id: "provider-a".to_string(),
                provider_name: "Provider A".to_string(),
                model: "model-a".to_string(),
                base_url: "https://api.example.com/v1".to_string(),
                api_key_present: true,
                label: "Provider A / model-a".to_string(),
            },
            api_key: "sk-test".to_string(),
        };
        let start = Instant::now();
        let row = build_result_row(ResultRowParts {
            target: &target,
            prompt_kind: BenchmarkPromptKind::Code,
            task_id: Some("csv_moving_average".to_string()),
            task_title: Some("CSV moving average".to_string()),
            prompt: "question prompt".to_string(),
            start,
            first_token_time: None,
            end: start,
            output: "generated answer".to_string(),
            usage: None,
            returned_model: String::new(),
            service_tier: String::new(),
            system_fingerprint: String::new(),
            status_code: Some(200),
            request_id: "req-1".to_string(),
            error: String::new(),
            code_evaluation: None,
            row_key: "7:code:csv_moving_average".to_string(),
        });

        assert_eq!(row.entry_index, 7);
        assert_eq!(row.prompt_kind, BenchmarkPromptKind::Code);
        assert_eq!(row.task_id.as_deref(), Some("csv_moving_average"));
        assert_eq!(row.task_title.as_deref(), Some("CSV moving average"));
        assert_eq!(row.prompt, "question prompt");
        assert_eq!(row.response_text, "generated answer");
    }

    #[test]
    fn result_row_uses_api_error_as_response_text_for_details() {
        let target = ApiBenchmarkTarget {
            entry: ApiBenchmarkEntry {
                index: 7,
                app_type: "codex".to_string(),
                provider_id: "provider-a".to_string(),
                provider_name: "Provider A".to_string(),
                model: "model-a".to_string(),
                base_url: "https://api.example.com/v1".to_string(),
                api_key_present: true,
                label: "Provider A / model-a".to_string(),
            },
            api_key: "sk-test".to_string(),
        };
        let start = Instant::now();
        let row = build_result_row(ResultRowParts {
            target: &target,
            prompt_kind: BenchmarkPromptKind::Code,
            task_id: Some("csv_moving_average".to_string()),
            task_title: Some("CSV moving average".to_string()),
            prompt: "question prompt".to_string(),
            start,
            first_token_time: None,
            end: start,
            output: String::new(),
            usage: None,
            returned_model: String::new(),
            service_tier: String::new(),
            system_fingerprint: String::new(),
            status_code: Some(503),
            request_id: "req-1".to_string(),
            error: "HTTP 503: upstream unavailable".to_string(),
            code_evaluation: None,
            row_key: "7:code:csv_moving_average".to_string(),
        });

        assert_eq!(row.response_text, "HTTP 503: upstream unavailable");
    }

    #[test]
    fn codex_provider_toml_becomes_benchmark_entry() {
        let mut providers = IndexMap::new();
        providers.insert(
            "codex-a".to_string(),
            provider(
                "codex-a",
                "Codex A",
                json!({
                    "auth": {"OPENAI_API_KEY": "sk-test"},
                    "config": "model_provider = \"OpenAI\"\nmodel = \"gpt-5.5\"\n\n[model_providers.OpenAI]\nbase_url = \"https://api.example.com/v1\"\n"
                }),
            ),
        );

        let entries = ApiBenchmarkService::list_entries(&AppType::Codex, &providers);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].index, 0);
        assert_eq!(entries[0].provider_id, "codex-a");
        assert_eq!(entries[0].model, "gpt-5.5");
        assert_eq!(entries[0].base_url, "https://api.example.com/v1");
        assert!(entries[0].api_key_present);
    }

    #[test]
    fn provider_without_api_key_is_omitted() {
        let mut providers = IndexMap::new();
        providers.insert(
            "missing-key".to_string(),
            provider(
                "missing-key",
                "Missing Key",
                json!({
                    "config": "model_provider = \"OpenAI\"\nmodel = \"gpt-5.5\"\n\n[model_providers.OpenAI]\nbase_url = \"https://api.example.com/v1\"\n"
                }),
            ),
        );

        let entries = ApiBenchmarkService::list_entries(&AppType::Codex, &providers);
        assert!(entries.is_empty());
    }

    #[test]
    fn openclaw_models_expand_to_multiple_entries() {
        let mut providers = IndexMap::new();
        providers.insert(
            "local".to_string(),
            provider(
                "local",
                "Local",
                json!({
                    "baseUrl": "http://127.0.0.1:8000/v1",
                    "apiKey": "none",
                    "models": [
                        {"id": "model-a"},
                        {"id": "model-b"}
                    ]
                }),
            ),
        );

        let entries = ApiBenchmarkService::list_entries(&AppType::OpenClaw, &providers);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].model, "model-a");
        assert_eq!(entries[1].model, "model-b");
        assert_eq!(entries[1].index, 1);
    }

    #[test]
    fn extracts_python_code_from_fenced_or_raw_response() {
        let fenced = "说明文字\n```python\nprint('ok')\n```\n结束";
        assert_eq!(extract_python_code(fenced).as_deref(), Some("print('ok')"));

        let raw = "import sys\nprint(sys.stdin.read())";
        assert_eq!(extract_python_code(raw).as_deref(), Some(raw));
    }

    #[tokio::test]
    async fn evaluates_generated_python_code_with_fixture_tests() {
        let code = r#"
import argparse
import csv
import sys
from collections import defaultdict

def fmt(values, size):
    if len(values) < size:
        return ""
    return f"{sum(values[-size:]) / size:.2f}"

def main():
    parser = argparse.ArgumentParser()
    parser.parse_args()
    rows = list(csv.DictReader(sys.stdin))
    history = defaultdict(list)
    out = csv.DictWriter(sys.stdout, fieldnames=["symbol", "date", "close", "ma5", "ma20"])
    out.writeheader()
    for row in rows:
        close = float(row["close"])
        hist = history[row["symbol"]]
        hist.append(close)
        out.writerow({
            "symbol": row["symbol"],
            "date": row["date"],
            "close": row["close"],
            "ma5": fmt(hist, 5),
            "ma20": fmt(hist, 20),
        })

if __name__ == "__main__":
    main()
"#;

        let evaluation = evaluate_generated_python(code, Some("csv_moving_average")).await;
        assert!(evaluation.code_extracted);
        assert_eq!(evaluation.task_id.as_deref(), Some("csv_moving_average"));
        assert!(evaluation.syntax_ok);
        assert!(evaluation.runnable);
        assert_eq!(evaluation.passed_tests, evaluation.total_tests);
        assert_eq!(evaluation.case_results.len(), evaluation.total_tests);
        assert!(evaluation.case_results[0].passed);
        assert!(evaluation.case_results[0]
            .stdout
            .contains("symbol,date,close,ma5,ma20"));
        assert!(evaluation.case_results[0]
            .expected_stdout
            .contains("symbol,date,close,ma5,ma20"));
        assert_eq!(evaluation.score, 100.0);
    }

    #[tokio::test]
    async fn syntax_errors_are_reported_without_running_tests() {
        let evaluation =
            evaluate_generated_python("def broken(:\n    pass", Some("csv_moving_average")).await;
        assert!(evaluation.code_extracted);
        assert!(!evaluation.syntax_ok);
        assert!(!evaluation.runnable);
        assert_eq!(evaluation.passed_tests, 0);
        assert!(evaluation.score < 25.0);
        assert!(evaluation
            .failure_reason
            .as_deref()
            .unwrap_or("")
            .contains("Syntax"));
    }
}
