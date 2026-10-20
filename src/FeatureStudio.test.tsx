import { randomFillSync } from "node:crypto";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, describe, expect, it } from "vitest";
import FeatureStudio from "./FeatureStudio";
import type {
  ActiveFeatureJob,
  FeatureCommitResult,
  FeatureInputCandidate,
  FeaturePreviewSession,
  FeatureRegistrySnapshot,
  FeatureSetCatalogEntry,
  FeatureSetRequest,
} from "./feature-studio-types";

// UI fixtures only: values below mimic the backend IPC contract and are not market data.
const registry: FeatureRegistrySnapshot = {
  engineId: "rg-native-features",
  engineVersion: "1.0.0",
  temporalSemantics: {
    version: 1,
    decision_boundary: "Each row's decision time is its target bar's available_at_utc.",
    current_bar: "The target bar that closes at the decision time is included.",
    window_basis: "Windows count observed closed bars of one series.",
    warmup: "Rows before an indicator is fully seeded are null with status WARMUP.",
    reference_join: "A reference value is visible only when available at or before the decision time.",
    reference_staleness: "A reference older than its declared maximum staleness is STALE.",
    gaps: "No resampling, synthesis, centered windows, or forward labels.",
  },
  indicators: [
    {
      id: "SMA",
      version: 1,
      name: "Simple moving average",
      description: "Arithmetic mean of the selected price over the window.",
      formula: "sum(x[t-n+1..=t]) / n",
      warmup_rule: "First n-1 rows are WARMUP",
      inputs: "SELECTED_PRICE",
      outputs: ["value"],
      parameters: [
        { name: "period", description: "Number of observed closed bars", kind: "INTEGER", min: 1, max: 500, default: 20 },
        { name: "price", description: "Bar field used as the indicator input", kind: "CHOICE", options: ["OPEN", "HIGH", "LOW", "CLOSE", "TYPICAL"], default: "CLOSE" },
      ],
    },
  ],
  limits: { maxFeatures: 32, maxPeriod: 500, maxStalenessSeconds: 2678400, maxAvailabilityLagSeconds: 604800, previewRows: 20 },
  researchNotice: "Derived research features computed from registered datasets. They are not trading signals, backtest results, or evidence of edge.",
};

const readyDataset = `ds_${"a".repeat(64)}`;
const quarantinedDataset = `ds_${"b".repeat(64)}`;
const inputs: FeatureInputCandidate[] = [
  {
    dataset_id: readyDataset,
    status: "READY",
    eligible: true,
    reason: null,
    source_file_name: "synthetic-fixture-ui.csv",
    source_id: "synthetic-fixture:ui-test",
    broker: "SYNTHETIC-FIXTURE",
    server: "SYNTHETIC-FIXTURE",
    created_at_utc: "2026-10-20T00:00:00.000000Z",
    series: [{ symbol: "EURUSD.m", timeframe: "H1", market_role: "TARGET", bar_count: 30, first_open_utc: "2024-01-01T00:00:00.000000Z", last_open_utc: "2024-01-02T05:00:00.000000Z", missing_bars: 0, closed_session_bars: 0, unclassified_gap_bars: 0 }],
  },
  {
    dataset_id: quarantinedDataset,
    status: "QUARANTINED",
    eligible: false,
    reason: "Snapshot failed integrity verification",
    source_file_name: "tampered-fixture.csv",
    source_id: "synthetic-fixture:tampered",
    broker: "SYNTHETIC-FIXTURE",
    server: "SYNTHETIC-FIXTURE",
    created_at_utc: null,
    series: [],
  },
];

const featureSetId = `fs_${"c".repeat(64)}`;
const jobId = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c81";

