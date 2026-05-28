import { Fragment, type KeyboardEvent, useEffect, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import {
  Activity,
  ChevronDown,
  ChevronRight,
  CheckCircle2,
  Code2,
  Clock3,
  Loader2,
  Play,
  RotateCcw,
  Terminal,
  XCircle,
} from "lucide-react";
import { toast } from "sonner";

import {
  apiBenchmarkApi,
  type ApiBenchmarkProgressEvent,
  type ApiBenchmarkResultRow,
  type ApiBenchmarkRunResult,
  type ApiBenchmarkRowProgressStatus,
  type ApiBenchmarkSummary,
  type CodeEvaluationResult,
} from "@/lib/api/api-benchmark";
import type { AppId } from "@/lib/api";
import { generateUUID } from "@/utils/uuid";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Badge } from "@/components/ui/badge";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Textarea } from "@/components/ui/textarea";

interface ApiBenchmarkPanelProps {
  appId: AppId;
  providerId: string;
  providerName: string;
}

type RowStatusByKey = Record<string, ApiBenchmarkRowProgressStatus>;

const DEFAULT_MAX_CONCURRENCY = 2;
const MAX_MAX_CONCURRENCY = 5;

function formatMetric(value?: number | null, digits = 3) {
  if (typeof value !== "number" || Number.isNaN(value)) {
    return "--";
  }
  return value.toFixed(digits);
}

function parseMaxConcurrency(value: string) {
  const parsed = parseInt(value, 10);
  if (!Number.isFinite(parsed)) {
    return DEFAULT_MAX_CONCURRENCY;
  }
  return Math.min(MAX_MAX_CONCURRENCY, Math.max(1, parsed));
}

function formatCodeEvaluation(
  evaluation: ApiBenchmarkRunResult["rows"][number]["codeEvaluation"],
) {
  if (!evaluation) {
    return "--";
  }
  if (!evaluation.codeExtracted || !evaluation.syntaxOk) {
    return "error";
  }
  return `${evaluation.passedTests}/${evaluation.totalTests}`;
}

function isCodeError(evaluation?: CodeEvaluationResult | null) {
  return !!evaluation && (!evaluation.codeExtracted || !evaluation.syntaxOk);
}

function preClassName(extra = "") {
  return `min-h-[120px] max-h-[320px] overflow-auto whitespace-pre rounded-md border border-border-default bg-muted/30 p-3 font-mono text-xs leading-relaxed ${extra}`;
}

function stableRowKey(row: ApiBenchmarkResultRow, index = 0) {
  return (
    row.rowKey ||
    `${row.entryIndex}:${row.promptKind}:${row.taskId || row.time || index}`
  );
}

function rowHasFailed(row: ApiBenchmarkResultRow) {
  if (row.error) {
    return true;
  }
  const evaluation = row.codeEvaluation;
  if (!evaluation) {
    return false;
  }
  if (isCodeError(evaluation) || !evaluation.runnable) {
    return true;
  }
  return evaluation.passedTests < evaluation.totalTests;
}

function medianMetric(values: number[]) {
  if (!values.length) {
    return null;
  }
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
}

function valuesForSummary(
  rows: ApiBenchmarkResultRow[],
  getter: (row: ApiBenchmarkResultRow) => number | null | undefined,
) {
  return rows
    .map(getter)
    .filter((value): value is number => typeof value === "number");
}

function summarizeRows(rows: ApiBenchmarkResultRow[]): ApiBenchmarkSummary[] {
  const labels: string[] = [];
  rows.forEach((row) => {
    if (!labels.includes(row.label)) {
      labels.push(row.label);
    }
  });

  return labels.map((label) => {
    const successfulRows = rows.filter(
      (row) => row.label === label && !rowHasFailed(row),
    );
    return {
      label,
      success: successfulRows.length,
      ttftSecMedian: medianMetric(
        valuesForSummary(successfulRows, (row) => row.ttftSec),
      ),
      totalTimeSecMedian: medianMetric(
        valuesForSummary(successfulRows, (row) => row.totalTimeSec),
      ),
      tokensPerSecMedian: medianMetric(
        valuesForSummary(successfulRows, (row) => row.tokensPerSec),
      ),
      charsPerSecMedian: medianMetric(
        valuesForSummary(successfulRows, (row) => row.charsPerSec),
      ),
    };
  });
}

