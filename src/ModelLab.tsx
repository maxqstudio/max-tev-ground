import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AlertTriangle, BrainCircuit, Check, ChevronRight, CircleHelp, Clock3, FileCheck2, FolderOpen, GitBranch, RefreshCw, Search, ShieldCheck, X } from "lucide-react";
import type {
  ActiveInferenceJob,
  ComparisonReport,
  InferenceCommitResult,
  InferenceInputCandidate,
  InferencePreview,
  InferencePreviewSession,
  InferenceProgressSnapshot,
  InferenceRecordCatalogEntry,
  InferenceRequest,
  InputColumn,
  ModelCandidate,
  ModelCatalogEntry,
  ModelLabAttempt,
  ModelLabRegistrySnapshot,
  ModelRegistrationRequest,
  ModelStatus,
  OllamaDiscovery,
  OutputField,
  RegistrationPreview,
  RegistrationResult,
} from "./model-lab-types";

interface OutputDraft {
  name: string;
  kind: "FLOAT64" | "CATEGORY";
  minimum: string;
  maximum: string;
  labels: string;
}

interface RegistrationDraft {
  inputFields: string;
  outputs: OutputDraft[];
  seed: string;
  maxOutputTokens: string;
  contextTokens: string;
  instruction: string;
}

interface Bounds {
  maxRows: string;
  maxOutputMegabytes: string;
  requestTimeoutMs: string;
}

const defaultBounds: Bounds = { maxRows: "1000000", maxOutputMegabytes: "1024", requestTimeoutMs: "30000" };

function message(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason);
}

