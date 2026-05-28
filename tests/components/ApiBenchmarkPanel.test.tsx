import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ApiBenchmarkPanel } from "@/components/usage/ApiBenchmarkPanel";
import type {
  ApiBenchmarkResultRow,
  ApiBenchmarkRunResult,
} from "@/lib/api/api-benchmark";
import { emitTauriEvent } from "../msw/tauriMocks";

const runProviderMock = vi.hoisted(() => vi.fn());
const runMock = vi.hoisted(() => vi.fn());
const updateConcurrencyMock = vi.hoisted(() => vi.fn());
const rerunPythonMock = vi.hoisted(() => vi.fn());
const toastSuccessMock = vi.hoisted(() => vi.fn());
const toastErrorMock = vi.hoisted(() => vi.fn());

vi.mock("@/lib/api/api-benchmark", () => ({
  apiBenchmarkApi: {
    runProvider: (...args: unknown[]) => runProviderMock(...args),
    run: (...args: unknown[]) => runMock(...args),
    updateConcurrency: (...args: unknown[]) => updateConcurrencyMock(...args),
    rerunPython: (...args: unknown[]) => rerunPythonMock(...args),
  },
}));

vi.mock("sonner", () => ({
  toast: {
    success: (...args: unknown[]) => toastSuccessMock(...args),
    error: (...args: unknown[]) => toastErrorMock(...args),
  },
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => key,
  }),
}));

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((promiseResolve, promiseReject) => {
    resolve = promiseResolve;
    reject = promiseReject;
  });
  return { promise, resolve, reject };
}

function benchmarkRow(
  overrides: Partial<ApiBenchmarkResultRow> = {},
): ApiBenchmarkResultRow {
  return {
    rowKey: "0:code:task-a",
    entryIndex: 0,
    promptKind: "code",
    taskId: "task-a",
    taskTitle: "Task A",
    prompt: "Write Python for Task A",
    responseText: "",
    time: "2026-05-16T10:00:00",
    label: "Provider / model-a",
    modelRequested: "model-a",
    modelReturned: "",
    serviceTier: "",
    systemFingerprint: "",
    statusCode: null,
    requestId: "",
    ttftSec: null,
    totalTimeSec: 0,
    genTimeSec: null,
    inputTokens: null,
    outputTokens: null,
    totalTokens: null,
    tokensPerSec: null,
    chars: 0,
    charsPerSec: null,
    error: "",
    codeEvaluation: null,
    ...overrides,
  };
}

