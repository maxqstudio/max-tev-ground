import { randomFillSync } from "node:crypto";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { mockIPC } from "@tauri-apps/api/mocks";
import { beforeAll, describe, expect, it } from "vitest";
import ModelLab from "./ModelLab";
import type {
  ActiveInferenceJob,
  ComparisonReport,
  InferenceCommitResult,
  InferenceInputCandidate,
  InferencePreviewSession,
  InferenceRecordCatalogEntry,
  InferenceRequest,
  ModelCandidate,
  ModelCatalogEntry,
  ModelLabRegistrySnapshot,
  ModelManifest,
  ModelRegistrationRequest,
  OllamaDiscovery,
  RegistrationPreview,
} from "./model-lab-types";

// UI fixtures only: values mimic the backend IPC contract and are not market data, trained
// models, or results.
const registry: ModelLabRegistrySnapshot = {
  registryVersion: "1.0.0",
  adapters: [
    { id: "OLLAMA_LOOPBACK", version: "ollama-generate-json/1", description: "Loopback Ollama models; never pulled." },
    { id: "ONNX_NATIVE", version: "rg-native-onnx/1", description: "Copied ONNX files, built-in interpreter." },
  ],
  defaultOllamaEndpoint: "http://127.0.0.1:11434",
  onnxOperators: ["Gemm", "Relu", "Softmax"],
  onnxOpsets: [7, 21],
  maxOnnxBytes: 268435456,
  ollamaOutputTokens: [16, 1024],
  ollamaContextTokens: [512, 32768],
  requestTimeoutMs: [100, 120000],
  executableAuthority: "ADVISORY_ONLY",
  lockedAuthorities: ["DECISION_FILTER", "DECISION_VETO", "DECISION_OVERRIDE", "POSITION_SIZING", "ORDER_GENERATION"],
  inferenceSemantics: {
    version: 1,
    rows: "Exactly one output row per input row.",
    causality: "A row's model input is built only from that row's own bound values.",
    skip: "A row is SKIPPED when a bound feature is not READY.",
    invalid_output: "An answer that does not match the declared fields is INVALID_OUTPUT.",
    authority: "ADVISORY_ONLY: outputs never alter a strategy decision or emit an order.",
    determinism: "A replay mismatch is recorded as NON_DETERMINISTIC.",
  },
  previewRows: 20,
  modelNotice: "Registration records identity only.",
  inferenceNotice: "ADVISORY_ONLY research outputs of a registered local model. They are not evidence of edge.",
  comparisonNotice: "Descriptive statistics only; not accuracy against outcomes and not evidence of PnL or edge.",
};

const featureSet = `fs_${"a".repeat(64)}`;
const blockedSet = `fs_${"b".repeat(64)}`;
const dataset = `ds_${"c".repeat(64)}`;
const target = { dataset_id: dataset, symbol: "EURUSD.m", timeframe: "H1" };
const modelId = `md_${"d".repeat(64)}`;
const otherModelId = `md_${"e".repeat(64)}`;
const recordId = `ir_${"1".repeat(64)}`;
const otherRecordId = `ir_${"2".repeat(64)}`;
const jobId = "018f4c4f-7d9b-7b9f-a70e-92bb7cf04ca3";

const onnxCandidate: ModelCandidate = {
  candidate_id: "onnx:018f4c4f-7d9b-7b9f-a70e-92bb7cf04ca4",
  adapter: "ONNX_NATIVE",
  model_name: "spread.onnx",
  content_digest: "9".repeat(64),
  eligible: true,
  reason: null,
  registered_model_ids: [],
  ollama: null,
  onnx: {
    ir_version: 8,
    opset: 17,
    producer_name: "rg-synthetic-fixture",
    input: { name: "input", shape: [1, 2], width: 2 },
    outputs: [{ name: "output", shape: [1, 1], width: 1 }],
    operators: { Gemm: 1 },
    parameter_count: 3,
    operations_per_row: 2,
  },
  content_bytes: 210,
};