function newJobId(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function shortHash(value: string): string {
  return value.length > 24 ? `${value.slice(0, 12)}…${value.slice(-8)}` : value;
}

function formatValue(value: number | null): string {
  if (value === null) return "—";
  return Math.abs(value) >= 1000 ? value.toFixed(2) : value.toPrecision(7);
}

function statusClass(status: ModelStatus | "READY" | "QUARANTINED"): string {
  return status === "RUNTIME_UNAVAILABLE" ? "status-in-progress" : `status-${status.toLowerCase()}`;
}

function bindingKey(binding: InputColumn): string {
  return JSON.stringify(binding);
}

function defaultDraft(candidate: ModelCandidate): RegistrationDraft {
  const width = candidate.onnx?.input.width ?? 2;
  const outputs = candidate.onnx ? Math.max(1, candidate.onnx.outputs.reduce((sum, output) => sum + output.width, 0)) : 1;
  return {
    inputFields: Array.from({ length: width }, (_, index) => `input_${index + 1}`).join(", "),
    outputs: Array.from({ length: outputs }, (_, index): OutputDraft => candidate.adapter === "ONNX_NATIVE"
      ? { name: `output_${index + 1}`, kind: "FLOAT64", minimum: "-1000000", maximum: "1000000", labels: "" }
      : { name: "direction", kind: "CATEGORY", minimum: "-1", maximum: "1", labels: "UP, DOWN, FLAT" }),
    seed: "42",
    maxOutputTokens: "64",
    contextTokens: "2048",
    instruction: "Classify the research inputs. Answer only with the required JSON fields.",
  };
}

function buildRegistration(candidate: ModelCandidate | undefined, draft: RegistrationDraft | null): ModelRegistrationRequest {
  if (!candidate || !draft) throw new Error("Select an eligible model candidate.");
  const inputFields = draft.inputFields.split(",").map((field) => field.trim()).filter(Boolean);
  if (inputFields.length === 0) throw new Error("Declare at least one input field.");
  const outputs: OutputField[] = draft.outputs.map((output) => {
    const name = output.name.trim();
    if (!name) throw new Error("Every output needs a name.");
    if (output.kind === "CATEGORY") {
      const labels = output.labels.split(",").map((label) => label.trim()).filter(Boolean);
      if (labels.length < 2) throw new Error(`Output ${name} needs at least two labels.`);
      return { name, kind: { kind: "CATEGORY", labels } };
    }
    const minimum = Number(output.minimum);
    const maximum = Number(output.maximum);
    if (output.minimum.trim() === "" || output.maximum.trim() === "" || !Number.isFinite(minimum) || !Number.isFinite(maximum) || minimum >= maximum) {
      throw new Error(`Output ${name} needs finite bounds with the minimum below the maximum.`);
    }
    return { name, kind: { kind: "FLOAT64", minimum, maximum } };
  });
  const whole = (value: string, label: string) => {
    const parsed = Number(value);
    if (!Number.isSafeInteger(parsed) || parsed < 0) throw new Error(`${label} must be a whole number.`);
    return parsed;
  };
  return {
    candidate_id: candidate.candidate_id,
    input_fields: inputFields,
    outputs,
    runtime: candidate.adapter === "ONNX_NATIVE"
      ? { adapter: "ONNX_NATIVE" }
      : {
        adapter: "OLLAMA_LOOPBACK",
        seed: whole(draft.seed, "Seed"),
        max_output_tokens: whole(draft.maxOutputTokens, "Max output tokens"),
        context_tokens: whole(draft.contextTokens, "Context tokens"),
        instruction: draft.instruction,
      },
    authority: "ADVISORY_ONLY",
  };
}

function buildInference(model: ModelCatalogEntry | undefined, input: InferenceInputCandidate | undefined, bindings: Record<string, string>, bounds: Bounds): InferenceRequest {
  if (!model?.manifest || model.status !== "READY") throw new Error("Select a READY registered model.");
  if (!input?.eligible) throw new Error("Select a READY feature set or decision set.");
  const requestBindings: Record<string, InputColumn> = {};
  for (const field of model.manifest.identity.input_fields) {
    const chosen = input.columns.find((column) => bindingKey(column.binding) === bindings[field]);
    if (!chosen) throw new Error(`Bind the model input ${field} to a column of the selected input.`);
    requestBindings[field] = chosen.binding;
  }
  const maxRows = Number(bounds.maxRows);
  const maxMegabytes = Number(bounds.maxOutputMegabytes);
  const timeout = Number(bounds.requestTimeoutMs);
  if (!Number.isSafeInteger(maxRows) || maxRows <= 0) throw new Error("Max inference rows must be a positive whole number.");
  if (!Number.isSafeInteger(maxMegabytes) || maxMegabytes <= 0) throw new Error("Max output size must be a positive whole number.");
  if (!Number.isSafeInteger(timeout) || timeout <= 0) throw new Error("Request timeout must be a positive whole number of milliseconds.");
  return {
    model_id: model.model_id,
    input: input.kind === "FEATURE_SET" ? { kind: "FEATURE_SET", feature_set_id: input.id } : { kind: "DECISION_SET", decision_set_id: input.id },
    bindings: requestBindings,
    authority: "ADVISORY_ONLY",
    quotas: { max_rows: maxRows, max_output_bytes: maxMegabytes * 1024 * 1024, request_timeout_ms: timeout },
  };
}

const phaseLabel: Record<InferenceProgressSnapshot["phase"], string> = {
  QUEUED: "Queued",
  VERIFYING_MODEL: "Re-verifying the model",
  VERIFYING_INPUT: "Re-verifying the input",
  INFERRING: "Running the model",
  FINALIZING: "Writing and hashing outputs",
};

export default function ModelLab() {
  const [registry, setRegistry] = useState<ModelLabRegistrySnapshot | null>(null);
  const [endpoint, setEndpoint] = useState("");
  const [discovery, setDiscovery] = useState<OllamaDiscovery | null>(null);
  const [onnxCandidates, setOnnxCandidates] = useState<ModelCandidate[]>([]);
  const [candidateId, setCandidateId] = useState("");
  const [draft, setDraft] = useState<RegistrationDraft | null>(null);
  const [registration, setRegistration] = useState<RegistrationPreview | null>(null);
  const [models, setModels] = useState<ModelCatalogEntry[]>([]);
  const [inputs, setInputs] = useState<InferenceInputCandidate[]>([]);
  const [records, setRecords] = useState<InferenceRecordCatalogEntry[]>([]);
  const [attempts, setAttempts] = useState<ModelLabAttempt[]>([]);
  const [modelId, setModelId] = useState("");
  const [inputId, setInputId] = useState("");
  const [bindings, setBindings] = useState<Record<string, string>>({});
  const [bounds, setBounds] = useState<Bounds>(defaultBounds);
  const [preview, setPreview] = useState<InferencePreviewSession | null>(null);
  const [activeJobId, setActiveJobId] = useState<string | null>(null);
  const [progress, setProgress] = useState<InferenceProgressSnapshot | null>(null);
  const [previewPending, setPreviewPending] = useState(false);
  const [busy, setBusy] = useState<"loading" | "discovering" | "selecting" | "registering" | "previewing" | "confirming" | "cancelling" | "comparing" | null>("loading");
  const [selectedRecord, setSelectedRecord] = useState<InferenceRecordCatalogEntry | null>(null);
  const [compareIds, setCompareIds] = useState<string[]>([]);
  const [comparison, setComparison] = useState<ComparisonReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const awaitingPreview = useRef(false);

  const candidates = [...(discovery?.candidates ?? []), ...onnxCandidates];
  const candidate = candidates.find((item) => item.candidate_id === candidateId);
  const readyModels = models.filter((item) => item.status === "READY" && item.manifest);
  const model = models.find((item) => item.model_id === modelId);
  const input = inputs.find((item) => item.id === inputId);
  const eligibleInputs = inputs.filter((item) => item.eligible);
  const locked = busy !== null || preview !== null || previewPending;

  const defaultBindings = useCallback((nextModel: ModelCatalogEntry | undefined, nextInput: InferenceInputCandidate | undefined) => {
    const fields = nextModel?.manifest?.identity.input_fields ?? [];
    const columns = nextInput?.columns ?? [];
    return Object.fromEntries(fields.map((field, index) => {
      const named = columns.find((column) => column.binding.source === "FEATURE" ? column.binding.feature_key === field : column.binding.source === "DECISION_INPUT" ? column.binding.role === field : field === "decision");
      return [field, bindingKey((named ?? columns[Math.min(index, columns.length - 1)])?.binding ?? { source: "DECISION" }) ];
    }));
  }, []);

  const refreshCatalog = useCallback(async () => {
    const [nextModels, nextRecords, history] = await Promise.all([
      invoke<ModelCatalogEntry[]>("list_models"),
      invoke<InferenceRecordCatalogEntry[]>("list_inference_records"),
      invoke<ModelLabAttempt[]>("list_model_lab_attempts"),
    ]);
    setModels(nextModels);
    setRecords(nextRecords);
    setAttempts(history);
    return { nextModels, nextRecords };
  }, []);

  useEffect(() => {
    let current = true;
    Promise.all([
      invoke<ModelLabRegistrySnapshot>("get_model_lab_registry"),
      invoke<string>("get_ollama_endpoint"),
      invoke<ModelCandidate[]>("list_onnx_candidates"),
      invoke<ModelCatalogEntry[]>("list_models"),
      invoke<InferenceInputCandidate[]>("list_inference_inputs"),
      invoke<InferenceRecordCatalogEntry[]>("list_inference_records"),
      invoke<ModelLabAttempt[]>("list_model_lab_attempts"),
      invoke<ActiveInferenceJob | null>("get_active_inference_job"),
    ]).then(([nextRegistry, nextEndpoint, staged, nextModels, nextInputs, nextRecords, history, active]) => {
      if (!current) return;
      setRegistry(nextRegistry);
      setEndpoint(nextEndpoint);
      setOnnxCandidates(staged);
      setModels(nextModels);
      setInputs(nextInputs);
      setRecords(nextRecords);
      setAttempts(history);
      const firstModel = nextModels.find((item) => item.status === "READY" && item.manifest);
      const firstInput = nextInputs.find((item) => item.eligible);
      setModelId(firstModel?.model_id ?? "");
      setInputId(firstInput?.id ?? "");
      setBindings(defaultBindings(firstModel, firstInput));
      if (active?.state === "PREPARED") {
        setPreview(active.session);
        setActiveJobId(active.session.jobId);
        setNotice("The validated inference preview was restored. Confirm or cancel it before starting another.");
      } else if (active?.state === "RUNNING") {
        setActiveJobId(active.jobId);
        setProgress(active.progress);
        setPreviewPending(true);
      }
      setBusy(active?.state === "RUNNING" ? "previewing" : null);
    }).catch((reason: unknown) => {
      if (!current) return;
      setError(message(reason));
      setBusy(null);
    });
    return () => { current = false; };
  }, [defaultBindings]);

  useEffect(() => {
    if (!previewPending || !activeJobId) return;
    let current = true;
    let timer: number | undefined;
    async function poll() {
      try {
        const active = await invoke<ActiveInferenceJob | null>("get_active_inference_job");
        if (!current) return;
        if (active?.state === "RUNNING") {
          setProgress(active.progress);
          timer = window.setTimeout(poll, 300);
          return;
        }
        if (awaitingPreview.current) return;
        setPreviewPending(false);
        setProgress(null);
        setBusy(null);
        if (active?.state === "PREPARED") {
          setPreview(active.session);
          setActiveJobId(active.session.jobId);
        } else {
          setActiveJobId(null);
          setNotice("The inference job ended before a preview was prepared. Check the attempt history, then retry.");
          refreshCatalog().catch(() => undefined);
        }
      } catch (reason) {
        if (!current) return;
        setError(message(reason));
        timer = window.setTimeout(poll, 1_000);
      }
    }
    timer = window.setTimeout(poll, 300);
    return () => {
      current = false;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [activeJobId, previewPending, refreshCatalog]);

  async function saveEndpointAndDiscover(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setError(null);
    setNotice(null);
    setBusy("discovering");
    try {
      const saved = await invoke<string>("set_ollama_endpoint", { endpoint });
      setEndpoint(saved);
      const found = await invoke<OllamaDiscovery>("discover_ollama_models");
      setDiscovery(found);
      if (found.availability !== "AVAILABLE") setNotice(`Ollama runtime at ${found.endpoint} is ${found.availability.toLowerCase()}: ${found.diagnostic ?? "no diagnostic"}. No model output is produced without a runtime.`);
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }

  async function selectOnnx() {
    setError(null);
    setNotice(null);
    setBusy("selecting");
    try {
      const staged = await invoke<ModelCandidate | null>("select_onnx_model");
      if (staged) {
        setOnnxCandidates(await invoke<ModelCandidate[]>("list_onnx_candidates"));
        if (staged.eligible) chooseCandidate(staged);
        else setNotice(`${staged.model_name} cannot be registered: ${staged.reason ?? "unsupported model"}.`);
      }
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }

  function chooseCandidate(next: ModelCandidate) {
    setCandidateId(next.candidate_id);
    setDraft(defaultDraft(next));
    setRegistration(null);
  }

  async function previewRegistration() {
    setError(null);
    setNotice(null);
    let request: ModelRegistrationRequest;
    try {
      request = buildRegistration(candidate, draft);
    } catch (reason) {
      setError(message(reason));
      return;
    }
    setBusy("registering");
    try {
      setRegistration(await invoke<RegistrationPreview>("preview_model_registration", { request }));
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }

  async function confirmRegistration() {
    if (!registration) return;
    setError(null);
    let request: ModelRegistrationRequest;
    try {
      request = buildRegistration(candidate, draft);
    } catch (reason) {
      setError(message(reason));
      return;
    }
    setBusy("registering");
    let result: RegistrationResult;
    try {
      result = await invoke<RegistrationResult>("register_model", { request, confirmedModelId: registration.model_id });
    } catch (reason) {
      setRegistration(null);
      setBusy(null);
      setError(`The model was not registered: ${message(reason)}`);
      return;
    }
    setRegistration(null);
    setBusy(null);
    setNotice(`${result.disposition === "ALREADY_REGISTERED" ? "Model was already registered" : "Model registered"}: ${result.model_id}.`);
    try {
      const { nextModels } = await refreshCatalog();
      const registered = nextModels.find((item) => item.model_id === result.model_id);
      if (registered?.status === "READY") {
        setModelId(registered.model_id);
        setBindings(defaultBindings(registered, input));
      }
    } catch (reason) {
      setError(`The model was registered, but the catalogue did not refresh: ${message(reason)}. Use Refresh to retry.`);
    }
  }

  async function discardCandidate(id: string) {
    setError(null);
    try {
      await invoke("discard_onnx_candidate", { candidateId: id });
      setOnnxCandidates(await invoke<ModelCandidate[]>("list_onnx_candidates"));
      if (candidateId === id) {
        setCandidateId("");
        setDraft(null);
        setRegistration(null);
      }
    } catch (reason) {
      setError(message(reason));
    }
  }

  function changeModel(id: string) {
    setModelId(id);
    setBindings(defaultBindings(models.find((item) => item.model_id === id), input));
  }

  function changeInput(id: string) {
    setInputId(id);
    setBindings(defaultBindings(model, inputs.find((item) => item.id === id)));
  }

  async function createPreview(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setError(null);
    setNotice(null);
    let request: InferenceRequest;
    try {
      request = buildInference(model, input, bindings, bounds);
    } catch (reason) {
      setError(message(reason));
      return;
    }
    const jobId = newJobId();
    setActiveJobId(jobId);
    setProgress(null);
    setPreviewPending(true);
    setBusy("previewing");
    awaitingPreview.current = true;
    try {
      setPreview(await invoke<InferencePreviewSession>("preview_inference", { jobId, request }));
    } catch (reason) {
      setActiveJobId(null);
      const text = message(reason);
      if (text.toLowerCase().includes("cancel")) setNotice("Inference cancelled. No inference record was registered.");
      else setError(text);
      try { await refreshCatalog(); } catch { /* keep the preview diagnostic */ }
    } finally {
      awaitingPreview.current = false;
      setPreviewPending(false);
      setProgress(null);
      setBusy(null);
    }
  }

  async function cancelJob() {
    if (!activeJobId) return;
    setError(null);
    setBusy("cancelling");
    let result: string;
    try {
      result = await invoke<string>("cancel_inference_job", { jobId: activeJobId });
    } catch (reason) {
      setError(message(reason));
      setBusy(previewPending ? "previewing" : null);
      return;
    }
    if (result === "CANCELLATION_REQUESTED") {
      setNotice("Cancellation requested. Inference stops before the next row; the runtime itself is not stopped, and nothing is registered.");
      return;
    }
    setPreview(null);
    setActiveJobId(null);
    setBusy(null);
    setNotice("Preview cancelled. No inference record was registered.");
    try {
      await refreshCatalog();
    } catch (reason) {
      setError(`The preview was cancelled, but the record history did not refresh: ${message(reason)}. Use Refresh to retry.`);
    }
  }

  async function confirmPreview() {
    if (!preview) return;
    setError(null);
    setBusy("confirming");
    let result: InferenceCommitResult;
    try {
      result = await invoke<InferenceCommitResult>("confirm_inference", { jobId: preview.jobId });
    } catch (reason) {
      // The backend consumes the prepared preview before committing, so it cannot be retried.
      setPreview(null);
      setActiveJobId(null);
      setBusy(null);
      setError(`The inference record was not registered: ${message(reason)}`);
      try { await refreshCatalog(); } catch { /* keep the commit diagnostic */ }
      return;
    }
    setPreview(null);
    setActiveJobId(null);
    setBusy(null);
    setNotice(result.disposition === "MATERIALIZED"
      ? `Inference record frozen and registered: ${result.record_id}.`
      : result.disposition === "REPLAY_REPRODUCED"
        ? `Replay reproduced the registered output digest: ${result.record_id} · ${result.determinism}.`
        : `Replay produced a different output digest. It was recorded as NON_DETERMINISTIC; the registered record ${result.record_id} was kept unchanged.`);
    try {
      const { nextRecords } = await refreshCatalog();
      setSelectedRecord(nextRecords.find((item) => item.record_id === result.record_id) ?? null);
    } catch (reason) {
      setError(`The inference was committed, but the catalogue did not refresh: ${message(reason)}. Use Refresh to retry.`);
    }
  }

  async function refreshAll() {
    setError(null);
    setBusy("loading");
    try {
      const [nextInputs, staged] = await Promise.all([
        invoke<InferenceInputCandidate[]>("list_inference_inputs"),
        invoke<ModelCandidate[]>("list_onnx_candidates"),
        refreshCatalog(),
      ]);
      setInputs(nextInputs);
      setOnnxCandidates(staged);
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }

  async function inspectRecord(recordId: string) {
    setError(null);
    try {
      const entry = await invoke<InferenceRecordCatalogEntry | null>("inspect_inference_record", { recordId });
      if (!entry) throw new Error("Inference record no longer exists in the registry.");
      setSelectedRecord(entry);
    } catch (reason) {
      setError(message(reason));
    }
  }

  async function compareSelected() {
    setError(null);
    setBusy("comparing");
    try {
      setComparison(await invoke<ComparisonReport>("compare_inference_records", { request: { record_ids: compareIds } }));
    } catch (reason) {
      setComparison(null);
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }

  function toggleCompare(recordId: string) {
    setComparison(null);
    setCompareIds(compareIds.includes(recordId) ? compareIds.filter((id) => id !== recordId) : [...compareIds, recordId]);
  }

  const unavailableInputs = inputs.filter((item) => !item.eligible).map((item) => <p className="form-hint" key={item.id}><AlertTriangle size={13} /> <code title={item.id}>{shortHash(item.id)}</code>: {item.reason}</p>);
  const unusableModels = models.filter((item) => item.status !== "READY").map((item) => <p className="form-hint" key={item.model_id}><AlertTriangle size={13} /> <code title={item.model_id}>{shortHash(item.model_id)}</code> {item.status}: {item.diagnostic}</p>);

  return (
    <div className="page-content data-ground-page feature-studio-page">
      <div className="page-heading data-ground-heading">
        <div>
          <div className="eyebrow">RG-R04 · MODEL LAB</div>
          <h1>Model Lab</h1>
          <p className="page-intro">Register local Ollama and ONNX models by immutable identity and record their ADVISORY_ONLY outputs over verified feature and decision sets.</p>
        </div>
        <div className="data-ground-heading-actions">
          <button className="button button-quiet" type="button" onClick={refreshAll} disabled={busy !== null}><RefreshCw size={15} aria-hidden="true" />Refresh</button>
        </div>
      </div>

      {error && <div className="data-alert data-alert-error" role="alert"><AlertTriangle size={16} /><span>{error}</span><button type="button" aria-label="Dismiss error" onClick={() => setError(null)}><X size={15} /></button></div>}
      {notice && <div className="data-alert data-alert-notice" role="status"><Check size={16} /><span>{notice}</span><button type="button" aria-label="Dismiss notice" onClick={() => setNotice(null)}><X size={15} /></button></div>}
      {registry && <p className="feature-notice"><CircleHelp size={13} aria-hidden="true" /> {registry.inferenceNotice}</p>}

      <section className="data-section" aria-labelledby="model-discovery-title">
        <div className="data-section-heading">
          <div className="step-badge">01</div>
          <div><h2 id="model-discovery-title">Local runtimes and discovery</h2><p>Discovery lists models as unregistered candidates. Nothing is pulled, downloaded, registered, or run until you confirm.</p></div>
        </div>
        <form onSubmit={saveEndpointAndDiscover} noValidate>
          <fieldset className="form-fieldset" disabled={busy !== null}>
            <div className="form-grid">
              <label className="form-field"><span>Ollama endpoint <small>loopback IP and port only</small></span>
                <input type="text" value={endpoint} placeholder={registry?.defaultOllamaEndpoint} onChange={(event) => setEndpoint(event.target.value)} />
              </label>
            </div>
            <div className="form-actions">
              <button className="button button-quiet" type="submit"><Search size={15} />{busy === "discovering" ? "Asking the runtime…" : "Save endpoint and discover"}</button>
              <button className="button button-quiet" type="button" onClick={selectOnnx}><FolderOpen size={15} />{busy === "selecting" ? "Copying the model…" : "Select ONNX file"}</button>
            </div>
          </fieldset>
        </form>
        {discovery && <p className="form-hint">Ollama at <code>{discovery.endpoint}</code>: <strong>{discovery.availability}</strong>{discovery.diagnostic ? ` · ${discovery.diagnostic}` : ""}</p>}
        {registry && <p className="form-hint">ONNX files are copied into application storage and run by the built-in interpreter for opsets {registry.onnxOpsets[0]}–{registry.onnxOpsets[1]} and these operators: {registry.onnxOperators.join(", ")}.</p>}
        {candidates.length === 0 ? <div className="empty-registry compact-empty"><BrainCircuit size={18} /><span>No model candidates. Discover installed Ollama models or select an ONNX file.</span></div> : (
          <div className="dataset-table-wrap">
            <table className="dataset-table" aria-label="Model candidates">
              <thead><tr><th>Candidate</th><th>Adapter</th><th>Content digest</th><th>State</th><th><span className="sr-only">Action</span></th></tr></thead>
              <tbody>{candidates.map((item) => <tr key={item.candidate_id}>
                <td><strong>{item.model_name}</strong><small>{item.onnx ? `input ${item.onnx.input.name} [${item.onnx.input.shape.join(", ")}] · ${item.onnx.parameter_count.toLocaleString()} parameters` : item.ollama ? [item.ollama.family, item.ollama.parameter_size, item.ollama.quantization_level].filter(Boolean).join(" · ") : ""}</small></td>
                <td>{item.adapter}</td>
                <td><code title={item.content_digest}>{shortHash(item.content_digest)}</code></td>
                <td>{item.eligible ? (item.registered_model_ids.length > 0 ? "Registered" : "Unregistered") : <span title={item.reason ?? undefined}>Not eligible: {item.reason}</span>}</td>
                <td>
                  <button className="inspect-button" type="button" disabled={!item.eligible || busy !== null} onClick={() => chooseCandidate(item)}>Review <ChevronRight size={14} /></button>
                  {item.adapter === "ONNX_NATIVE" && <button className="inspect-button" type="button" disabled={busy !== null} onClick={() => discardCandidate(item.candidate_id)}>Discard</button>}
                </td>
              </tr>)}</tbody>
            </table>
          </div>
        )}
      </section>

      {candidate && draft && <section className="data-section" aria-labelledby="model-register-title">
        <div className="data-section-heading">
          <div className="step-badge">02</div>
          <div><h2 id="model-register-title">Register {candidate.model_name}</h2><p>Identity binds the adapter, model name, content digest, declared input and output schema, and runtime settings. Authority is fixed to {registry?.executableAuthority ?? "ADVISORY_ONLY"}.</p></div>
        </div>
        <fieldset className="form-fieldset" disabled={busy !== null || registration !== null}>
          <div className="form-grid">
            <label className="form-field"><span>Input fields <small>{candidate.onnx ? `exactly ${candidate.onnx.input.width}, comma-separated` : "comma-separated lower_snake_case"}</small></span>
              <input type="text" value={draft.inputFields} onChange={(event) => setDraft({ ...draft, inputFields: event.target.value })} />
            </label>
            {draft.outputs.map((output, index) => <div className="form-field" key={index}>
              <label className="form-field"><span>Output {index + 1} name</span><input type="text" value={output.name} onChange={(event) => setDraft({ ...draft, outputs: draft.outputs.map((item, position) => position === index ? { ...item, name: event.target.value } : item) })} /></label>
              {candidate.adapter === "OLLAMA_LOOPBACK" && <label className="form-field"><span>Output {index + 1} kind</span>
                <select value={output.kind} onChange={(event) => setDraft({ ...draft, outputs: draft.outputs.map((item, position) => position === index ? { ...item, kind: event.target.value as OutputDraft["kind"] } : item) })}>
                  <option value="CATEGORY">Category labels</option>
                  <option value="FLOAT64">Bounded number</option>
                </select>
              </label>}
              {output.kind === "CATEGORY"
                ? <label className="form-field"><span>Output {index + 1} labels</span><input type="text" value={output.labels} onChange={(event) => setDraft({ ...draft, outputs: draft.outputs.map((item, position) => position === index ? { ...item, labels: event.target.value } : item) })} /></label>
                : <>
                  <label className="form-field"><span>Output {index + 1} minimum</span><input type="number" step="any" value={output.minimum} onChange={(event) => setDraft({ ...draft, outputs: draft.outputs.map((item, position) => position === index ? { ...item, minimum: event.target.value } : item) })} /></label>
                  <label className="form-field"><span>Output {index + 1} maximum</span><input type="number" step="any" value={output.maximum} onChange={(event) => setDraft({ ...draft, outputs: draft.outputs.map((item, position) => position === index ? { ...item, maximum: event.target.value } : item) })} /></label>
                </>}
            </div>)}
            {candidate.adapter === "OLLAMA_LOOPBACK" && <>
              <label className="form-field"><span>Seed</span><input type="number" value={draft.seed} onChange={(event) => setDraft({ ...draft, seed: event.target.value })} /></label>
              <label className="form-field"><span>Max output tokens <small>{registry ? `${registry.ollamaOutputTokens[0]}–${registry.ollamaOutputTokens[1]}` : ""}</small></span><input type="number" value={draft.maxOutputTokens} onChange={(event) => setDraft({ ...draft, maxOutputTokens: event.target.value })} /></label>
              <label className="form-field"><span>Context tokens <small>{registry ? `${registry.ollamaContextTokens[0]}–${registry.ollamaContextTokens[1]}` : ""}</small></span><input type="number" value={draft.contextTokens} onChange={(event) => setDraft({ ...draft, contextTokens: event.target.value })} /></label>
              <label className="form-field"><span>Instruction <small>no dates, symbols, or rows are added to it</small></span><textarea value={draft.instruction} onChange={(event) => setDraft({ ...draft, instruction: event.target.value })} /></label>
            </>}
          </div>
          {candidate.adapter === "OLLAMA_LOOPBACK" && <div className="form-actions"><button className="button button-quiet" type="button" onClick={() => setDraft({ ...draft, outputs: [...draft.outputs, { name: "", kind: "FLOAT64", minimum: "-1", maximum: "1", labels: "" }] })}>Add output</button></div>}
        </fieldset>
        {registration ? <div className="audit-summary" role="status">
          <ShieldCheck size={17} />
          <span>Model ID <code title={registration.model_id}>{shortHash(registration.model_id)}</code> · {registration.manifest.identity.adapter_version} · digest <code title={registration.manifest.identity.content_digest}>{shortHash(registration.manifest.identity.content_digest)}</code>{registration.already_registered ? " · already registered" : ""}. Registration records identity only; it does not establish accuracy or profitability.</span>
        </div> : null}
        <div className="form-actions">
          {registration ? <>
            <button className="button button-quiet" type="button" onClick={() => setRegistration(null)} disabled={busy !== null}><X size={15} />Edit</button>
            <button className="button button-primary" type="button" onClick={confirmRegistration} disabled={busy !== null}><FileCheck2 size={16} />{busy === "registering" ? "Registering…" : "Confirm registration"}</button>
          </> : <button className="button button-primary" type="button" onClick={previewRegistration} disabled={busy !== null}><ShieldCheck size={16} />Preview model identity</button>}
        </div>
      </section>}

      <section className="data-section registry-section" aria-labelledby="model-catalog-title">
        <div className="data-section-heading">
          <div className="step-badge">03</div>
          <div><h2 id="model-catalog-title">Registered models</h2><p>Each read rechecks the manifest and content. A changed or missing digest quarantines a model; an unreachable runtime is shown as unavailable.</p></div>
        </div>
        {models.length === 0 ? <div className="empty-registry"><GitBranch size={18} /><span>No models registered yet.</span></div> : (
          <div className="dataset-table-wrap">
            <table className="dataset-table" aria-label="Registered models">
              <thead><tr><th>Model</th><th>Status</th><th>Adapter</th><th>Inputs → outputs</th><th>Content digest</th></tr></thead>
              <tbody>{models.map((entry) => <tr key={entry.model_id}>
                <td><strong><code title={entry.model_id}>{shortHash(entry.model_id)}</code></strong><small>{entry.model_name ?? "Manifest unavailable"}</small></td>
                <td><span className={`status-pill ${statusClass(entry.status)}`}>{entry.status}</span></td>
                <td>{entry.adapter ?? "—"}</td>
                <td>{entry.manifest ? `${entry.manifest.identity.input_fields.join(", ")} → ${entry.manifest.identity.outputs.map((output) => output.name).join(", ")}` : "—"}</td>
                <td><code title={entry.manifest?.identity.content_digest}>{entry.manifest ? shortHash(entry.manifest.identity.content_digest) : "—"}</code></td>
              </tr>)}</tbody>
            </table>
          </div>
        )}
        {unusableModels}
      </section>

      <form onSubmit={createPreview} noValidate>
        <section className="data-section" aria-labelledby="inference-title">
          <div className="data-section-heading">
            <div className="step-badge">04</div>
            <div><h2 id="inference-title">Inference</h2><p>Bind every model input to a column of one READY feature set or decision set. Each row uses only its own values.</p></div>
          </div>
          {readyModels.length === 0 || eligibleInputs.length === 0 ? <>
            <div className="empty-registry"><GitBranch size={18} /><span>{readyModels.length === 0 ? "Register a model that verifies as READY first." : "No READY feature set or decision set is registered."}</span></div>
            {unavailableInputs}
          </> : (
            <fieldset className="form-fieldset" disabled={locked}>
              <div className="form-grid">
                <label className="form-field"><span>Model</span>
                  <select value={modelId} onChange={(event) => changeModel(event.target.value)}>
                    {models.map((item) => <option key={item.model_id} value={item.model_id} disabled={item.status !== "READY"}>{`${item.model_name ?? "Unknown"} · ${shortHash(item.model_id)}${item.status === "READY" ? "" : ` · ${item.status}`}`}</option>)}
                  </select>
                </label>
                <label className="form-field"><span>Input</span>
                  <select value={inputId} onChange={(event) => changeInput(event.target.value)}>
                    {inputs.map((item) => <option key={item.id} value={item.id} disabled={!item.eligible}>{`${item.kind === "FEATURE_SET" ? "Feature set" : "Decision set"} · ${item.target ? `${item.target.symbol} ${item.target.timeframe}` : "Unknown target"} · ${shortHash(item.id)}${item.eligible ? "" : ` · ${item.status} (unavailable)`}`}</option>)}
                  </select>
                </label>
                {model?.manifest?.identity.input_fields.map((field) => <label className="form-field" key={field}><span>{field} <small>model input</small></span>
                  <select value={bindings[field] ?? ""} onChange={(event) => setBindings({ ...bindings, [field]: event.target.value })}>
                    <option value="">Select a column</option>
                    {input?.columns.map((column) => <option key={bindingKey(column.binding)} value={bindingKey(column.binding)}>{column.label}</option>)}
                  </select>
                </label>)}
                <label className="form-field"><span>Max inference rows</span><input type="number" value={bounds.maxRows} onChange={(event) => setBounds({ ...bounds, maxRows: event.target.value })} /></label>
                <label className="form-field"><span>Max output size (MB)</span><input type="number" value={bounds.maxOutputMegabytes} onChange={(event) => setBounds({ ...bounds, maxOutputMegabytes: event.target.value })} /></label>
                <label className="form-field"><span>Request timeout (ms)</span><input type="number" value={bounds.requestTimeoutMs} onChange={(event) => setBounds({ ...bounds, requestTimeoutMs: event.target.value })} /></label>
              </div>
              {unavailableInputs}
            </fieldset>
          )}
          {registry && <ul className="temporal-rules">
            {([
              ["Rows", registry.inferenceSemantics.rows],
              ["Causality", registry.inferenceSemantics.causality],
              ["Skipped", registry.inferenceSemantics.skip],
              ["Invalid output", registry.inferenceSemantics.invalid_output],
              ["Authority", registry.inferenceSemantics.authority],
              ["Determinism", registry.inferenceSemantics.determinism],
            ] as const).map(([label, text]) => <li key={label}><strong>{label}</strong><span>{text}</span></li>)}
          </ul>}
          {!preview && (
            <div className="form-actions">
              {previewPending ? <>
                <span className="inline-progress" role="status"><span className="loading-indicator" /> {busy === "cancelling" ? "Waiting for inference to stop…" : progress ? `${phaseLabel[progress.phase]} · ${progress.processed_rows.toLocaleString()} / ${progress.total_rows.toLocaleString()} rows` : "Starting inference…"}</span>
                <button className="button button-quiet" type="button" onClick={cancelJob} disabled={busy === "cancelling"}><X size={15} />{busy === "cancelling" ? "Cancelling…" : "Cancel inference"}</button>
              </> : (
                <button className="button button-primary" type="submit" disabled={busy !== null || readyModels.length === 0 || eligibleInputs.length === 0}><ShieldCheck size={16} />Validate and preview inference</button>
              )}
            </div>
          )}
        </section>
      </form>

      {preview && <InferencePreviewPanel preview={preview.preview} busy={busy === "confirming" || busy === "cancelling"} onConfirm={confirmPreview} onCancel={cancelJob} />}

      <section className="data-section registry-section" aria-labelledby="inference-catalog-title">
        <div className="data-section-heading">
          <div className="step-badge">05</div>
          <div><h2 id="inference-catalog-title">Inference records</h2><p>Records are immutable. Every read rechecks the manifest, output digest, consumed input, and model registration. Select two or more over the same input to compare them.</p></div>
        </div>
        {records.length === 0 ? <div className="empty-registry"><GitBranch size={18} /><span>No inference records registered yet.</span></div> : <>
          <div className="dataset-table-wrap">
            <table className="dataset-table" aria-label="Inference records">
              <thead><tr><th><span className="sr-only">Compare</span></th><th>Record</th><th>Status</th><th>Determinism</th><th>OK / skipped / invalid</th><th><span className="sr-only">Action</span></th></tr></thead>
              <tbody>{records.map((entry) => <tr key={entry.record_id}>
                <td><input type="checkbox" aria-label={`Compare ${shortHash(entry.record_id)}`} checked={compareIds.includes(entry.record_id)} disabled={entry.status !== "READY"} onChange={() => toggleCompare(entry.record_id)} /></td>
                <td><strong><code title={entry.record_id}>{shortHash(entry.record_id)}</code></strong><small>{entry.manifest ? `${entry.manifest.lineage.model_name} on ${entry.manifest.lineage.target.symbol} ${entry.manifest.lineage.target.timeframe}` : "Manifest unavailable"}</small></td>
                <td><span className={`status-pill ${statusClass(entry.status)}`}>{entry.status}</span></td>
                <td>{entry.determinism.replaceAll("_", " ")}</td>
                <td>{entry.manifest ? `${entry.manifest.summary.ok_rows} / ${entry.manifest.summary.skipped_rows} / ${entry.manifest.summary.invalid_output_rows}` : "—"}</td>
                <td><button className="inspect-button" type="button" onClick={() => inspectRecord(entry.record_id)}>Inspect <ChevronRight size={14} /></button></td>
              </tr>)}</tbody>
            </table>
          </div>
          <div className="form-actions"><button className="button button-quiet" type="button" onClick={compareSelected} disabled={busy !== null || compareIds.length < 2}>Compare selected records</button></div>
        </>}
        {comparison && <ComparisonPanel report={comparison} />}
        {selectedRecord && <RecordDetails entry={selectedRecord} onClose={() => setSelectedRecord(null)} />}
      </section>

      <section className="data-section attempts-section" aria-labelledby="model-attempts-title">
        <div className="data-section-heading"><div className="step-badge step-muted">06</div><div><h2 id="model-attempts-title">Model Lab attempt history</h2><p>Registrations, inferences, replays, failures, and cancellations remain visible for reproducibility.</p></div></div>
        {attempts.length === 0 ? <div className="empty-registry compact-empty">No Model Lab attempts recorded.</div> : <div className="attempt-list">{attempts.slice(0, 8).map((attempt) => <div className="attempt-row" key={attempt.attempt_id}><span className={`attempt-status attempt-${attempt.status.toLowerCase()}`}>{attempt.status}</span><strong>{attempt.subject_id ? shortHash(attempt.subject_id) : attempt.operation}</strong><time>{attempt.completed_at_utc}</time>{attempt.diagnostic_message && <small title={attempt.diagnostic_message}>{attempt.diagnostic_code ?? "Diagnostic"}</small>}</div>)}</div>}
      </section>
    </div>
  );
}

function InferencePreviewPanel({ preview, busy, onConfirm, onCancel }: { preview: InferencePreview; busy: boolean; onConfirm: () => void; onCancel: () => void }) {
  const manifest = preview.manifest;
  const summary = manifest.summary;
  return <section className="data-section preview-panel" aria-labelledby="inference-preview-title">
    <div className="preview-title-row"><div><div className="eyebrow">INFERENCE PREVIEW · NOT YET REGISTERED · {manifest.authority}</div><h2 id="inference-preview-title">{manifest.lineage.model_name} on {manifest.lineage.target.symbol} {manifest.lineage.target.timeframe}</h2></div><span className={`status-pill ${statusClass(manifest.status)}`}>{manifest.status}</span></div>
    <div className="preview-stats"><Stat label="OK rows" value={summary.ok_rows.toLocaleString()} /><Stat label="Skipped rows" value={summary.skipped_rows.toLocaleString()} /><Stat label="Invalid outputs" value={summary.invalid_output_rows.toLocaleString()} /></div>
    <div className="audit-summary" role="status">
      <ShieldCheck size={17} />
      <span>Reasons: {Object.entries(summary.reasons).map(([reason, count]) => `${reason} ${count}`).join(" · ") || "none"}. {preview.existing ? (preview.existing.output_matches ? "This identity is already registered and this run reproduced its output digest; confirming records the replay." : "This identity is already registered with a different output digest; confirming records the run as NON_DETERMINISTIC and keeps the registered record.") : ""}</span>
    </div>
    <div className="series-table-wrap"><table className="series-table feature-values"><caption>Most recent {preview.sample_rows.length} rows</caption><thead><tr><th>Decision (UTC)</th><th>Row</th>{manifest.identity.bindings.map((binding) => <th key={binding.field}>{binding.field}</th>)}{manifest.outputs.map((output) => <th key={output.name}>{output.name}</th>)}</tr></thead><tbody>
      {preview.sample_rows.map((row) => <tr key={row.decision_time_utc}><td>{row.decision_time_utc}</td><td><strong>{row.row_status.replaceAll("_", " ")}</strong>{row.reason_code ? ` · ${row.reason_code.replaceAll("_", " ")}` : ""}</td>{row.inputs.map((value) => <td key={value.field}>{formatValue(value.value)}</td>)}{row.outputs.map((value) => <td key={value.field}>{value.label ?? formatValue(value.value)}</td>)}</tr>)}
    </tbody></table></div>
    <div className="hash-row"><span>Record ID</span><code title={manifest.record_id}>{shortHash(manifest.record_id)}</code><span>Model</span><code title={manifest.lineage.model_id}>{shortHash(manifest.lineage.model_id)}</code><span>Input</span><code title={manifest.lineage.input_id}>{shortHash(manifest.lineage.input_id)}</code><span>Output SHA-256</span><code title={manifest.output_sha256}>{shortHash(manifest.output_sha256)}</code></div>
    <div className="preview-actions"><button className="button button-quiet" type="button" onClick={onCancel} disabled={busy}><X size={15} />Cancel preview</button><button className="button button-primary" type="button" onClick={onConfirm} disabled={busy}><FileCheck2 size={16} />{busy ? "Freezing inference record…" : "Freeze and register inference record"}</button></div>
  </section>;
}

function ComparisonPanel({ report }: { report: ComparisonReport }) {
  const percent = (value: number | null) => value === null ? "—" : `${(value * 100).toFixed(1)}%`;
  return <section className="dataset-details" aria-label="Descriptive comparison">
    <div className="details-heading"><div><div className="eyebrow">DESCRIPTIVE COMPARISON · {report.rows.toLocaleString()} ROWS</div><h3><code>{shortHash(report.input_id)}</code></h3></div></div>
    <p className="form-hint">{report.notice}</p>
    <div className="series-table-wrap detail-series"><table className="series-table" aria-label="Coverage by record"><thead><tr><th>Record</th><th>Model</th><th>Coverage</th><th>Invalid output rate</th><th>Mean latency (ms)</th></tr></thead><tbody>
      {report.records.map((record) => <tr key={record.record_id}><td><code title={record.record_id}>{shortHash(record.record_id)}</code></td><td>{record.model_name}</td><td>{percent(record.coverage)}</td><td>{percent(record.invalid_output_rate)}</td><td>{record.mean_latency_us === null ? "—" : (record.mean_latency_us / 1000).toFixed(2)}</td></tr>)}
    </tbody></table></div>
    <div className="series-table-wrap detail-series"><table className="series-table" aria-label="Agreement between records"><thead><tr><th>Pair</th><th>Output</th><th>Rows both OK</th><th>Agreement</th><th>Mean |difference|</th><th>Max |difference|</th></tr></thead><tbody>
      {report.pairs.flatMap((pair) => pair.fields.map((field) => <tr key={`${pair.left_record_id}-${pair.right_record_id}-${field.field}`}><td><code>{shortHash(pair.left_record_id)}</code> · <code>{shortHash(pair.right_record_id)}</code></td><td>{field.field}</td><td>{field.rows_both_ok.toLocaleString()}</td><td>{percent(field.agreement_rate)}</td><td>{formatValue(field.mean_absolute_difference)}</td><td>{formatValue(field.max_absolute_difference)}</td></tr>))}
    </tbody></table></div>
  </section>;
}

function RecordDetails({ entry, onClose }: { entry: InferenceRecordCatalogEntry; onClose: () => void }) {
  const manifest = entry.manifest;
  return <section className="dataset-details" aria-label="Inference record integrity and lineage">
    <div className="details-heading"><div><div className="eyebrow">INFERENCE RECORD INSPECTION</div><h3><code>{shortHash(entry.record_id)}</code></h3></div><button type="button" className="icon-button" aria-label="Close inference record inspection" onClick={onClose}><X size={15} /></button></div>
    <div className="integrity-list"><Integrity value={entry.integrity.manifest_valid} label="Manifest and identity" /><Integrity value={entry.integrity.output_valid} label="Output SHA-256" /><Integrity value={entry.integrity.lineage_valid} label="Input and model lineage" /></div>
    {entry.diagnostic && <p className="integrity-diagnostic" role="alert">{entry.diagnostic}</p>}
    <div className="details-meta"><span>Record ID <code>{entry.record_id}</code></span><span>Model <code>{entry.model_id}</code></span><span>Input <code>{entry.input_id}</code></span><span>Determinism <code>{entry.determinism}</code> ({entry.replays.length} replay{entry.replays.length === 1 ? "" : "s"})</span>{manifest && <><span>Authority <code>{manifest.authority}</code></span><span>Feature set <code>{manifest.lineage.feature_set_id}</code></span><span>Output SHA-256 <code>{manifest.output_sha256}</code></span><span><Clock3 size={12} /> Created <code>{manifest.created_at_utc}</code></span></>}</div>
    {manifest && <div className="series-table-wrap detail-series"><table className="series-table"><thead><tr><th>Model input</th><th>Source</th><th>Column</th></tr></thead><tbody>{manifest.identity.bindings.map((binding) => <tr key={binding.field}><td>{binding.field}</td><td>{binding.source}</td><td>{binding.column}</td></tr>)}</tbody></table></div>}
  </section>;
}

function Integrity({ value, label }: { value: boolean; label: string }) {
  return <span className={value ? "integrity-valid" : "integrity-invalid"}>{value ? <Check size={13} /> : <AlertTriangle size={13} />}{label}: {value ? "verified" : "mismatch"}</span>;
}

function Stat({ label, value }: { label: string; value: string }) {
  return <div className="preview-stat"><span>{label}</span><strong>{value}</strong></div>;
}