describe("ApiBenchmarkPanel", () => {
  beforeEach(() => {
    runProviderMock.mockReset();
    runMock.mockReset();
    updateConcurrencyMock.mockReset();
    updateConcurrencyMock.mockResolvedValue(true);
    rerunPythonMock.mockReset();
    toastSuccessMock.mockReset();
    toastErrorMock.mockReset();
  });

  it("shows a clickable status button for a row while the benchmark command is still running", async () => {
    const pendingRun = deferred<ApiBenchmarkRunResult>();
    runProviderMock.mockReturnValue(pendingRun.promise);

    render(
      <ApiBenchmarkPanel
        appId="codex"
        providerId="provider-a"
        providerName="Provider A"
      />,
    );

    await waitFor(() => expect(runProviderMock).toHaveBeenCalledTimes(1));
    const options = runProviderMock.mock.calls[0]?.[2] as {
      maxConcurrency: number;
      progressRunId: string;
    };
    expect(options.maxConcurrency).toBe(2);

    await act(async () => {
      emitTauriEvent("api-benchmark-progress", {
        runId: options.progressRunId,
        event: "rowStarted",
        rowKey: "0:code:task-a",
        row: benchmarkRow(),
        completed: 0,
        total: 2,
      });
    });

    const runningButton = await screen.findByRole("button", {
      name: /apiBenchmark\.rowStatusRunning/,
    });
    expect(runningButton).toBeInTheDocument();

    await act(async () => {
      emitTauriEvent("api-benchmark-progress", {
        runId: options.progressRunId,
        event: "rowCompleted",
        rowKey: "0:code:task-a",
        row: benchmarkRow({
          statusCode: 503,
          error: "HTTP 503: upstream unavailable",
        }),
        completed: 1,
        total: 2,
      });
    });

    const failedButton = await screen.findByRole("button", {
      name: /apiBenchmark\.rowStatusFailed/,
    });
    fireEvent.click(failedButton);

    expect(
      screen.getByDisplayValue("HTTP 503: upstream unavailable"),
    ).toBeInTheDocument();

    await act(async () => {
      pendingRun.resolve({
        rows: [
          benchmarkRow({
            statusCode: 503,
            error: "HTTP 503: upstream unavailable",
          }),
        ],
        summaries: [],
      });
      await pendingRun.promise;
    });
  });

  it("shows a queued status before a row starts running", async () => {
    const pendingRun = deferred<ApiBenchmarkRunResult>();
    runProviderMock.mockReturnValue(pendingRun.promise);

    render(
      <ApiBenchmarkPanel
        appId="codex"
        providerId="provider-a"
        providerName="Provider A"
      />,
    );

    await waitFor(() => expect(runProviderMock).toHaveBeenCalledTimes(1));
    const options = runProviderMock.mock.calls[0]?.[2] as {
      progressRunId: string;
    };

    await act(async () => {
      emitTauriEvent("api-benchmark-progress", {
        runId: options.progressRunId,
        event: "rowQueued",
        rowKey: "0:code:task-a",
        row: benchmarkRow(),
        completed: 0,
        total: 2,
      });
    });

    const queuedButton = await screen.findByRole("button", {
      name: /apiBenchmark\.rowStatusQueued/,
    });
    expect(queuedButton).toBeInTheDocument();

    await act(async () => {
      emitTauriEvent("api-benchmark-progress", {
        runId: options.progressRunId,
        event: "rowStarted",
        rowKey: "0:code:task-a",
        row: benchmarkRow(),
        completed: 0,
        total: 2,
      });
    });

    const runningButton = await screen.findByRole("button", {
      name: /apiBenchmark\.rowStatusRunning/,
    });
    expect(runningButton).toBeInTheDocument();

    await act(async () => {
      pendingRun.resolve({ rows: [benchmarkRow()], summaries: [] });
      await pendingRun.promise;
    });
  });

  it("updates the active benchmark concurrency without restarting the run", async () => {
    const pendingRun = deferred<ApiBenchmarkRunResult>();
    runProviderMock.mockReturnValue(pendingRun.promise);

    render(
      <ApiBenchmarkPanel
        appId="codex"
        providerId="provider-a"
        providerName="Provider A"
      />,
    );

    await waitFor(() => expect(runProviderMock).toHaveBeenCalledTimes(1));
    const options = runProviderMock.mock.calls[0]?.[2] as {
      maxConcurrency: number;
      progressRunId: string;
    };

    const concurrencyInput = screen.getByLabelText("apiBenchmark.concurrency");
    fireEvent.change(concurrencyInput, { target: { value: "4" } });
    fireEvent.keyDown(concurrencyInput, { key: "Enter", code: "Enter" });

    await waitFor(() => expect(updateConcurrencyMock).toHaveBeenCalledTimes(1));
    expect(updateConcurrencyMock).toHaveBeenCalledWith(options.progressRunId, 4);
    expect(runProviderMock).toHaveBeenCalledTimes(1);

    await act(async () => {
      pendingRun.resolve({ rows: [benchmarkRow()], summaries: [] });
      await pendingRun.promise;
    });
  });

  it("retries only failed rows without rerunning completed rows", async () => {
    const passedRow = benchmarkRow({
      rowKey: "0:code:task-a",
      taskId: "task-a",
      codeEvaluation: {
        taskId: "task-a",
        taskTitle: "Task A",
        codeExtracted: true,
        syntaxOk: true,
        runnable: true,
        passedTests: 2,
        totalTests: 2,
        caseResults: [],
        score: 100,
        executionTimeSec: 0.2,
        failureReason: null,
        extractedCode: "print('ok')",
      },
    });
    const failedRow = benchmarkRow({
      rowKey: "0:code:task-b",
      taskId: "task-b",
      taskTitle: "Task B",
      codeEvaluation: {
        taskId: "task-b",
        taskTitle: "Task B",
        codeExtracted: true,
        syntaxOk: true,
        runnable: true,
        passedTests: 1,
        totalTests: 2,
        caseResults: [],
        score: 70,
        executionTimeSec: 0.3,
        failureReason: "Wrong output",
        extractedCode: "print('bad')",
      },
    });
    runProviderMock.mockResolvedValue({
      rows: [passedRow, failedRow],
      summaries: [],
    });
    runMock.mockResolvedValue({
      rows: [
        benchmarkRow({
          rowKey: "0:code:task-b",
          taskId: "task-b",
          taskTitle: "Task B",
          codeEvaluation: {
            taskId: "task-b",
            taskTitle: "Task B",
            codeExtracted: true,
            syntaxOk: true,
            runnable: true,
            passedTests: 2,
            totalTests: 2,
            caseResults: [],
            score: 100,
            executionTimeSec: 0.2,
            failureReason: null,
            extractedCode: "print('fixed')",
          },
        }),
      ],
      summaries: [],
    });

    render(
      <ApiBenchmarkPanel
        appId="codex"
        providerId="provider-a"
        providerName="Provider A"
      />,
    );

    const retryButton = await screen.findByRole("button", {
      name: /apiBenchmark\.retryFailed/,
    });
    fireEvent.click(retryButton);

    await waitFor(() => expect(runMock).toHaveBeenCalledTimes(1));
    expect(runProviderMock).toHaveBeenCalledTimes(1);
    expect(runMock).toHaveBeenCalledWith(
      "codex",
      [0],
      expect.objectContaining({
        taskId: "task-b",
        maxConcurrency: 2,
      }),
    );
  });
});