function modelManifest(id: string, name: string): ModelManifest {
  return {
    schema_version: 1,
    model_id: id,
    identity_sha256: id.slice(3),
    identity: {
      schema_version: 1,
      registry_id: "rg-model-registry",
      registry_version: "1.0.0",
      adapter: "ONNX_NATIVE",
      adapter_version: "rg-native-onnx/1",
      model_name: name,
      content_digest: "9".repeat(64),
      input_fields: ["fast", "slow"],
      outputs: [{ name: "spread", kind: { kind: "FLOAT64", minimum: -10, maximum: 10 } }],
      runtime: { adapter: "ONNX_NATIVE" },
      decoding: {},
      onnx: onnxCandidate.onnx,
      authority: "ADVISORY_ONLY",
    },
    registered_at_utc: "2026-10-21T00:00:00.000000Z",
    content_file: "model.onnx",
    content_bytes: 210,
    source: { kind: "ONNX_FILE", source_file_name: name },
    status: "READY",
    research_notice: registry.modelNotice,
  };
}

function modelEntry(id = modelId, name = "spread.onnx", status: ModelCatalogEntry["status"] = "READY"): ModelCatalogEntry {
  return {
    model_id: id,
    adapter: "ONNX_NATIVE",
    model_name: name,
    status,
    manifest: modelManifest(id, name),
    integrity: { manifest_valid: true, content_valid: status !== "QUARANTINED" },
    diagnostic: status === "READY" ? null : `model ${id} (${name}) file is missing or no longer matches its registered SHA-256`,
  };
}

const inputs: InferenceInputCandidate[] = [
  {
    kind: "FEATURE_SET",
    id: featureSet,
    status: "READY",
    eligible: true,
    reason: null,
    target,
    row_count: 48,
    created_at_utc: "2026-10-21T00:00:00.000000Z",
    columns: [
      { binding: { source: "FEATURE", feature_key: "fast", output: "value" }, label: "fast (SMA)" },
      { binding: { source: "FEATURE", feature_key: "slow", output: "value" }, label: "slow (SMA)" },
    ],
  },
  {
    kind: "FEATURE_SET",
    id: blockedSet,
    status: "QUARANTINED",
    eligible: false,
    reason: `source dataset ${dataset} failed snapshot verification`,
    target,
    row_count: 48,
    created_at_utc: null,
    columns: [],
  },
];

function previewSession(existing: InferencePreviewSession["preview"]["existing"] = null): InferencePreviewSession {
  return {
    jobId,
    preview: {
      record_id: recordId,
      manifest_sha256: "3".repeat(64),
      existing,
      manifest: {
        schema_version: 1,
        record_id: recordId,
        identity_sha256: "1".repeat(64),
        identity: {
          model: { model_id: modelId, identity_sha256: "d".repeat(64), adapter: "ONNX_NATIVE", model_name: "spread.onnx", content_digest: "9".repeat(64) },
          input: { kind: "FEATURE_SET", id: featureSet, identity_sha256: "a".repeat(64), manifest_sha256: "4".repeat(64), output_sha256: "5".repeat(64), output_fingerprint: "6".repeat(64), row_count: 48, target },
          bindings: [
            { field: "fast", source: "FEATURE", column: "fast__value", status_column: "fast__status" },
            { field: "slow", source: "FEATURE", column: "slow__value", status_column: "slow__status" },
          ],
          authority: "ADVISORY_ONLY",
        },
        created_at_utc: "2026-10-21T00:00:01.000000Z",
        output_file: "inference.parquet",
        output_sha256: "7".repeat(64),
        output_fingerprint: "8".repeat(64),
        row_count: 48,
        columns: [],
        outputs: [{ name: "spread", kind: { kind: "FLOAT64", minimum: -10, maximum: 10 } }],
        summary: { ok_rows: 41, skipped_rows: 7, invalid_output_rows: 0, reasons: { WARMUP: 7 }, first_decision_utc: "2024-01-01T01:00:00.000000Z", last_decision_utc: "2024-01-03T00:00:00.000000Z" },
        run: { invoked_rows: 41, total_latency_us: 410, max_latency_us: 30 },
        lineage: { model_id: modelId, model_name: "spread.onnx", adapter: "ONNX_NATIVE", content_digest: "9".repeat(64), input_kind: "FEATURE_SET", input_id: featureSet, feature_set_id: featureSet, target, datasets: [] },
        authority: "ADVISORY_ONLY",
        status: "READY",
        research_notice: registry.inferenceNotice,
      },
      sample_rows: [
        { open_time_utc: "2024-01-01T00:00:00.000000Z", decision_time_utc: "2024-01-01T01:00:00.000000Z", row_status: "SKIPPED", reason_code: "WARMUP", inputs: [{ field: "fast", value: 1.1, label: null }, { field: "slow", value: null, label: null }], outputs: [{ field: "spread", value: null, label: null }] },
        { open_time_utc: "2024-01-02T23:00:00.000000Z", decision_time_utc: "2024-01-03T00:00:00.000000Z", row_status: "OK", reason_code: null, inputs: [{ field: "fast", value: 1.1023456, label: null }, { field: "slow", value: 1.0987654, label: null }], outputs: [{ field: "spread", value: 0.0035802, label: null }] },
      ],
    },
  };
}

