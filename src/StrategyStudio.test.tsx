import { randomFillSync } from "node:crypto";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, describe, expect, it } from "vitest";
import StrategyStudio from "./StrategyStudio";
import type {
  ActiveDecisionJob,
  DecisionCommitResult,
  DecisionInputCandidate,
  DecisionPreviewSession,
  DecisionSetCatalogEntry,
  DecisionSetRequest,
  StrategyRegistrySnapshot,
} from "./strategy-studio-types";

// UI fixtures only: values mimic the backend IPC contract and are not market data or results.
const registry: StrategyRegistrySnapshot = {
  engineId: "rg-native-strategies",
  engineVersion: "1.0.0",
  decisionSemantics: {
    version: 1,
    decision_rows: "Exactly one decision per row of the consumed feature set.",
    causality: "A decision reads only the feature values of its own row.",
    context: "Reference-series features are context inputs only. They can filter a target decision but never create a decision row of their own.",
    skip_precedence: "Bound inputs are checked in the definition's role order.",
    equality: "Directional thresholds are strict.",
    conflict: "BUY and SELL are never emitted together.",
  },
  strategies: [
    {
      id: "TREND_CONTEXT",
      version: 1,
      name: "Trend with context filter",
      description: "Compares a fast and a slow target-series feature, gated by a reference-market context feature.",
      decision_rule: "BUY when fast - slow > margin",
      equality_semantics: "Trend comparisons are strict.",
      warmup_rule: "No extra warmup.",
      bindings: [
        { role: "fast", description: "Faster target feature", source: "TARGET" },
        { role: "slow", description: "Slower target feature", source: "TARGET" },
        { role: "context", description: "Reference context", source: "REFERENCE" },
      ],
      parameters: [
        { name: "margin", description: "Minimum separation", min: 0, max: 1e9, default: 0 },
        { name: "context_min", description: "Lowest context value", min: -1e9, max: 1e9, default: 0 },
        { name: "context_max", description: "Highest context value", min: -1e9, max: 1e9, default: 100 },
      ],
    },
    {
      id: "THRESHOLD_BAND",
      version: 1,
      name: "Threshold band",
      description: "Compares one target-series feature with two thresholds.",
      decision_rule: "BUY when signal > upper",
      equality_semantics: "Comparisons are strict.",
      warmup_rule: "No extra warmup.",
      bindings: [{ role: "signal", description: "Target feature", source: "TARGET" }],
      parameters: [
        { name: "upper", description: "Upper threshold", min: -1e9, max: 1e9, default: 70 },
        { name: "lower", description: "Lower threshold", min: -1e9, max: 1e9, default: 30 },
      ],
    },
  ],
  previewRows: 20,
  researchNotice: "Research decisions from a deterministic rule over a registered feature set. They are not orders.",
};

const readySet = `fs_${"a".repeat(64)}`;
const blockedSet = `fs_${"b".repeat(64)}`;
const dataset = `ds_${"c".repeat(64)}`;
const target = { dataset_id: dataset, symbol: "EURUSD.m", timeframe: "H1" };
const context = { dataset_id: dataset, symbol: "GBPUSD.m", timeframe: "H4" };

const inputs: DecisionInputCandidate[] = [
  {
    feature_set_id: readySet,
    status: "READY",
    eligible: true,
    reason: null,
    target,
    row_count: 48,
    created_at_utc: "2026-10-20T00:00:00.000000Z",
    features: [
      { key: "fast", indicator_id: "SMA", source: "TARGET", reference: null, outputs: ["value"] },
      { key: "slow", indicator_id: "SMA", source: "TARGET", reference: null, outputs: ["value"] },
      { key: "ctx", indicator_id: "RSI_WILDER", source: "REFERENCE", reference: context, outputs: ["value"] },
    ],
  },
  {
    feature_set_id: blockedSet,
    status: "QUARANTINED",
    eligible: false,
    reason: `source dataset ${dataset} failed snapshot verification`,
    target,
    row_count: 48,
    created_at_utc: null,
    features: [],
  },
];

const decisionSetId = `dc_${"d".repeat(64)}`;
const jobId = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c93";

