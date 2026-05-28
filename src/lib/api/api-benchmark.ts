import { invoke } from "@tauri-apps/api/core";
import type { AppId } from "./types";

export type BenchmarkPromptKind = "short" | "medium" | "code";

export interface ApiBenchmarkEntry {
  index: number;
  appType: string;
  providerId: string;
  providerName: string;
  model: string;
  baseUrl: string;
  apiKeyPresent: boolean;
  label: string;
}

export interface ApiBenchmarkOptions {
  prompt: BenchmarkPromptKind;
  runs: number;
  taskId?: string | null;
  extraBody?: unknown;
  timeoutSecs?: number;
  maxConcurrency?: number;
  progressRunId?: string | null;
}

export interface ApiBenchmarkResultRow {
  rowKey?: string;
  entryIndex: number;
  promptKind: BenchmarkPromptKind;
  taskId?: string | null;
  taskTitle?: string | null;
  prompt: string;
  responseText: string;
  time: string;
  label: string;
  modelRequested: string;
  modelReturned: string;
  serviceTier: string;
  systemFingerprint: string;
  statusCode?: number | null;
  requestId: string;
  ttftSec?: number | null;
  totalTimeSec: number;
  genTimeSec?: number | null;
  inputTokens?: number | null;
  outputTokens?: number | null;
  totalTokens?: number | null;
  tokensPerSec?: number | null;
  chars: number;
  charsPerSec?: number | null;
  error: string;
  codeEvaluation?: CodeEvaluationResult | null;
}

export interface CodeEvaluationResult {
  taskId?: string | null;
  taskTitle?: string | null;
  codeExtracted: boolean;
  syntaxOk: boolean;
  runnable: boolean;
  passedTests: number;
  totalTests: number;
  caseResults: CodeEvaluationCaseResult[];
  score: number;
  executionTimeSec?: number | null;
  failureReason?: string | null;
  extractedCode?: string | null;
}

export interface CodeEvaluationCaseResult {
  caseId: string;
  passed: boolean;
  stdin: string;
  stdout: string;
  stderr: string;
  expectedStdout: string;
}

export interface ApiBenchmarkSummary {
  label: string;
  success: number;
  ttftSecMedian?: number | null;
  ttftSecP90?: number | null;
  totalTimeSecMedian?: number | null;
  totalTimeSecP90?: number | null;
  tokensPerSecMedian?: number | null;
  tokensPerSecP90?: number | null;
  charsPerSecMedian?: number | null;
  charsPerSecP90?: number | null;
}

export interface ApiBenchmarkRunResult {
  rows: ApiBenchmarkResultRow[];
  summaries: ApiBenchmarkSummary[];
}

export type ApiBenchmarkRowProgressStatus =
  | "queued"
  | "running"
  | "completed"
  | "failed";

export interface ApiBenchmarkProgressEvent {
  runId: string;
  event: "rowQueued" | "rowStarted" | "rowCompleted";
  rowKey: string;
  row: ApiBenchmarkResultRow;
  completed: number;
  total: number;
}

export const apiBenchmarkApi = {
  async listEntries(appId: AppId): Promise<ApiBenchmarkEntry[]> {
    return await invoke("list_api_benchmark_entries", { app: appId });
  },

  async run(
    appId: AppId,
    indices: number[],
    options: ApiBenchmarkOptions,
  ): Promise<ApiBenchmarkRunResult> {
    return await invoke("run_api_benchmark", { app: appId, indices, options });
  },

  async runProvider(
    appId: AppId,
    providerId: string,
    options: ApiBenchmarkOptions,
  ): Promise<ApiBenchmarkRunResult> {
    return await invoke("run_api_benchmark_provider", {
      app: appId,
      providerId,
      options,
    });
  },

  async updateConcurrency(
    runId: string,
    maxConcurrency: number,
  ): Promise<boolean> {
    return await invoke("update_api_benchmark_concurrency", {
      runId,
      maxConcurrency,
    });
  },

  async rerunPython(
    code: string,
    taskId?: string | null,
  ): Promise<CodeEvaluationResult> {
    return await invoke("rerun_api_benchmark_python", { code, taskId });
  },
};