function previewSession(): FeaturePreviewSession {
  return {
    jobId,
    preview: {
      feature_set_id: featureSetId,
      manifest_sha256: "d".repeat(64),
      manifest: {
        schema_version: 1,
        feature_set_id: featureSetId,
        identity_sha256: "c".repeat(64),
        identity: {
          schema_version: 1,
          engine_id: "rg-native-features",
          engine_version: "1.0.0",
          temporal_semantics: registry.temporalSemantics,
          target: { dataset_id: readyDataset, symbol: "EURUSD.m", timeframe: "H1" },
          features: [{ key: "sma_20", indicator_id: "SMA", indicator_version: 1, parameters: { period: 3, price: "CLOSE" }, source: { kind: "TARGET" }, outputs: ["value"], warmup_bars: 2 }],
          inputs: [{ dataset_id: readyDataset, fingerprint: "a".repeat(64), canonical_bars_sha256: "e".repeat(64), canonical_bars_fingerprint: "f".repeat(64), bar_count: 30 }],
        },
        created_at_utc: "2026-10-20T00:00:01.000000Z",
        output_file: "features.parquet",
        output_sha256: "1".repeat(64),
        output_fingerprint: "2".repeat(64),
        row_count: 30,
        columns: [],
        feature_summaries: [{ key: "sma_20", ready_rows: 28, warmup_rows: 2, stale_rows: 0, no_observation_rows: 0, first_ready_decision_utc: "2024-01-01T03:00:00.000000Z" }],
        lineage: [{ dataset_id: readyDataset, fingerprint: "a".repeat(64), source_id: "synthetic-fixture:ui-test", broker: "SYNTHETIC-FIXTURE", server: "SYNTHETIC-FIXTURE", source_file_name: "synthetic-fixture-ui.csv", dataset_created_at_utc: "2026-10-20T00:00:00.000000Z", series_used: ["EURUSD.m H1"] }],
        status: "READY",
        research_notice: registry.researchNotice,
      },
      sample_rows: [
        { open_time_utc: "2024-01-01T01:00:00.000000Z", decision_time_utc: "2024-01-01T02:00:00.000000Z", features: [{ key: "sma_20", status: "WARMUP", values: [null], source_available_at_utc: null }] },
        { open_time_utc: "2024-01-01T02:00:00.000000Z", decision_time_utc: "2024-01-01T03:00:00.000000Z", features: [{ key: "sma_20", status: "READY", values: [1.1002346], source_available_at_utc: null }] },
      ],
    },
  };
}

function catalogEntry(): FeatureSetCatalogEntry {
  const manifest = previewSession().preview.manifest;
  return { feature_set_id: featureSetId, identity_sha256: manifest.identity_sha256, manifest_sha256: "d".repeat(64), status: "READY", manifest, integrity: { manifest_valid: true, output_valid: true, lineage_valid: true }, diagnostic: null };
}

interface Backend {
  inputs?: FeatureInputCandidate[];
  active?: () => ActiveFeatureJob | null;
  preview?: (request: FeatureSetRequest) => FeaturePreviewSession;
  confirm?: () => FeatureCommitResult;
  listSets?: () => FeatureSetCatalogEntry[];
  cancel?: () => string;
  calls?: string[];
}

function mockBackend(backend: Backend) {
  mockIPC((command, payload) => {
    backend.calls?.push(command);
    const args = (payload ?? {}) as Record<string, unknown>;
    switch (command) {
      case "get_feature_registry": return registry;
      case "list_feature_inputs": return backend.inputs ?? inputs;
      case "list_feature_sets": return backend.listSets?.() ?? [];
      case "list_feature_attempts": return [];
      case "get_active_feature_job": return backend.active?.() ?? null;
      case "preview_feature_set": return backend.preview?.(args.request as FeatureSetRequest);
      case "confirm_feature_set": return backend.confirm?.();
      case "cancel_feature_job": return backend.cancel?.() ?? "CANCELLED";
      default: return undefined;
    }
  });
}

async function addFeatureWhenLoaded() {
  await screen.findByText(/not trading signals/);
  fireEvent.click(screen.getByRole("button", { name: "Add feature" }));
}

async function addThreeBarSma() {
  await addFeatureWhenLoaded();
  fireEvent.change(screen.getByLabelText(/^period/), { target: { value: "3" } });
}