function previewSession(): DecisionPreviewSession {
  return {
    jobId,
    preview: {
      decision_set_id: decisionSetId,
      manifest_sha256: "e".repeat(64),
      manifest: {
        schema_version: 1,
        decision_set_id: decisionSetId,
        identity_sha256: "d".repeat(64),
        identity: {
          schema_version: 1,
          engine_id: "rg-native-strategies",
          engine_version: "1.0.0",
          decision_semantics: registry.decisionSemantics,
          strategy: {
            strategy_id: "TREND_CONTEXT",
            strategy_version: 1,
            parameters: { margin: 0, context_min: 0, context_max: 100 },
            bindings: [
              { role: "fast", source: "TARGET", feature_key: "fast", output: "value" },
              { role: "slow", source: "TARGET", feature_key: "slow", output: "value" },
              { role: "context", source: "REFERENCE", feature_key: "ctx", output: "value" },
            ],
          },
          feature_set: { feature_set_id: readySet, identity_sha256: "a".repeat(64), manifest_sha256: "f".repeat(64), output_sha256: "1".repeat(64), output_fingerprint: "2".repeat(64), row_count: 48, target },
        },
        created_at_utc: "2026-10-20T00:00:01.000000Z",
        output_file: "decisions.parquet",
        output_sha256: "3".repeat(64),
        output_fingerprint: "4".repeat(64),
        row_count: 48,
        summary: { buy_rows: 11, sell_rows: 9, skip_rows: 28, skip_reasons: { WARMUP: 15, STALE: 13 }, first_decision_utc: "2024-01-01T01:00:00.000000Z", last_decision_utc: "2024-01-03T00:00:00.000000Z" },
        lineage: { feature_set_id: readySet, target, context_series: [context], datasets: [] },
        status: "READY",
        research_notice: registry.researchNotice,
      },
      sample_rows: [
        { open_time_utc: "2024-01-01T00:00:00.000000Z", decision_time_utc: "2024-01-01T01:00:00.000000Z", decision: "SKIP", reason_code: "WARMUP", reason_role: "fast", inputs: [{ role: "fast", status: "WARMUP", value: null }, { role: "slow", status: "WARMUP", value: null }, { role: "context", status: "NO_OBSERVATION", value: null }] },
        { open_time_utc: "2024-01-02T22:00:00.000000Z", decision_time_utc: "2024-01-02T23:00:00.000000Z", decision: "BUY", reason_code: "BUY_RULE", reason_role: null, inputs: [{ role: "fast", status: "READY", value: 1.1023456 }, { role: "slow", status: "READY", value: 1.0987654 }, { role: "context", status: "READY", value: 61.234567 }] },
      ],
    },
  };
}

function catalogEntry(status: "READY" | "QUARANTINED" = "READY"): DecisionSetCatalogEntry {
  const manifest = previewSession().preview.manifest;
  return {
    decision_set_id: decisionSetId,
    identity_sha256: manifest.identity_sha256,
    manifest_sha256: "e".repeat(64),
    feature_set_id: readySet,
    status,
    manifest,
    integrity: { manifest_valid: true, output_valid: true, lineage_valid: status === "READY" },
    diagnostic: status === "READY" ? null : `feature set ${readySet} is QUARANTINED, not READY`,
  };
}

interface Backend {
  inputs?: DecisionInputCandidate[];
  active?: () => ActiveDecisionJob | null;
  preview?: (request: DecisionSetRequest) => DecisionPreviewSession;
  confirm?: () => DecisionCommitResult;
  listSets?: () => DecisionSetCatalogEntry[];
  inspect?: () => DecisionSetCatalogEntry | null;
  cancel?: () => string;
  calls?: string[];
}

function mockBackend(backend: Backend) {
  mockIPC((command, payload) => {
    backend.calls?.push(command);
    const args = (payload ?? {}) as Record<string, unknown>;
    switch (command) {
      case "get_strategy_registry": return registry;
      case "list_decision_inputs": return backend.inputs ?? inputs;
      case "list_decision_sets": return backend.listSets?.() ?? [];
      case "list_decision_attempts": return [];
      case "inspect_decision_set": return backend.inspect?.() ?? null;
      case "get_active_decision_job": return backend.active?.() ?? null;
      case "preview_decision_set": return backend.preview?.(args.request as DecisionSetRequest);
      case "confirm_decision_set": return backend.confirm?.();
      case "cancel_decision_job": return backend.cancel?.() ?? "CANCELLED";
      default: return undefined;
    }
  });
}

