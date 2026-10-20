import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AlertTriangle, Check, ChevronRight, CircleHelp, Clock3, FileCheck2, Plus, RefreshCw, ShieldCheck, SlidersHorizontal, Trash2, X } from "lucide-react";
import type {
  ActiveFeatureJob,
  FeatureAttempt,
  FeatureCommitResult,
  FeatureInputCandidate,
  FeaturePreviewSession,
  FeatureRegistrySnapshot,
  FeatureSetCatalogEntry,
  FeatureSetPreview,
  FeatureSetRequest,
  FeatureSpec,
  IndicatorDefinition,
  ParameterValue,
  ProgressSnapshot,
} from "./feature-studio-types";

interface DraftFeature {
  draftId: number;
  key: string;
  indicatorId: string;
  parameters: Record<string, string>;
  sourceKind: "TARGET" | "REFERENCE";
  referenceDatasetId: string;
  referenceSeries: string;
  maxStalenessMinutes: string;
  lagMinutes: string;
}

interface Bounds {
  maxTargetRows: string;
  maxInputRows: string;
  maxOutputMegabytes: string;
}

const defaultBounds: Bounds = { maxTargetRows: "50000000", maxInputRows: "200000000", maxOutputMegabytes: "8192" };

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

function seriesValue(symbol: string, timeframe: string): string {
  return `${symbol}\u0000${timeframe}`;
}

function parseSeries(value: string): { symbol: string; timeframe: string } | null {
  const [symbol, timeframe] = value.split("\u0000");
  return symbol && timeframe ? { symbol, timeframe } : null;
}

function defaultParameters(definition: IndicatorDefinition): Record<string, string> {
  return Object.fromEntries(definition.parameters.map((spec) => [spec.name, String(spec.default)]));
}

function suggestedKey(definition: IndicatorDefinition, parameters: Record<string, string>, taken: Set<string>): string {
  const base = `${definition.id.toLowerCase()}${parameters.period ? `_${parameters.period}` : ""}`.replaceAll(/[^a-z0-9_]/g, "_").replaceAll(/_+/g, "_");
  let key = base;
  for (let index = 2; taken.has(key); index += 1) key = `${base}_v${index}`;
  return key;
}

function positiveInteger(value: string, label: string): number {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) throw new Error(`${label} must be a positive whole number.`);
  return parsed;
}

function buildRequest(
  registry: FeatureRegistrySnapshot,
  targetDatasetId: string,
  targetSeries: string,
  drafts: DraftFeature[],
  bounds: Bounds,
): FeatureSetRequest {
  const target = parseSeries(targetSeries);
  if (!targetDatasetId || !target) throw new Error("Select a READY input dataset and its target series.");
  if (drafts.length === 0) throw new Error("Add at least one feature.");
  const features = drafts.map((draft): FeatureSpec => {
    const definition = registry.indicators.find((item) => item.id === draft.indicatorId);
    if (!definition) throw new Error(`Feature ${draft.key || "(unnamed)"} uses an unknown indicator.`);
    const parameters: Record<string, ParameterValue> = {};
    for (const spec of definition.parameters) {
      const raw = (draft.parameters[spec.name] ?? "").trim();
      if (spec.kind === "CHOICE") {
        if (!spec.options.includes(raw)) throw new Error(`${draft.key}: ${spec.name} must be one of ${spec.options.join(", ")}.`);
        parameters[spec.name] = raw;
        continue;
      }
      const value = Number(raw);
      if (raw === "" || !Number.isFinite(value) || value < spec.min || value > spec.max || (spec.kind === "INTEGER" && !Number.isInteger(value))) {
        throw new Error(`${draft.key}: ${spec.name} must be ${spec.kind === "INTEGER" ? "a whole number" : "a number"} between ${spec.min} and ${spec.max}.`);
      }
      parameters[spec.name] = value;
    }
    if (draft.sourceKind === "TARGET") {
      return { key: draft.key.trim(), indicator_id: definition.id, indicator_version: definition.version, parameters, source: { kind: "TARGET" } };
    }
    const series = parseSeries(draft.referenceSeries);
    if (!draft.referenceDatasetId || !series) throw new Error(`${draft.key}: select the reference dataset and series.`);
    const staleness = Number(draft.maxStalenessMinutes) * 60;
    const lag = Number(draft.lagMinutes) * 60;
    if (!Number.isSafeInteger(staleness) || staleness < 1 || staleness > registry.limits.maxStalenessSeconds) {
      throw new Error(`${draft.key}: maximum staleness must be whole minutes between 1 and ${registry.limits.maxStalenessSeconds / 60}.`);
    }
    if (!Number.isSafeInteger(lag) || lag < 0 || lag > registry.limits.maxAvailabilityLagSeconds) {
      throw new Error(`${draft.key}: availability lag must be whole minutes between 0 and ${registry.limits.maxAvailabilityLagSeconds / 60}.`);
    }
    return {
      key: draft.key.trim(),
      indicator_id: definition.id,
      indicator_version: definition.version,
      parameters,
      source: {
        kind: "REFERENCE",
        series: { dataset_id: draft.referenceDatasetId, symbol: series.symbol, timeframe: series.timeframe },
        max_staleness_seconds: staleness,
        availability_lag_seconds: lag,
      },
    };
  });
  return {
    target: { dataset_id: targetDatasetId, symbol: target.symbol, timeframe: target.timeframe },
    features,
    quotas: {
      max_target_rows: positiveInteger(bounds.maxTargetRows, "Max target rows"),
      max_input_rows: positiveInteger(bounds.maxInputRows, "Max input rows"),
      max_output_bytes: positiveInteger(bounds.maxOutputMegabytes, "Max output size") * 1024 * 1024,
    },
  };
}