describe("gate feature_studio_ui", () => {
  beforeAll(() => {
    Object.defineProperty(window, "crypto", {
      configurable: true,
      value: { getRandomValues: (buffer: Uint8Array) => randomFillSync(buffer) },
    });
  });

  it("shows verified inputs, the typed registry, and the timing contract without inventing results", async () => {
    mockBackend({});
    render(<FeatureStudio />);
    expect(await screen.findByText(/not trading signals, backtest results, or evidence of edge/)).toBeTruthy();
    const target = await screen.findByLabelText("Target dataset");
    const options = Array.from((target as HTMLSelectElement).options);
    expect(options.find((option) => option.value === readyDataset)?.disabled).toBe(false);
    expect(options.find((option) => option.value === quarantinedDataset)?.disabled).toBe(true);
    expect(screen.getByText(/tampered-fixture.csv: Snapshot failed integrity verification/)).toBeTruthy();
    expect(screen.getByText(/synthetic-fixture:ui-test/)).toBeTruthy();
    expect(screen.getByText(registry.temporalSemantics.reference_join)).toBeTruthy();
    expect(screen.getByText("No feature sets registered yet.")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Validate and preview" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByText(/FEATURE PREVIEW/)).toBeNull();
  });

  it("previews source-backed values, registers only after explicit confirmation, and shows lineage", async () => {
    let committed = false;
    const requests: FeatureSetRequest[] = [];
    mockBackend({
      preview: (request) => { requests.push(request); return previewSession(); },
      confirm: () => { committed = true; return { feature_set_id: featureSetId, status: "READY", disposition: "MATERIALIZED" }; },
      listSets: () => committed ? [catalogEntry()] : [],
    });
    render(<FeatureStudio />);
    await addThreeBarSma();
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview" }));
    expect(await screen.findByText(/FEATURE PREVIEW · NOT YET REGISTERED/)).toBeTruthy();
    expect(requests).toHaveLength(1);
    expect(requests[0].target).toEqual({ dataset_id: readyDataset, symbol: "EURUSD.m", timeframe: "H1" });
    expect(requests[0].features[0]).toEqual({ key: "sma_20", indicator_id: "SMA", indicator_version: 1, parameters: { period: 3, price: "CLOSE" }, source: { kind: "TARGET" } });
    expect(requests[0].quotas.max_output_bytes).toBeGreaterThan(0);
    const values = screen.getByRole("table", { name: /Most recent 2 computed rows/ });
    expect(within(values).getByText("1.100235")).toBeTruthy();
    expect(within(values).getByText("WARMUP")).toBeTruthy();
    expect(screen.getByRole("table", { name: "Input lineage" })).toBeTruthy();
    expect(screen.getByText("No feature sets registered yet.")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Freeze and register feature set" }));
    expect(await screen.findByText(/Feature set frozen and registered/)).toBeTruthy();
    expect(await screen.findByText("FEATURE SET INSPECTION")).toBeTruthy();
    expect(screen.getByText("Dataset lineage: verified")).toBeTruthy();
    expect(screen.queryByText(/FEATURE PREVIEW/)).toBeNull();
  });

  it("keeps a successful commit separate from a failed catalogue refresh", async () => {
    let committed = false;
    mockBackend({
      preview: () => previewSession(),
      confirm: () => { committed = true; return { feature_set_id: featureSetId, status: "READY", disposition: "MATERIALIZED" }; },
      listSets: () => { if (committed) throw new Error("catalogue verification busy"); return []; },
    });
    render(<FeatureStudio />);
    await addThreeBarSma();
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview" }));
    fireEvent.click(await screen.findByRole("button", { name: "Freeze and register feature set" }));
    expect(await screen.findByText(/Feature set frozen and registered/)).toBeTruthy();
    expect(await screen.findByText(/The feature set was committed, but the catalogue did not refresh: catalogue verification busy/)).toBeTruthy();
    expect(screen.queryByText(/was not registered/)).toBeNull();
  });

  it("drops a consumed preview when confirmation fails so it cannot be confirmed twice", async () => {
    mockBackend({
      preview: () => previewSession(),
      confirm: () => { throw new Error("prepared feature set failed verification"); },
    });
    render(<FeatureStudio />);
    await addThreeBarSma();
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview" }));
    fireEvent.click(await screen.findByRole("button", { name: "Freeze and register feature set" }));
    expect(await screen.findByText(/The feature set was not registered: prepared feature set failed verification/)).toBeTruthy();
    expect(screen.queryByText(/FEATURE PREVIEW/)).toBeNull();
    expect(screen.queryByRole("button", { name: "Freeze and register feature set" })).toBeNull();
    expect(screen.getByRole("button", { name: "Validate and preview" })).toBeTruthy();
  });

  it("restores a running computation with progress and requests cancellation", async () => {
    let running = true;
    const calls: string[] = [];
    mockBackend({
      calls,
      active: () => running ? { state: "RUNNING", jobId, progress: { phase: "COMPUTING", processed_rows: 5, total_rows: 30 } } : null,
      cancel: () => { running = false; return "CANCELLATION_REQUESTED"; },
    });
    render(<FeatureStudio />);
    expect(await screen.findByText(/Computing causal features · 5 \/ 30 rows/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel computation" }));
    expect(await screen.findByText(/Cancellation requested/)).toBeTruthy();
    expect(calls).toContain("cancel_feature_job");
    expect(await screen.findByText(/ended before a preview was prepared/)).toBeTruthy();
  });

  it("rejects out-of-range parameters before calling the backend", async () => {
    const calls: string[] = [];
    mockBackend({ calls, preview: () => previewSession() });
    render(<FeatureStudio />);
    await addFeatureWhenLoaded();
    fireEvent.change(screen.getByLabelText(/^period/), { target: { value: "0" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview" }));
    expect(await screen.findByText(/period must be a whole number between 1 and 500/)).toBeTruthy();
    expect(calls).not.toContain("preview_feature_set");
  });

  it("explains that a READY verified dataset is required when none exists", async () => {
    mockBackend({ inputs: [] });
    render(<FeatureStudio />);
    expect(await screen.findByText(/No READY verified dataset is registered/)).toBeTruthy();
    expect((screen.getByRole("button", { name: "Validate and preview" }) as HTMLButtonElement).disabled).toBe(true);
  });
});