function recordEntry(id = recordId, status: "READY" | "QUARANTINED" = "READY"): InferenceRecordCatalogEntry {
  const manifest = { ...previewSession().preview.manifest, record_id: id };
  return {
    record_id: id,
    model_id: modelId,
    input_id: featureSet,
    status,
    determinism: "NOT_REPLAYED",
    replays: [],
    manifest,
    integrity: { manifest_valid: true, output_valid: status === "READY", lineage_valid: true },
    diagnostic: status === "READY" ? null : `inference record ${id} manifest or output digest does not match the immutable registry`,
  };
}

interface Backend {
  models?: () => ModelCatalogEntry[];
  inputs?: InferenceInputCandidate[];
  records?: () => InferenceRecordCatalogEntry[];
  staged?: () => ModelCandidate[];
  discover?: () => OllamaDiscovery;
  select?: () => ModelCandidate | null;
  previewRegistration?: (request: ModelRegistrationRequest) => RegistrationPreview;
  register?: (args: Record<string, unknown>) => unknown;
  active?: () => ActiveInferenceJob | null;
  preview?: (request: InferenceRequest) => InferencePreviewSession;
  confirm?: () => InferenceCommitResult;
  cancel?: () => string;
  compare?: (ids: string[]) => ComparisonReport;
  inspect?: () => InferenceRecordCatalogEntry | null;
  calls?: string[];
}

function mockBackend(backend: Backend) {
  mockIPC((command, payload) => {
    backend.calls?.push(command);
    const args = (payload ?? {}) as Record<string, unknown>;
    switch (command) {
      case "get_model_lab_registry": return registry;
      case "get_ollama_endpoint": return "http://127.0.0.1:11434";
      case "set_ollama_endpoint": return args.endpoint;
      case "discover_ollama_models": return backend.discover?.();
      case "select_onnx_model": return backend.select?.() ?? null;
      case "list_onnx_candidates": return backend.staged?.() ?? [];
      case "preview_model_registration": return backend.previewRegistration?.(args.request as ModelRegistrationRequest);
      case "register_model": return backend.register?.(args);
      case "list_models": return backend.models?.() ?? [];
      case "list_inference_inputs": return backend.inputs ?? inputs;
      case "list_inference_records": return backend.records?.() ?? [];
      case "list_model_lab_attempts": return [];
      case "inspect_inference_record": return backend.inspect?.() ?? null;
      case "get_active_inference_job": return backend.active?.() ?? null;
      case "preview_inference": return backend.preview?.(args.request as InferenceRequest);
      case "confirm_inference": return backend.confirm?.();
      case "cancel_inference_job": return backend.cancel?.() ?? "CANCELLED";
      case "compare_inference_records": return backend.compare?.((args.request as { record_ids: string[] }).record_ids);
      default: return undefined;
    }
  });
}