function formatValue(value: number | null): string {
  if (value === null) return "—";
  return Math.abs(value) >= 1000 ? value.toFixed(2) : value.toPrecision(7);
}

const phaseLabel: Record<ProgressSnapshot["phase"], string> = {
  QUEUED: "Queued",
  VERIFYING_INPUTS: "Re-verifying input fingerprints",
  COMPUTING: "Computing causal features",
  FINALIZING: "Writing and hashing output",
};

export default function FeatureStudio() {
  const [registry, setRegistry] = useState<FeatureRegistrySnapshot | null>(null);
  const [inputs, setInputs] = useState<FeatureInputCandidate[]>([]);
  const [featureSets, setFeatureSets] = useState<FeatureSetCatalogEntry[]>([]);
  const [attempts, setAttempts] = useState<FeatureAttempt[]>([]);
  const [targetDatasetId, setTargetDatasetId] = useState("");
  const [targetSeries, setTargetSeries] = useState("");
  const [drafts, setDrafts] = useState<DraftFeature[]>([]);
  const [bounds, setBounds] = useState<Bounds>(defaultBounds);
  const [preview, setPreview] = useState<FeaturePreviewSession | null>(null);
  const [activeJobId, setActiveJobId] = useState<string | null>(null);
  const [progress, setProgress] = useState<ProgressSnapshot | null>(null);
  const [previewPending, setPreviewPending] = useState(false);
  const [busy, setBusy] = useState<"loading" | "previewing" | "confirming" | "cancelling" | null>("loading");
  const [selectedSet, setSelectedSet] = useState<FeatureSetCatalogEntry | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const nextDraftId = useRef(1);
  const awaitingPreview = useRef(false);

  const eligibleInputs = inputs.filter((item) => item.eligible);

  const refreshCatalog = useCallback(async () => {
    const [sets, history] = await Promise.all([
      invoke<FeatureSetCatalogEntry[]>("list_feature_sets"),
      invoke<FeatureAttempt[]>("list_feature_attempts"),
    ]);
    setFeatureSets(sets);
    setAttempts(history);
    return sets;
  }, []);

  // Keeps the chosen target when it is still eligible; otherwise selects the first eligible dataset and series so the
  // form state matches what the target selects display.
  const selectDefaultTarget = useCallback((candidates: FeatureInputCandidate[], currentDatasetId: string) => {
    if (candidates.some((item) => item.eligible && item.dataset_id === currentDatasetId)) return;
    const firstEligible = candidates.find((item) => item.eligible);
    const firstSeries = firstEligible?.series[0];
    setTargetDatasetId(firstEligible?.dataset_id ?? "");
    setTargetSeries(firstSeries ? seriesValue(firstSeries.symbol, firstSeries.timeframe) : "");
  }, []);

  const refreshInputs = useCallback(async () => {
    const candidates = await invoke<FeatureInputCandidate[]>("list_feature_inputs");
    setInputs(candidates);
    selectDefaultTarget(candidates, targetDatasetId);
    return candidates;
  }, [selectDefaultTarget, targetDatasetId]);

  useEffect(() => {
    let current = true;
    Promise.all([
      invoke<FeatureRegistrySnapshot>("get_feature_registry"),
      invoke<FeatureInputCandidate[]>("list_feature_inputs"),
      invoke<FeatureSetCatalogEntry[]>("list_feature_sets"),
      invoke<FeatureAttempt[]>("list_feature_attempts"),
      invoke<ActiveFeatureJob | null>("get_active_feature_job"),
    ]).then(([nextRegistry, candidates, sets, history, active]) => {
      if (!current) return;
      setRegistry(nextRegistry);
      setInputs(candidates);
      setFeatureSets(sets);
      setAttempts(history);
      selectDefaultTarget(candidates, "");
      if (active?.state === "PREPARED") {
        setPreview(active.session);
        setActiveJobId(active.session.jobId);
        setNotice("The validated feature preview was restored. Confirm or cancel it before starting another.");
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
  }, [selectDefaultTarget]);

  useEffect(() => {
    if (!previewPending || !activeJobId) return;
    let current = true;
    let timer: number | undefined;
    async function poll() {
      try {
        const active = await invoke<ActiveFeatureJob | null>("get_active_feature_job");
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
          setNotice("The feature job ended before a preview was prepared. Check the attempt history, then retry.");
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

  const targetCandidate = inputs.find((item) => item.dataset_id === targetDatasetId);
  const locked = busy !== null || preview !== null || previewPending;
  const unavailableReasons = inputs.filter((item) => !item.eligible).map((item) => <p className="form-hint" key={item.dataset_id}><AlertTriangle size={13} /> {item.source_file_name ?? item.dataset_id}: {item.reason}</p>);

  function addFeature() {
    if (!registry) return;
    const definition = registry.indicators[0];
    const parameters = defaultParameters(definition);
    const taken = new Set(drafts.map((draft) => draft.key));
    setDrafts((current) => [...current, {
      draftId: nextDraftId.current++,
      key: suggestedKey(definition, parameters, taken),
      indicatorId: definition.id,
      parameters,
      sourceKind: "TARGET",
      referenceDatasetId: "",
      referenceSeries: "",
      maxStalenessMinutes: "60",
      lagMinutes: "0",
    }]);
  }

  function updateDraft(draftId: number, change: Partial<DraftFeature>) {
    setDrafts((current) => current.map((draft) => draft.draftId === draftId ? { ...draft, ...change } : draft));
  }

  function changeIndicator(draft: DraftFeature, indicatorId: string) {
    const definition = registry?.indicators.find((item) => item.id === indicatorId);
    if (!definition) return;
    const parameters = defaultParameters(definition);
    const taken = new Set(drafts.filter((item) => item.draftId !== draft.draftId).map((item) => item.key));
    updateDraft(draft.draftId, { indicatorId, parameters, key: suggestedKey(definition, parameters, taken) });
  }

  async function createPreview(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!registry) return;
    setError(null);
    setNotice(null);
    let request: FeatureSetRequest;
    try {
      request = buildRequest(registry, targetDatasetId, targetSeries, drafts, bounds);
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
      const session = await invoke<FeaturePreviewSession>("preview_feature_set", { jobId, request });
      setPreview(session);
    } catch (reason) {
      setActiveJobId(null);
      const text = message(reason);
      if (text.toLowerCase().includes("cancel")) setNotice("Feature computation cancelled. No feature set was registered.");
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
      result = await invoke<string>("cancel_feature_job", { jobId: activeJobId });
    } catch (reason) {
      setError(message(reason));
      setBusy(previewPending ? "previewing" : null);
      return;
    }
    if (result === "CANCELLATION_REQUESTED") {
      setNotice("Cancellation requested. The engine stops at its next checkpoint and registers nothing.");
      return;
    }
    setPreview(null);
    setActiveJobId(null);
    setBusy(null);
    setNotice("Preview cancelled. No feature set was registered.");
    try {
      await refreshCatalog();
    } catch (reason) {
      setError(`The preview was cancelled, but the feature-set history did not refresh: ${message(reason)}. Use Refresh to retry.`);
    }
  }

  async function confirmPreview() {
    if (!preview) return;
    setError(null);
    setBusy("confirming");
    let result: FeatureCommitResult;
    try {
      result = await invoke<FeatureCommitResult>("confirm_feature_set", { jobId: preview.jobId });
    } catch (reason) {
      // The backend consumes the prepared preview before committing, so it cannot be retried.
      setPreview(null);
      setActiveJobId(null);
      setBusy(null);
      setError(`The feature set was not registered: ${message(reason)}`);
      try { await refreshCatalog(); } catch { /* keep the commit diagnostic */ }
      return;
    }
    setPreview(null);
    setActiveJobId(null);
    setBusy(null);
    setNotice(`${result.disposition === "REUSED" ? "Identical feature set reused" : "Feature set frozen and registered"}: ${result.feature_set_id} · ${result.status}.`);
    try {
      const sets = await refreshCatalog();
      setSelectedSet(sets.find((item) => item.feature_set_id === result.feature_set_id) ?? null);
    } catch (reason) {
      setError(`The feature set was committed, but the catalogue did not refresh: ${message(reason)}. Use Refresh to retry.`);
    }
  }

  async function refreshAll() {
    setError(null);
    setBusy("loading");
    try {
      await Promise.all([refreshInputs(), refreshCatalog()]);
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }

  async function inspectSet(featureSetId: string) {
    setError(null);
    try {
      const entry = await invoke<FeatureSetCatalogEntry | null>("inspect_feature_set", { featureSetId });
      if (!entry) throw new Error("Feature set no longer exists in the registry.");
      setSelectedSet(entry);
    } catch (reason) {
      setError(message(reason));
    }
  }

  return (
    <div className="page-content data-ground-page feature-studio-page">
      <div className="page-heading data-ground-heading">
        <div>
          <div className="eyebrow">RG-R02 · FEATURE STUDIO</div>
          <h1>Feature Studio</h1>
          <p className="page-intro">Derive versioned, causal indicators from verified READY datasets and freeze them as immutable feature sets.</p>
        </div>
        <div className="data-ground-heading-actions">
          <button className="button button-quiet" type="button" onClick={refreshAll} disabled={busy !== null}><RefreshCw size={15} aria-hidden="true" />Refresh</button>
        </div>
      </div>

      {error && <div className="data-alert data-alert-error" role="alert"><AlertTriangle size={16} /><span>{error}</span><button type="button" aria-label="Dismiss error" onClick={() => setError(null)}><X size={15} /></button></div>}
      {notice && <div className="data-alert data-alert-notice" role="status"><Check size={16} /><span>{notice}</span><button type="button" aria-label="Dismiss notice" onClick={() => setNotice(null)}><X size={15} /></button></div>}
      {registry && <p className="feature-notice"><CircleHelp size={13} aria-hidden="true" /> {registry.researchNotice}</p>}

      <form onSubmit={createPreview} noValidate>
        <section className="data-section" aria-labelledby="feature-input-title">
          <div className="data-section-heading">
            <div className="step-badge">01</div>
            <div><h2 id="feature-input-title">Input dataset</h2><p>Only READY datasets that pass hash verification can be used. Nothing is read from the filesystem directly.</p></div>
          </div>
          {busy === "loading" && !registry ? (
            <div className="empty-registry"><span className="loading-indicator" /> Verifying registered datasets…</div>
          ) : eligibleInputs.length === 0 ? (
            <>
              <div className="empty-registry"><SlidersHorizontal size={18} /><span>No READY verified dataset is registered. Import and verify one in Data Ground first.</span></div>
              {unavailableReasons}
            </>
          ) : (
            <fieldset className="form-fieldset" disabled={locked}>
              <div className="form-grid">
                <label className="form-field"><span>Target dataset</span>
                  <select value={targetDatasetId} onChange={(event) => { setTargetDatasetId(event.target.value); setTargetSeries(""); }}>
                    {inputs.map((item) => <option key={item.dataset_id} value={item.dataset_id} disabled={!item.eligible}>{(item.source_file_name ?? item.dataset_id) + (item.eligible ? "" : ` · ${item.status} (unavailable)`)}</option>)}
                  </select>
                </label>
                <label className="form-field"><span>Target series</span>
                  <select value={targetSeries} onChange={(event) => setTargetSeries(event.target.value)} required>
                    <option value="">Select a series</option>
                    {targetCandidate?.series.map((series) => <option key={series.symbol + series.timeframe} value={seriesValue(series.symbol, series.timeframe)}>{series.symbol} · {series.timeframe} · {series.bar_count.toLocaleString()} bars</option>)}
                  </select>
                </label>
              </div>
              {targetCandidate && <p className="form-hint">Source <code>{targetCandidate.source_id}</code> · {targetCandidate.broker} / {targetCandidate.server} · dataset <code title={targetCandidate.dataset_id}>{shortHash(targetCandidate.dataset_id)}</code></p>}
              {unavailableReasons}
            </fieldset>
          )}
        </section>

        <section className="data-section" aria-labelledby="feature-registry-title">
          <div className="data-section-heading">
            <div className="step-badge">02</div>
            <div><h2 id="feature-registry-title">Features</h2><p>Native Rust indicators with typed, bounded parameters. Volume is not used: imported volume is not declared as tick or exchange volume.</p></div>
          </div>
          <fieldset className="form-fieldset" disabled={locked || !registry || eligibleInputs.length === 0}>
            {drafts.length === 0 && <div className="empty-registry compact-empty">No features configured yet.</div>}
            {drafts.map((draft) => {
              const definition = registry?.indicators.find((item) => item.id === draft.indicatorId);
              const referenceCandidate = inputs.find((item) => item.dataset_id === draft.referenceDatasetId);
              return (
                <div className="feature-card" key={draft.draftId}>
                  <div className="form-grid">
                    <label className="form-field"><span>Feature key</span><input value={draft.key} onChange={(event) => updateDraft(draft.draftId, { key: event.target.value })} required /></label>
                    <label className="form-field"><span>Indicator</span>
                      <select value={draft.indicatorId} onChange={(event) => changeIndicator(draft, event.target.value)}>
                        {registry?.indicators.map((item) => <option key={item.id} value={item.id}>{item.name} · v{item.version}</option>)}
                      </select>
                    </label>
                    {definition?.parameters.map((spec) => (
                      <label className="form-field" key={spec.name}><span>{spec.name} <small>{spec.kind === "CHOICE" ? spec.description : `${spec.min}–${spec.max}`}</small></span>
                        {spec.kind === "CHOICE" ? (
                          <select value={draft.parameters[spec.name] ?? spec.default} onChange={(event) => updateDraft(draft.draftId, { parameters: { ...draft.parameters, [spec.name]: event.target.value } })}>
                            {spec.options.map((option) => <option key={option} value={option}>{option}</option>)}
                          </select>
                        ) : (
                          <input type="number" min={spec.min} max={spec.max} step={spec.kind === "INTEGER" ? 1 : "any"} value={draft.parameters[spec.name] ?? ""} onChange={(event) => updateDraft(draft.draftId, { parameters: { ...draft.parameters, [spec.name]: event.target.value } })} />
                        )}
                      </label>
                    ))}
                    <label className="form-field"><span>Source</span>
                      <select value={draft.sourceKind} onChange={(event) => updateDraft(draft.draftId, { sourceKind: event.target.value as DraftFeature["sourceKind"] })}>
                        <option value="TARGET">Target series</option>
                        <option value="REFERENCE">Reference series (as-of join)</option>
                      </select>
                    </label>
                    {draft.sourceKind === "REFERENCE" && <>
                      <label className="form-field"><span>Reference dataset</span>
                        <select value={draft.referenceDatasetId} onChange={(event) => updateDraft(draft.draftId, { referenceDatasetId: event.target.value, referenceSeries: "" })}>
                          <option value="">Select a dataset</option>
                          {eligibleInputs.map((item) => <option key={item.dataset_id} value={item.dataset_id}>{item.source_file_name ?? item.dataset_id}</option>)}
                        </select>
                      </label>
                      <label className="form-field"><span>Reference series</span>
                        <select value={draft.referenceSeries} onChange={(event) => updateDraft(draft.draftId, { referenceSeries: event.target.value })}>
                          <option value="">Select a series</option>
                          {referenceCandidate?.series.map((series) => <option key={series.symbol + series.timeframe} value={seriesValue(series.symbol, series.timeframe)}>{series.symbol} · {series.timeframe}</option>)}
                        </select>
                      </label>
                      <label className="form-field"><span>Max staleness (minutes)</span><input type="number" min={1} step={1} value={draft.maxStalenessMinutes} onChange={(event) => updateDraft(draft.draftId, { maxStalenessMinutes: event.target.value })} /></label>
                      <label className="form-field"><span>Availability lag (minutes)</span><input type="number" min={0} step={1} value={draft.lagMinutes} onChange={(event) => updateDraft(draft.draftId, { lagMinutes: event.target.value })} /></label>
                    </>}
                  </div>
                  {definition && <p className="form-hint"><code>{definition.formula}</code> · {definition.warmup_rule} · outputs: {definition.outputs.join(", ")}</p>}
                  <button className="button button-quiet" type="button" onClick={() => setDrafts((current) => current.filter((item) => item.draftId !== draft.draftId))}><Trash2 size={14} />Remove {draft.key || "feature"}</button>
                </div>
              );
            })}
            <button className="button button-secondary" type="button" onClick={addFeature} disabled={!registry || drafts.length >= (registry?.limits.maxFeatures ?? 0)}><Plus size={15} />Add feature</button>
          </fieldset>
        </section>

        <section className="data-section" aria-labelledby="feature-time-title">
          <div className="data-section-heading">
            <div className="step-badge">03</div>
            <div><h2 id="feature-time-title">Timing contract and bounds</h2><p>Bound to every feature-set identity{registry ? ` (temporal semantics v${registry.temporalSemantics.version})` : ""}.</p></div>
          </div>
          {registry && <ul className="temporal-rules">
            {([
              ["Decision boundary", registry.temporalSemantics.decision_boundary],
              ["Current bar", registry.temporalSemantics.current_bar],
              ["Windows", registry.temporalSemantics.window_basis],
              ["Warmup", registry.temporalSemantics.warmup],
              ["Reference join", registry.temporalSemantics.reference_join],
              ["Staleness", registry.temporalSemantics.reference_staleness],
              ["Gaps", registry.temporalSemantics.gaps],
            ] as const).map(([label, text]) => <li key={label}><strong>{label}</strong><span>{text}</span></li>)}
          </ul>}
          <fieldset className="form-fieldset" disabled={locked}>
            <div className="form-grid">
              <label className="form-field"><span>Max target rows</span><input type="number" value={bounds.maxTargetRows} onChange={(event) => setBounds({ ...bounds, maxTargetRows: event.target.value })} /></label>
              <label className="form-field"><span>Max total input rows (all datasets)</span><input type="number" value={bounds.maxInputRows} onChange={(event) => setBounds({ ...bounds, maxInputRows: event.target.value })} /></label>
              <label className="form-field"><span>Max output size (MB)</span><input type="number" value={bounds.maxOutputMegabytes} onChange={(event) => setBounds({ ...bounds, maxOutputMegabytes: event.target.value })} /></label>
            </div>
          </fieldset>
          {!preview && (
            <div className="form-actions">
              {previewPending ? <>
                <span className="inline-progress" role="status"><span className="loading-indicator" /> {busy === "cancelling" ? "Waiting for the engine to stop…" : progress ? `${phaseLabel[progress.phase]} · ${progress.processed_rows.toLocaleString()} / ${progress.total_rows.toLocaleString()} rows` : "Starting feature computation…"}</span>
                <button className="button button-quiet" type="button" onClick={cancelJob} disabled={busy === "cancelling"}><X size={15} />{busy === "cancelling" ? "Cancelling…" : "Cancel computation"}</button>
              </> : (
                <button className="button button-primary" type="submit" disabled={busy !== null || !registry || eligibleInputs.length === 0 || drafts.length === 0}><ShieldCheck size={16} />Validate and preview</button>
              )}
            </div>
          )}
        </section>
      </form>

      {preview && <FeaturePreviewPanel preview={preview.preview} busy={busy === "confirming" || busy === "cancelling"} onConfirm={confirmPreview} onCancel={cancelJob} />}

      <section className="data-section registry-section" aria-labelledby="feature-catalog-title">
        <div className="data-section-heading">
          <div className="step-badge">04</div>
          <div><h2 id="feature-catalog-title">Feature-set catalogue</h2><p>Registered sets are immutable; every read rechecks the manifest, output digest, and the current bytes of every source dataset.</p></div>
        </div>
        {featureSets.length === 0 ? <div className="empty-registry"><SlidersHorizontal size={18} /><span>No feature sets registered yet.</span></div> : (
          <div className="dataset-table-wrap">
            <table className="dataset-table">
              <thead><tr><th>Feature set</th><th>Status</th><th>Features</th><th>Rows</th><th>Output SHA-256</th><th><span className="sr-only">Action</span></th></tr></thead>
              <tbody>{featureSets.map((entry) => <tr key={entry.feature_set_id}>
                <td><strong><code title={entry.feature_set_id}>{shortHash(entry.feature_set_id)}</code></strong><small>{entry.manifest ? `${entry.manifest.identity.target.symbol} ${entry.manifest.identity.target.timeframe} · ${entry.manifest.created_at_utc}` : "Manifest unavailable"}</small></td>
                <td><span className={`status-pill status-${entry.status.toLowerCase()}`}>{entry.status}</span></td>
                <td>{entry.manifest?.identity.features.map((feature) => feature.key).join(", ") ?? "—"}</td>
                <td>{entry.manifest?.row_count.toLocaleString() ?? "—"}</td>
                <td><code title={entry.manifest?.output_sha256}>{entry.manifest ? shortHash(entry.manifest.output_sha256) : "—"}</code></td>
                <td><button className="inspect-button" type="button" onClick={() => inspectSet(entry.feature_set_id)}>Inspect <ChevronRight size={14} /></button></td>
              </tr>)}</tbody>
            </table>
          </div>
        )}
        {selectedSet && <FeatureSetDetails entry={selectedSet} onClose={() => setSelectedSet(null)} />}
      </section>

      <section className="data-section attempts-section" aria-labelledby="feature-attempts-title">
        <div className="data-section-heading"><div className="step-badge step-muted">05</div><div><h2 id="feature-attempts-title">Feature attempt history</h2><p>Failed and cancelled computations remain visible for reproducibility.</p></div></div>
        {attempts.length === 0 ? <div className="empty-registry compact-empty">No feature attempts recorded.</div> : <div className="attempt-list">{attempts.slice(0, 8).map((attempt) => <div className="attempt-row" key={attempt.attempt_id}><span className={`attempt-status attempt-${attempt.status.toLowerCase()}`}>{attempt.status}</span><strong>{attempt.feature_set_id ? shortHash(attempt.feature_set_id) : "No feature set"}</strong><time>{attempt.completed_at_utc}</time>{attempt.diagnostic_message && <small title={attempt.diagnostic_message}>{attempt.diagnostic_code ?? "Diagnostic"}</small>}</div>)}</div>}
      </section>
    </div>
  );
}

function FeaturePreviewPanel({ preview, busy, onConfirm, onCancel }: { preview: FeatureSetPreview; busy: boolean; onConfirm: () => void; onCancel: () => void }) {
  const manifest = preview.manifest;
  const features = manifest.identity.features;
  return <section className="data-section preview-panel" aria-labelledby="feature-preview-title">
    <div className="preview-title-row"><div><div className="eyebrow">FEATURE PREVIEW · NOT YET REGISTERED</div><h2 id="feature-preview-title">{manifest.identity.target.symbol} {manifest.identity.target.timeframe} · {features.length} features</h2></div><span className={`status-pill status-${manifest.status.toLowerCase()}`}>{manifest.status}</span></div>
    <div className="preview-stats"><Stat label="Feature rows" value={manifest.row_count.toLocaleString()} /><Stat label="Input datasets" value={String(manifest.lineage.length)} /><Stat label="Engine" value={`${manifest.identity.engine_id} ${manifest.identity.engine_version}`} /></div>
    <div className="audit-summary" role="status">
      {manifest.status === "READY" ? <ShieldCheck size={17} /> : <AlertTriangle size={17} />}
      <span>{manifest.status === "READY" ? "Every feature has ready rows. Values were computed from the verified input bytes shown below." : "At least one feature has no ready rows (warmup, stale, or unobserved throughout). It will be registered as BLOCKED."}</span>
    </div>
    <div className="series-table-wrap"><table className="series-table"><thead><tr><th>Feature</th><th>Indicator</th><th>Source</th><th>Ready</th><th>Warmup</th><th>Stale</th><th>No observation</th><th>First ready decision</th></tr></thead><tbody>
      {manifest.feature_summaries.map((summary) => {
        const feature = features.find((item) => item.key === summary.key);
        return <tr key={summary.key}><td>{summary.key}</td><td>{feature ? `${feature.indicator_id} v${feature.indicator_version} ${Object.entries(feature.parameters).map(([name, value]) => `${name}=${value}`).join(" ")}` : "—"}</td><td>{feature?.source.kind === "REFERENCE" ? `${feature.source.series.symbol} ${feature.source.series.timeframe} · ≤${feature.source.max_staleness_seconds / 60}m stale · +${feature.source.availability_lag_seconds / 60}m lag` : "Target"}</td><td>{summary.ready_rows.toLocaleString()}</td><td>{summary.warmup_rows.toLocaleString()}</td><td>{summary.stale_rows.toLocaleString()}</td><td>{summary.no_observation_rows.toLocaleString()}</td><td>{summary.first_ready_decision_utc ?? "Never"}</td></tr>;
      })}
    </tbody></table></div>
    <div className="series-table-wrap"><table className="series-table feature-values"><caption>Most recent {preview.sample_rows.length} computed rows</caption><thead><tr><th>Decision (UTC)</th>{features.map((feature) => <th key={feature.key}>{feature.key}</th>)}</tr></thead><tbody>
      {preview.sample_rows.map((row) => <tr key={row.decision_time_utc}><td>{row.decision_time_utc}</td>{row.features.map((value) => <td key={value.key}>{value.status === "READY" ? value.values.map(formatValue).join(" / ") : <span className="row-status">{value.status.replaceAll("_", " ")}</span>}</td>)}</tr>)}
    </tbody></table></div>
    <div className="series-table-wrap"><table className="series-table"><caption>Input lineage</caption><thead><tr><th>Dataset</th><th>Source</th><th>Series used</th><th>Fingerprint</th></tr></thead><tbody>
      {manifest.lineage.map((input) => <tr key={input.dataset_id}><td>{input.source_file_name}</td><td>{input.source_id} · {input.broker}</td><td>{input.series_used.join(", ")}</td><td><code title={input.fingerprint}>{shortHash(input.fingerprint)}</code></td></tr>)}
    </tbody></table></div>
    <div className="hash-row"><span>Feature set ID</span><code title={manifest.feature_set_id}>{shortHash(manifest.feature_set_id)}</code><span>Manifest SHA-256</span><code title={preview.manifest_sha256}>{shortHash(preview.manifest_sha256)}</code><span>Output SHA-256</span><code title={manifest.output_sha256}>{shortHash(manifest.output_sha256)}</code><span>Output row fingerprint</span><code title={manifest.output_fingerprint}>{shortHash(manifest.output_fingerprint)}</code></div>
    <div className="preview-actions"><button className="button button-quiet" type="button" onClick={onCancel} disabled={busy}><X size={15} />Cancel preview</button><button className="button button-primary" type="button" onClick={onConfirm} disabled={busy}><FileCheck2 size={16} />{busy ? "Freezing feature set…" : manifest.status === "READY" ? "Freeze and register feature set" : "Register as BLOCKED"}</button></div>
  </section>;
}

function FeatureSetDetails({ entry, onClose }: { entry: FeatureSetCatalogEntry; onClose: () => void }) {
  const manifest = entry.manifest;
  return <section className="dataset-details" aria-label="Feature set integrity and lineage">
    <div className="details-heading"><div><div className="eyebrow">FEATURE SET INSPECTION</div><h3><code>{shortHash(entry.feature_set_id)}</code></h3></div><button type="button" className="icon-button" aria-label="Close feature set inspection" onClick={onClose}><X size={15} /></button></div>
    <div className="integrity-list"><Integrity value={entry.integrity.manifest_valid} label="Manifest and identity" /><Integrity value={entry.integrity.output_valid} label="Output SHA-256" /><Integrity value={entry.integrity.lineage_valid} label="Dataset lineage" /></div>
    {entry.diagnostic && <p className="integrity-diagnostic" role="alert">{entry.diagnostic}</p>}
    <div className="details-meta"><span>Feature set ID <code>{entry.feature_set_id}</code></span><span>Identity SHA-256 <code>{entry.identity_sha256}</code></span><span>Manifest SHA-256 <code>{entry.manifest_sha256}</code></span>{manifest && <><span>Output SHA-256 <code>{manifest.output_sha256}</code></span><span>Temporal semantics <code>v{manifest.identity.temporal_semantics.version}</code></span><span><Clock3 size={12} /> Created <code>{manifest.created_at_utc}</code></span></>}</div>
    {manifest && <div className="series-table-wrap detail-series"><table className="series-table"><thead><tr><th>Input dataset</th><th>Fingerprint</th><th>Canonical bars SHA-256</th><th>Bars</th></tr></thead><tbody>{manifest.identity.inputs.map((input) => <tr key={input.dataset_id}><td><code title={input.dataset_id}>{shortHash(input.dataset_id)}</code></td><td><code title={input.fingerprint}>{shortHash(input.fingerprint)}</code></td><td><code title={input.canonical_bars_sha256}>{shortHash(input.canonical_bars_sha256)}</code></td><td>{input.bar_count.toLocaleString()}</td></tr>)}</tbody></table></div>}
  </section>;
}

function Integrity({ value, label }: { value: boolean; label: string }) {
  return <span className={value ? "integrity-valid" : "integrity-invalid"}>{value ? <Check size={13} /> : <AlertTriangle size={13} />}{label}: {value ? "verified" : "mismatch"}</span>;
}

function Stat({ label, value }: { label: string; value: string }) {
  return <div className="preview-stat"><span>{label}</span><strong>{value}</strong></div>;
}