async function loaded() {
  await screen.findByText(/They are not orders/);
}

describe("gate desktop_strategy_ipc", () => {
  beforeAll(() => {
    Object.defineProperty(window, "crypto", {
      configurable: true,
      value: { getRandomValues: (buffer: Uint8Array) => randomFillSync(buffer) },
    });
  });

  it("shows verified inputs with unavailable reasons, the typed registry, and the decision contract", async () => {
    mockBackend({});
    render(<StrategyStudio />);
    await loaded();
    const featureSet = screen.getByRole("combobox", { name: "Feature set" }) as HTMLSelectElement;
    expect(featureSet.value).toBe(readySet);
    expect(Array.from(featureSet.options).find((option) => option.value === blockedSet)?.disabled).toBe(true);
    expect(screen.getByText(/failed snapshot verification/)).toBeTruthy();
    expect(screen.getByText(/Target/).textContent).toContain("EURUSD.m H1");
    expect(screen.getByText(registry.decisionSemantics.context)).toBeTruthy();
    expect(screen.getByText("No decision sets registered yet.")).toBeTruthy();
    expect(screen.queryByText(/DECISION PREVIEW/)).toBeNull();
  });

  it("offers each role only features of its required source and pre-fills them", async () => {
    mockBackend({});
    render(<StrategyStudio />);
    await loaded();
    const options = (label: RegExp) => Array.from((screen.getByRole("combobox", { name: label }) as HTMLSelectElement).options).map((option) => option.textContent ?? "");
    expect(options(/^fast/).filter((text) => text.includes("("))).toEqual(["fast (SMA)", "slow (SMA)"]);
    expect(options(/^context /).filter((text) => text.includes("("))).toEqual(["ctx (RSI_WILDER on GBPUSD.m H4)"]);
    expect((screen.getByRole("combobox", { name: /^fast/ }) as HTMLSelectElement).value).toBe("fast\u0000value");
    expect((screen.getByRole("combobox", { name: /^slow/ }) as HTMLSelectElement).value).toBe("slow\u0000value");
    expect((screen.getByRole("combobox", { name: /^context / }) as HTMLSelectElement).value).toBe("ctx\u0000value");
  });

  it("previews decisions with SKIP reasons, registers only after confirmation, and shows lineage", async () => {
    let committed = false;
    const requests: DecisionSetRequest[] = [];
    mockBackend({
      preview: (request) => { requests.push(request); return previewSession(); },
      confirm: () => { committed = true; return { decision_set_id: decisionSetId, status: "READY", disposition: "MATERIALIZED" }; },
      listSets: () => committed ? [catalogEntry()] : [],
    });
    render(<StrategyStudio />);
    await loaded();
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview decisions" }));
    expect(await screen.findByText(/DECISION PREVIEW · NOT YET REGISTERED/)).toBeTruthy();
    expect(requests).toEqual([{
      feature_set_id: readySet,
      strategy_id: "TREND_CONTEXT",
      strategy_version: 1,
      parameters: { margin: 0, context_min: 0, context_max: 100 },
      bindings: { fast: { feature_key: "fast", output: "value" }, slow: { feature_key: "slow", output: "value" }, context: { feature_key: "ctx", output: "value" } },
      quotas: { max_rows: 50000000, max_output_bytes: 8192 * 1024 * 1024 },
    }]);
    expect(screen.getByText(/WARMUP 15 · STALE 13/)).toBeTruthy();
    expect(screen.getByText(/Context only: GBPUSD.m H4/)).toBeTruthy();
    const sample = screen.getByRole("table", { name: /Most recent 2 decisions/ });
    expect(within(sample).getByText("BUY")).toBeTruthy();
    expect(within(sample).getByText("WARMUP · fast")).toBeTruthy();
    expect(screen.getByText("No decision sets registered yet.")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Freeze and register decision set" }));
    expect(await screen.findByText(/Decision set frozen and registered/)).toBeTruthy();
    expect(await screen.findByText("DECISION SET INSPECTION")).toBeTruthy();
    expect(screen.getByText("Feature-set lineage: verified")).toBeTruthy();
    expect(screen.queryByText(/DECISION PREVIEW/)).toBeNull();
  });

  it("keeps a successful commit separate from a failed catalogue refresh", async () => {
    let committed = false;
    mockBackend({
      preview: () => previewSession(),
      confirm: () => { committed = true; return { decision_set_id: decisionSetId, status: "READY", disposition: "MATERIALIZED" }; },
      listSets: () => { if (committed) throw new Error("catalogue verification busy"); return []; },
    });
    render(<StrategyStudio />);
    await loaded();
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview decisions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Freeze and register decision set" }));
    expect(await screen.findByText(/Decision set frozen and registered/)).toBeTruthy();
    expect(await screen.findByText(/The decision set was committed, but the catalogue did not refresh: catalogue verification busy/)).toBeTruthy();
    expect(screen.queryByText(/was not registered/)).toBeNull();
  });

  it("drops a consumed preview when confirmation fails so it cannot be confirmed twice", async () => {
    mockBackend({
      preview: () => previewSession(),
      confirm: () => { throw new Error("feature set changed or lost READY verification after preview"); },
    });
    render(<StrategyStudio />);
    await loaded();
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview decisions" }));
    fireEvent.click(await screen.findByRole("button", { name: "Freeze and register decision set" }));
    expect(await screen.findByText(/The decision set was not registered: feature set changed/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Freeze and register decision set" })).toBeNull();
    expect(screen.getByRole("button", { name: "Validate and preview decisions" })).toBeTruthy();
  });

  it("restores a running evaluation with progress and requests cancellation", async () => {
    let running = true;
    const calls: string[] = [];
    mockBackend({
      calls,
      active: () => running ? { state: "RUNNING", jobId, progress: { phase: "EVALUATING", processed_rows: 12, total_rows: 48 } } : null,
      cancel: () => { running = false; return "CANCELLATION_REQUESTED"; },
    });
    render(<StrategyStudio />);
    expect(await screen.findByText(/Evaluating decisions · 12 \/ 48 rows/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel evaluation" }));
    expect(await screen.findByText(/Cancellation requested/)).toBeTruthy();
    expect(calls).toContain("cancel_decision_job");
    expect(await screen.findByText(/ended before a preview was prepared/)).toBeTruthy();
  });

  it("rejects out-of-range parameters and unbound roles before calling the backend", async () => {
    const calls: string[] = [];
    mockBackend({ calls, preview: () => previewSession() });
    render(<StrategyStudio />);
    await loaded();
    fireEvent.change(screen.getByRole("spinbutton", { name: /^margin/ }), { target: { value: "-1" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview decisions" }));
    expect(await screen.findByText(/margin must be a number between 0 and 1000000000/)).toBeTruthy();
    fireEvent.change(screen.getByRole("spinbutton", { name: /^margin/ }), { target: { value: "0" } });
    fireEvent.change(screen.getByRole("combobox", { name: /^context / }), { target: { value: "" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview decisions" }));
    expect(await screen.findByText(/Bind the context role to a reference-series feature/)).toBeTruthy();
    expect(calls).not.toContain("preview_decision_set");
  });

  it("explains why no feature set is usable and shows a revoked decision set as QUARANTINED", async () => {
    const revoked = catalogEntry("QUARANTINED");
    mockBackend({ inputs: [inputs[1]], listSets: () => [revoked], inspect: () => revoked });
    render(<StrategyStudio />);
    expect(await screen.findByText(/No READY feature set is registered/)).toBeTruthy();
    expect(screen.getByText(/failed snapshot verification/)).toBeTruthy();
    expect((screen.getByRole("button", { name: "Validate and preview decisions" }) as HTMLButtonElement).disabled).toBe(true);
    const catalogue = screen.getByRole("region", { name: "Decision-set catalogue" });
    expect(within(catalogue).getByText("QUARANTINED")).toBeTruthy();
    fireEvent.click(within(catalogue).getByRole("button", { name: /Inspect/ }));
    expect(await screen.findByText("Feature-set lineage: mismatch")).toBeTruthy();
    expect(screen.getByText(/is QUARANTINED, not READY/)).toBeTruthy();
  });
});