function mergeRows(
  currentRows: ApiBenchmarkResultRow[],
  updatedRows: ApiBenchmarkResultRow[],
) {
  const rows = [...currentRows];
  updatedRows.forEach((row, index) => {
    const key = stableRowKey(row, index);
    const existingIndex = rows.findIndex(
      (current, currentIndex) => stableRowKey(current, currentIndex) === key,
    );
    if (existingIndex >= 0) {
      rows[existingIndex] = row;
    } else {
      rows.push(row);
    }
  });
  return rows;
}

function retryGroupsForRows(rows: ApiBenchmarkResultRow[]) {
  const groups = new Map<string, Set<number>>();
  rows.filter(rowHasFailed).forEach((row) => {
    if (!row.taskId) {
      return;
    }
    const indices = groups.get(row.taskId) ?? new Set<number>();
    indices.add(row.entryIndex);
    groups.set(row.taskId, indices);
  });
  return Array.from(groups.entries()).map(([taskId, indices]) => ({
    taskId,
    indices: Array.from(indices),
  }));
}

export function ApiBenchmarkPanel({
  appId,
  providerId,
  providerName,
}: ApiBenchmarkPanelProps) {
  const { t } = useTranslation();
  const [timeoutSecs, setTimeoutSecs] = useState("180");
  const [concurrencyInput, setConcurrencyInput] = useState(
    String(DEFAULT_MAX_CONCURRENCY),
  );
  const [maxConcurrency, setMaxConcurrency] = useState(DEFAULT_MAX_CONCURRENCY);
  const [isRunning, setIsRunning] = useState(false);
  const [activeRunId, setActiveRunId] = useState<string | null>(null);
  const [result, setResult] = useState<ApiBenchmarkRunResult | null>(null);
  const [expandedRows, setExpandedRows] = useState<Set<string>>(new Set());
  const [rerunningRows, setRerunningRows] = useState<Set<string>>(new Set());
  const [regeneratingRows, setRegeneratingRows] = useState<Set<string>>(
    new Set(),
  );
  const [rowStatuses, setRowStatuses] = useState<RowStatusByKey>({});

  useEffect(() => {
    setResult(null);
    setExpandedRows(new Set());
    setRowStatuses({});
    void runBenchmark();
  }, [appId, providerId]);

  function parsedTimeoutSecs() {
    return Math.max(1, parseInt(timeoutSecs, 10) || 180);
  }

  function markRowsComplete(rows: ApiBenchmarkResultRow[]) {
    setRowStatuses((prev) => {
      const next = { ...prev };
      rows.forEach((row, index) => {
        const key = stableRowKey(row, index);
        next[key] = rowHasFailed(row) ? "failed" : "completed";
      });
      return next;
    });
  }

  async function runBenchmark(concurrencyOverride = maxConcurrency) {
    const parsedTimeout = parsedTimeoutSecs();
    const progressRunId = generateUUID();
    let unlistenProgress: UnlistenFn | undefined;

    try {
      setIsRunning(true);
      setActiveRunId(progressRunId);
      setResult({ rows: [], summaries: [] });
      setExpandedRows(new Set());
      setRowStatuses({});
      unlistenProgress = await listen("api-benchmark-progress", (event) => {
        const payload = event.payload as ApiBenchmarkProgressEvent;
        if (!payload || payload.runId !== progressRunId) {
          return;
        }
        applyProgressEvent(payload);
      });
      const data = await apiBenchmarkApi.runProvider(appId, providerId, {
        prompt: "code",
        runs: 1,
        timeoutSecs: parsedTimeout,
        maxConcurrency: concurrencyOverride,
        progressRunId,
      });
      setResult(data);
      markRowsComplete(data.rows);
      toast.success(t("apiBenchmark.runComplete"));
    } catch (e) {
      toast.error(`${t("apiBenchmark.runFailed")}: ${String(e)}`);
    } finally {
      unlistenProgress?.();
      setIsRunning(false);
      setActiveRunId((current) => (current === progressRunId ? null : current));
    }
  }

  function rowKey(row: ApiBenchmarkResultRow, index: number) {
    return stableRowKey(row, index);
  }

  function applyProgressEvent(event: ApiBenchmarkProgressEvent) {
    setResult((prev) => {
      const current = prev ?? { rows: [], summaries: [] };
      const rows = [...current.rows];
      const existingIndex = rows.findIndex(
        (row, index) => rowKey(row, index) === event.rowKey,
      );

      if (existingIndex >= 0) {
        rows[existingIndex] = event.row;
      } else {
        rows.push(event.row);
      }

      return {
        ...current,
        rows,
      };
    });

    setRowStatuses((prev) => ({
      ...prev,
      [event.rowKey]:
        event.event === "rowQueued"
          ? "queued"
          : event.event === "rowStarted"
            ? "running"
            : rowHasFailed(event.row)
              ? "failed"
              : "completed",
    }));
  }

  function toggleRow(key: string) {
    setExpandedRows((prev) => {
      const next = new Set(prev);
      if (next.has(key)) {
        next.delete(key);
      } else {
        next.add(key);
      }
      return next;
    });
  }

  function applyConcurrencyInput() {
    const nextConcurrency = parseMaxConcurrency(concurrencyInput);
    setConcurrencyInput(String(nextConcurrency));
    setMaxConcurrency(nextConcurrency);
    return nextConcurrency;
  }

  function handleConcurrencyKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key !== "Enter") {
      return;
    }
    event.preventDefault();
    const nextConcurrency = applyConcurrencyInput();
    if (isRunning && activeRunId) {
      void apiBenchmarkApi
        .updateConcurrency(activeRunId, nextConcurrency)
        .catch((e) => {
          toast.error(
            `${t("apiBenchmark.concurrencyUpdateFailed")}: ${String(e)}`,
          );
        });
    }
  }

  function runBenchmarkFromControls() {
    const nextConcurrency = applyConcurrencyInput();
    void runBenchmark(nextConcurrency);
  }

  async function retryFailedRowsFromControls() {
    if (!result) {
      return;
    }
    const retryGroups = retryGroupsForRows(result.rows);
    if (!retryGroups.length) {
      return;
    }

    const nextConcurrency = applyConcurrencyInput();
    const parsedTimeout = parsedTimeoutSecs();
    let unlistenProgress: UnlistenFn | undefined;

    try {
      setIsRunning(true);
      for (const group of retryGroups) {
        const progressRunId = generateUUID();
        setActiveRunId(progressRunId);
        unlistenProgress = await listen("api-benchmark-progress", (event) => {
          const payload = event.payload as ApiBenchmarkProgressEvent;
          if (!payload || payload.runId !== progressRunId) {
            return;
          }
          applyProgressEvent(payload);
        });

        const data = await apiBenchmarkApi.run(appId, group.indices, {
          prompt: "code",
          runs: 1,
          timeoutSecs: parsedTimeout,
          maxConcurrency: nextConcurrency,
          taskId: group.taskId,
          progressRunId,
        });
        setResult((prev) => {
          if (!prev) {
            return data;
          }
          const rows = mergeRows(prev.rows, data.rows);
          return {
            rows,
            summaries: summarizeRows(rows),
          };
        });
        markRowsComplete(data.rows);
        unlistenProgress?.();
        unlistenProgress = undefined;
      }
      toast.success(t("apiBenchmark.retryFailedComplete"));
    } catch (e) {
      toast.error(`${t("apiBenchmark.retryFailedFailed")}: ${String(e)}`);
    } finally {
      unlistenProgress?.();
      setIsRunning(false);
      setActiveRunId(null);
    }
  }

  function patchResultRow(
    key: string,
    updater: (row: ApiBenchmarkResultRow) => ApiBenchmarkResultRow,
  ) {
    setResult((prev) => {
      if (!prev) {
        return prev;
      }
      return {
        ...prev,
        rows: prev.rows.map((row, index) =>
          rowKey(row, index) === key ? updater(row) : row,
        ),
      };
    });
  }

  function statusForRow(row: ApiBenchmarkResultRow, key: string) {
    return rowStatuses[key] ?? (rowHasFailed(row) ? "failed" : "completed");
  }

  function statusButtonLabel(status: ApiBenchmarkRowProgressStatus) {
    if (status === "queued") {
      return t("apiBenchmark.rowStatusQueued");
    }
    if (status === "running") {
      return t("apiBenchmark.rowStatusRunning");
    }
    if (status === "failed") {
      return t("apiBenchmark.rowStatusFailed");
    }
    return t("apiBenchmark.rowStatusCompleted");
  }

  function statusButtonClassName(status: ApiBenchmarkRowProgressStatus) {
    if (status === "queued") {
      return "h-7 border-slate-200 bg-slate-50 px-2 text-xs text-slate-600 hover:bg-slate-100 dark:border-slate-800 dark:bg-slate-950/30 dark:text-slate-300";
    }
    if (status === "failed") {
      return "h-7 border-red-200 bg-red-50 px-2 text-xs text-red-600 hover:bg-red-100 dark:border-red-900/60 dark:bg-red-950/30 dark:text-red-300";
    }
    if (status === "running") {
      return "h-7 border-blue-200 bg-blue-50 px-2 text-xs text-blue-600 hover:bg-blue-100 dark:border-blue-900/60 dark:bg-blue-950/30 dark:text-blue-300";
    }
    return "h-7 border-emerald-200 bg-emerald-50 px-2 text-xs text-emerald-700 hover:bg-emerald-100 dark:border-emerald-900/60 dark:bg-emerald-950/30 dark:text-emerald-300";
  }

  function displayCodeText(row: ApiBenchmarkResultRow) {
    return (
      row.codeEvaluation?.extractedCode || row.responseText || row.error || ""
    );
  }

  async function regenerateRow(row: ApiBenchmarkResultRow, key: string) {
    try {
      setRegeneratingRows((prev) => new Set(prev).add(key));
      const data = await apiBenchmarkApi.run(appId, [row.entryIndex], {
        prompt: "code",
        runs: 1,
        taskId: row.taskId,
        timeoutSecs: Math.max(1, parseInt(timeoutSecs, 10) || 180),
      });
      const nextRow = data.rows[0];
      if (nextRow) {
        patchResultRow(key, () => nextRow);
        setRowStatuses((prev) => ({
          ...prev,
          [key]: rowHasFailed(nextRow) ? "failed" : "completed",
        }));
        setExpandedRows((prev) => new Set(prev).add(key));
      }
      toast.success(t("apiBenchmark.regenerateComplete"));
    } catch (e) {
      toast.error(`${t("apiBenchmark.regenerateFailed")}: ${String(e)}`);
    } finally {
      setRegeneratingRows((prev) => {
        const next = new Set(prev);
        next.delete(key);
        return next;
      });
    }
  }

  async function rerunPython(row: ApiBenchmarkResultRow, key: string) {
    const code =
      row.codeEvaluation?.extractedCode || (row.error ? "" : row.responseText);
    if (!code.trim()) {
      toast.error(t("apiBenchmark.noPythonCode"));
      return;
    }

    try {
      setRerunningRows((prev) => new Set(prev).add(key));
      const evaluation = await apiBenchmarkApi.rerunPython(code, row.taskId);
      patchResultRow(key, (current) => ({
        ...current,
        codeEvaluation: evaluation,
      }));
      toast.success(t("apiBenchmark.pythonRunComplete"));
    } catch (e) {
      toast.error(`${t("apiBenchmark.pythonRunFailed")}: ${String(e)}`);
    } finally {
      setRerunningRows((prev) => {
        const next = new Set(prev);
        next.delete(key);
        return next;
      });
    }
  }

  const retryGroups = result ? retryGroupsForRows(result.rows) : [];
  const retryFailedCount = retryGroups.reduce(
    (count, group) => count + group.indices.length,
    0,
  );
  const hasRetryableFailures = retryGroups.length > 0;

  return (
    <div className="space-y-5">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
        <div className="space-y-1">
          <div className="text-sm font-medium">{providerName}</div>
          <div className="text-xs text-muted-foreground">
            {t("apiBenchmark.runAllQuestions")}
          </div>
        </div>
        <div className="grid w-full grid-cols-2 gap-2 sm:w-72">
          <div className="space-y-2">
            <Label htmlFor="apiBenchmarkTimeout">
              {t("streamCheck.timeout")}
            </Label>
            <Input
              id="apiBenchmarkTimeout"
              type="number"
              min={1}
              value={timeoutSecs}
              onChange={(event) => setTimeoutSecs(event.target.value)}
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="apiBenchmarkConcurrency">
              {t("apiBenchmark.concurrency")}
            </Label>
            <Input
              id="apiBenchmarkConcurrency"
              type="number"
              min={1}
              max={MAX_MAX_CONCURRENCY}
              value={concurrencyInput}
              onChange={(event) => setConcurrencyInput(event.target.value)}
              onKeyDown={handleConcurrencyKeyDown}
            />
          </div>
        </div>
      </div>

      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          onClick={
            hasRetryableFailures
              ? () => void retryFailedRowsFromControls()
              : runBenchmarkFromControls
          }
          disabled={isRunning}
        >
          {isRunning ? (
            <Loader2 className="mr-2 h-4 w-4 animate-spin" />
          ) : hasRetryableFailures ? (
            <RotateCcw className="mr-2 h-4 w-4" />
          ) : (
            <Play className="mr-2 h-4 w-4" />
          )}
          {hasRetryableFailures
            ? t("apiBenchmark.retryFailed", { count: retryFailedCount })
            : t("apiBenchmark.runAll")}
        </Button>
        {hasRetryableFailures && (
          <Button
            type="button"
            variant="outline"
            onClick={runBenchmarkFromControls}
            disabled={isRunning}
          >
            <Play className="mr-2 h-4 w-4" />
            {t("apiBenchmark.rerunAll")}
          </Button>
        )}
      </div>

      {result && (
        <div className="space-y-4">
          <div className="flex items-center gap-2 text-sm font-medium">
            <Activity className="h-4 w-4 text-primary" />
            {t("apiBenchmark.results")}
          </div>

          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>{t("usage.provider")}</TableHead>
                <TableHead>{t("apiBenchmark.success")}</TableHead>
                <TableHead>TTFT</TableHead>
                <TableHead>{t("apiBenchmark.totalTime")}</TableHead>
                <TableHead>tok/s</TableHead>
                <TableHead>char/s</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {result.summaries.map((summary) => (
                <TableRow key={summary.label}>
                  <TableCell>{summary.label}</TableCell>
                  <TableCell>{summary.success}</TableCell>
                  <TableCell>{formatMetric(summary.ttftSecMedian)}</TableCell>
                  <TableCell>
                    {formatMetric(summary.totalTimeSecMedian)}
                  </TableCell>
                  <TableCell>
                    {formatMetric(summary.tokensPerSecMedian, 2)}
                  </TableCell>
                  <TableCell>
                    {formatMetric(summary.charsPerSecMedian, 1)}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>

          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-10"></TableHead>
                <TableHead>{t("usage.time")}</TableHead>
                <TableHead>{t("usage.provider")}</TableHead>
                <TableHead>{t("usage.status")}</TableHead>
                <TableHead>TTFT</TableHead>
                <TableHead>{t("apiBenchmark.totalTime")}</TableHead>
                <TableHead>{t("usage.outputTokens")}</TableHead>
                <TableHead>{t("apiBenchmark.codePass")}</TableHead>
                <TableHead>{t("usage.errorMessage")}</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {result.rows.map((row, index) => {
                const key = rowKey(row, index);
                const expanded = expandedRows.has(key);
                const isRerunning = rerunningRows.has(key);
                const isRegenerating = regeneratingRows.has(key);
                const status = statusForRow(row, key);

                return (
                  <Fragment key={key}>
                    <TableRow>
                      <TableCell>
                        <Button
                          type="button"
                          variant="ghost"
                          size="icon"
                          className="h-7 w-7"
                          onClick={() => toggleRow(key)}
                          aria-label={t("apiBenchmark.toggleDetails")}
                        >
                          {expanded ? (
                            <ChevronDown className="h-4 w-4" />
                          ) : (
                            <ChevronRight className="h-4 w-4" />
                          )}
                        </Button>
                      </TableCell>
                      <TableCell className="font-mono text-xs">
                        {row.time}
                      </TableCell>
                      <TableCell>{row.label}</TableCell>
                      <TableCell>
                        <Button
                          type="button"
                          variant="outline"
                          size="sm"
                          className={statusButtonClassName(status)}
                          onClick={() => toggleRow(key)}
                          aria-label={statusButtonLabel(status)}
                        >
                          {status === "queued" ? (
                            <Clock3 className="h-3.5 w-3.5" />
                          ) : status === "running" ? (
                            <Loader2 className="h-3.5 w-3.5 animate-spin" />
                          ) : status === "failed" ? (
                            <XCircle className="h-3.5 w-3.5" />
                          ) : (
                            <CheckCircle2 className="h-3.5 w-3.5" />
                          )}
                          {statusButtonLabel(status)}
                          {row.statusCode ? (
                            <span className="font-mono">{row.statusCode}</span>
                          ) : null}
                        </Button>
                      </TableCell>
                      <TableCell>{formatMetric(row.ttftSec)}</TableCell>
                      <TableCell>{formatMetric(row.totalTimeSec)}</TableCell>
                      <TableCell>{row.outputTokens ?? "--"}</TableCell>
                      <TableCell
                        className={
                          isCodeError(row.codeEvaluation)
                            ? "font-medium text-destructive"
                            : undefined
                        }
                      >
                        {formatCodeEvaluation(row.codeEvaluation)}
                      </TableCell>
                      <TableCell className="max-w-[280px] truncate text-destructive">
                        {row.error}
                      </TableCell>
                    </TableRow>
                    {expanded && (
                      <TableRow>
                        <TableCell colSpan={9} className="bg-muted/20 p-4">
                          <div className="space-y-4">
                            <div className="flex flex-wrap items-center gap-2">
                              {row.taskTitle && (
                                <Badge variant="outline">{row.taskTitle}</Badge>
                              )}
                              <Button
                                type="button"
                                variant="outline"
                                size="sm"
                                onClick={() => void regenerateRow(row, key)}
                                disabled={
                                  isRegenerating || isRerunning || isRunning
                                }
                              >
                                {isRegenerating ? (
                                  <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                                ) : (
                                  <RotateCcw className="mr-2 h-4 w-4" />
                                )}
                                {t("apiBenchmark.regenerateCode")}
                              </Button>
                              {row.promptKind === "code" && (
                                <Button
                                  type="button"
                                  variant="outline"
                                  size="sm"
                                  onClick={() => void rerunPython(row, key)}
                                  disabled={isRerunning || isRegenerating}
                                >
                                  {isRerunning ? (
                                    <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                                  ) : (
                                    <Terminal className="mr-2 h-4 w-4" />
                                  )}
                                  {t("apiBenchmark.rerunPython")}
                                </Button>
                              )}
                              {row.codeEvaluation && (
                                <Badge variant="secondary">
                                  {formatCodeEvaluation(row.codeEvaluation)}
                                </Badge>
                              )}
                            </div>

                            <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
                              <div className="space-y-2">
                                <Label>{t("apiBenchmark.requestPrompt")}</Label>
                                <Textarea
                                  readOnly
                                  value={row.prompt}
                                  className="min-h-[180px] font-mono text-xs"
                                />
                              </div>
                              <div className="space-y-2">
                                <Label className="flex items-center gap-2">
                                  <Code2 className="h-4 w-4" />
                                  {t("apiBenchmark.generatedCode")}
                                </Label>
                                <Textarea
                                  readOnly
                                  value={displayCodeText(row)}
                                  className="min-h-[180px] font-mono text-xs"
                                />
                              </div>
                            </div>

                            {row.codeEvaluation?.caseResults?.length ? (
                              <div className="space-y-3">
                                <div className="text-sm font-medium">
                                  {t("apiBenchmark.pythonOutputs")}
                                </div>
                                {row.codeEvaluation.caseResults.map(
                                  (caseResult) => (
                                    <div
                                      key={caseResult.caseId}
                                      className="rounded-md border border-border-default p-3"
                                    >
                                      <div className="mb-3 flex items-center justify-between gap-2">
                                        <div className="font-mono text-xs">
                                          {caseResult.caseId}
                                        </div>
                                        <Badge
                                          variant={
                                            caseResult.passed
                                              ? "secondary"
                                              : "destructive"
                                          }
                                        >
                                          {caseResult.passed
                                            ? t("apiBenchmark.casePassed")
                                            : t("apiBenchmark.caseFailed")}
                                        </Badge>
                                      </div>
                                      <div className="overflow-x-auto">
                                        <div className="grid min-w-[880px] grid-cols-2 gap-3">
                                          <div className="space-y-2">
                                            <Label>
                                              {t("apiBenchmark.pythonStdout")}
                                            </Label>
                                            <ScrollArea className="max-h-[320px]">
                                              <pre className={preClassName()}>
                                                {caseResult.stdout ||
                                                  caseResult.stderr ||
                                                  ""}
                                              </pre>
                                            </ScrollArea>
                                          </div>
                                          <div className="space-y-2">
                                            <Label>
                                              {t("apiBenchmark.expectedStdout")}
                                            </Label>
                                            <ScrollArea className="max-h-[320px]">
                                              <pre className={preClassName()}>
                                                {caseResult.expectedStdout}
                                              </pre>
                                            </ScrollArea>
                                          </div>
                                        </div>
                                      </div>
                                    </div>
                                  ),
                                )}
                              </div>
                            ) : null}
                          </div>
                        </TableCell>
                      </TableRow>
                    )}
                  </Fragment>
                );
              })}
            </TableBody>
          </Table>
        </div>
      )}
    </div>
  );
}