async function loaded() {
  await screen.findByText(/They are not evidence of edge/);
}

describe("gate desktop_model_lab_ipc", () => {
  beforeAll(() => {
    Object.defineProperty(window, "crypto", {
      configurable: true,
      value: { getRandomValues: (buffer: Uint8Array) => randomFillSync(buffer) },
    });
  });

  it("reports an unavailable runtime truthfully and shows no candidates or model output", async () => {
    const calls: string[] = [];
    mockBackend({
      calls,
      discover: () => ({ endpoint: "http://127.0.0.1:11434", availability: "UNAVAILABLE", diagnostic: "model runtime at http://127.0.0.1:11434 is unavailable: connection refused", candidates: [] }),
    });
    render(<ModelLab />);
    await loaded();
    expect(screen.getByText(/No model candidates/)).toBeTruthy();
    expect(screen.getByText("Register a model that verifies as READY first.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Save endpoint and discover" }));
    expect(await screen.findByText(/is unavailable: .*No model output is produced without a runtime/)).toBeTruthy();
    expect(screen.getByText(/No model candidates/)).toBeTruthy();
    expect(calls).toContain("set_ollama_endpoint");
    expect(calls).not.toContain("register_model");
    expect(calls).not.toContain("preview_inference");
  });

  it("registers a selected ONNX candidate only after its identity is previewed and confirmed", async () => {
    let registered = false;
    const requests: ModelRegistrationRequest[] = [];
    const confirmations: unknown[] = [];
    mockBackend({
      select: () => onnxCandidate,
      staged: () => [onnxCandidate],
      previewRegistration: (request) => { requests.push(request); return { model_id: modelId, manifest: modelManifest(modelId, "spread.onnx"), already_registered: false }; },
      register: (args) => { confirmations.push(args.confirmedModelId); registered = true; return { model_id: modelId, disposition: "REGISTERED" }; },
      models: () => registered ? [modelEntry()] : [],
    });
    render(<ModelLab />);
    await loaded();
    fireEvent.click(screen.getByRole("button", { name: "Select ONNX file" }));
    expect(await screen.findByRole("heading", { name: "Register spread.onnx" })).toBeTruthy();
    fireEvent.change(screen.getByRole("textbox", { name: /Input fields/ }), { target: { value: "fast, slow" } });
    fireEvent.change(screen.getByRole("textbox", { name: "Output 1 name" }), { target: { value: "spread" } });
    fireEvent.change(screen.getByRole("spinbutton", { name: "Output 1 minimum" }), { target: { value: "-10" } });
    fireEvent.change(screen.getByRole("spinbutton", { name: "Output 1 maximum" }), { target: { value: "10" } });
    expect(screen.queryByRole("button", { name: "Confirm registration" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Preview model identity" }));
    expect(await screen.findByRole("button", { name: "Confirm registration" })).toBeTruthy();
    expect(requests).toEqual([{
      candidate_id: onnxCandidate.candidate_id,
      input_fields: ["fast", "slow"],
      outputs: [{ name: "spread", kind: { kind: "FLOAT64", minimum: -10, maximum: 10 } }],
      runtime: { adapter: "ONNX_NATIVE" },
      authority: "ADVISORY_ONLY",
    }]);
    expect(registered).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "Confirm registration" }));
    expect(await screen.findByText(`Model registered: ${modelId}.`)).toBeTruthy();
    expect(confirmations).toEqual([modelId]);
    const catalogue = screen.getByRole("table", { name: "Registered models" });
    expect(within(catalogue).getByText("READY")).toBeTruthy();
  });

  it("previews inference with skipped rows, registers only after confirmation, and shows lineage", async () => {
    let committed = false;
    const requests: InferenceRequest[] = [];
    mockBackend({
      models: () => [modelEntry()],
      preview: (request) => { requests.push(request); return previewSession(); },
      confirm: () => { committed = true; return { record_id: recordId, disposition: "MATERIALIZED", determinism: "NOT_REPLAYED" }; },
      records: () => committed ? [recordEntry()] : [],
    });
    render(<ModelLab />);
    await loaded();
    const input = screen.getByRole("combobox", { name: "Input" }) as HTMLSelectElement;
    expect(Array.from(input.options).find((option) => option.value === blockedSet)?.disabled).toBe(true);
    expect(screen.getByText(/failed snapshot verification/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview inference" }));
    expect(await screen.findByText(/INFERENCE PREVIEW · NOT YET REGISTERED · ADVISORY_ONLY/)).toBeTruthy();
    expect(requests).toEqual([{
      model_id: modelId,
      input: { kind: "FEATURE_SET", feature_set_id: featureSet },
      bindings: { fast: { source: "FEATURE", feature_key: "fast", output: "value" }, slow: { source: "FEATURE", feature_key: "slow", output: "value" } },
      authority: "ADVISORY_ONLY",
      quotas: { max_rows: 1000000, max_output_bytes: 1024 * 1024 * 1024, request_timeout_ms: 30000 },
    }]);
    expect(screen.getByText(/Reasons: WARMUP 7/)).toBeTruthy();
    const sample = screen.getByRole("table", { name: /Most recent 2 rows/ });
    expect(within(sample).getByText("SKIPPED")).toBeTruthy();
    expect(screen.getByText("No inference records registered yet.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Freeze and register inference record" }));
    expect(await screen.findByText(`Inference record frozen and registered: ${recordId}.`)).toBeTruthy();
    expect(await screen.findByText("INFERENCE RECORD INSPECTION")).toBeTruthy();
    expect(screen.getByText("Input and model lineage: verified")).toBeTruthy();
    expect(screen.queryByText(/INFERENCE PREVIEW/)).toBeNull();
  });

  it("reports a non-deterministic replay without claiming the record changed", async () => {
    mockBackend({
      models: () => [modelEntry()],
      preview: () => previewSession({ record_id: recordId, output_matches: false }),
      confirm: () => ({ record_id: recordId, disposition: "REPLAY_NON_DETERMINISTIC", determinism: "NON_DETERMINISTIC" }),
      records: () => [{ ...recordEntry(), determinism: "NON_DETERMINISTIC" }],
    });
    render(<ModelLab />);
    await loaded();
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview inference" }));
    expect(await screen.findByText(/confirming records the run as NON_DETERMINISTIC and keeps the registered record/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Freeze and register inference record" }));
    expect(await screen.findByText(/recorded as NON_DETERMINISTIC; the registered record .* was kept unchanged/)).toBeTruthy();
  });

  it("drops a consumed preview when confirmation fails so it cannot be confirmed twice", async () => {
    mockBackend({
      models: () => [modelEntry()],
      preview: () => previewSession(),
      confirm: () => { throw new Error(`input ${featureSet} failed verification: input changed or lost READY verification after preview`); },
    });
    render(<ModelLab />);
    await loaded();
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview inference" }));
    fireEvent.click(await screen.findByRole("button", { name: "Freeze and register inference record" }));
    expect(await screen.findByText(/The inference record was not registered: input .* failed verification/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Freeze and register inference record" })).toBeNull();
    expect(screen.getByRole("button", { name: "Validate and preview inference" })).toBeTruthy();
  });

  it("restores a running inference with progress and requests cancellation", async () => {
    let running = true;
    const calls: string[] = [];
    mockBackend({
      calls,
      models: () => [modelEntry()],
      active: () => running ? { state: "RUNNING", jobId, progress: { phase: "INFERRING", processed_rows: 12, total_rows: 48 } } : null,
      cancel: () => { running = false; return "CANCELLATION_REQUESTED"; },
    });
    render(<ModelLab />);
    expect(await screen.findByText(/Running the model · 12 \/ 48 rows/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Cancel inference" }));
    expect(await screen.findByText(/the runtime itself is not stopped/)).toBeTruthy();
    expect(calls).toContain("cancel_inference_job");
    expect(await screen.findByText(/ended before a preview was prepared/)).toBeTruthy();
  });

  it("compares selected READY records descriptively and never offers quarantined ones", async () => {
    const compared: string[][] = [];
    mockBackend({
      models: () => [modelEntry(), modelEntry(otherModelId, "shifted.onnx")],
      records: () => [recordEntry(), recordEntry(otherRecordId), recordEntry(`ir_${"3".repeat(64)}`, "QUARANTINED")],
      inspect: () => recordEntry(`ir_${"3".repeat(64)}`, "QUARANTINED"),
      compare: (ids) => {
        compared.push(ids);
        return {
          input_kind: "FEATURE_SET",
          input_id: featureSet,
          input_output_sha256: "5".repeat(64),
          rows: 48,
          records: ids.map((id) => ({ record_id: id, model_id: modelId, model_name: "spread.onnx", rows: 48, ok_rows: 41, skipped_rows: 7, invalid_output_rows: 0, coverage: 41 / 48, invalid_output_rate: 0, mean_latency_us: 10, max_latency_us: 30 })),
          pairs: [{ left_record_id: ids[0], right_record_id: ids[1], fields: [{ field: "spread", kind: "FLOAT64", rows_both_ok: 41, agreeing_rows: null, agreement_rate: null, mean_absolute_difference: 0.25, max_absolute_difference: 0.25 }] }],
          notice: registry.comparisonNotice,
        };
      },
    });
    render(<ModelLab />);
    await loaded();
    const table = screen.getByRole("table", { name: "Inference records" });
    const boxes = within(table).getAllByRole("checkbox") as HTMLInputElement[];
    expect(boxes[2].disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Compare selected records" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(boxes[0]);
    fireEvent.click(boxes[1]);
    fireEvent.click(screen.getByRole("button", { name: "Compare selected records" }));
    expect(await screen.findByText(/DESCRIPTIVE COMPARISON · 48 ROWS/)).toBeTruthy();
    expect(compared).toEqual([[recordId, otherRecordId]]);
    expect(screen.getAllByText("85.4%")).toHaveLength(2);
    expect(screen.getAllByText("0.2500000")).toHaveLength(2);
    expect(screen.getByText(/not accuracy against outcomes/)).toBeTruthy();
    expect(screen.queryByText(/PnL:|Win rate|Profit factor|Sharpe/)).toBeNull();
    fireEvent.click(within(table).getAllByRole("button", { name: /Inspect/ })[2]);
    expect(await screen.findByText("Output SHA-256: mismatch")).toBeTruthy();
  });

  it("rejects unbound inputs and quarantined models before calling the backend", async () => {
    const calls: string[] = [];
    mockBackend({ calls, models: () => [modelEntry(), modelEntry(otherModelId, "tampered.onnx", "QUARANTINED")], preview: () => previewSession() });
    render(<ModelLab />);
    await loaded();
    const model = screen.getByRole("combobox", { name: "Model" }) as HTMLSelectElement;
    expect(Array.from(model.options).find((option) => option.value === otherModelId)?.disabled).toBe(true);
    expect(screen.getByText(/QUARANTINED: model .* no longer matches its registered SHA-256/)).toBeTruthy();
    fireEvent.change(screen.getByRole("combobox", { name: /^slow/ }), { target: { value: "" } });
    fireEvent.click(screen.getByRole("button", { name: "Validate and preview inference" }));
    expect(await screen.findByText(/Bind the model input slow to a column of the selected input/)).toBeTruthy();
    expect(calls).not.toContain("preview_inference");
  });
});
