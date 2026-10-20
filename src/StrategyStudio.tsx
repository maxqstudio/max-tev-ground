import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AlertTriangle, Check, ChevronRight, CircleHelp, Clock3, FileCheck2, GitBranch, RefreshCw, ShieldCheck, X } from "lucide-react";
import type {
  ActiveDecisionJob,
  CandidateFeature,
  DecisionAttempt,
  DecisionCommitResult,
  DecisionInputCandidate,
  DecisionPreviewSession,
  DecisionProgressSnapshot,
  DecisionSetCatalogEntry,
  DecisionSetPreview,
  DecisionSetRequest,
  StrategyDefinition,
  StrategyRegistrySnapshot,
} from "./strategy-studio-types";

interface Bounds {
  maxRows: string;
  maxOutputMegabytes: string;
}

const defaultBounds: Bounds = { maxRows: "50000000", maxOutputMegabytes: "8192" };

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

function bindingValue(feature: CandidateFeature, output: string): string {
  return `${feature.key}\u0000${output}`;
}

function parseBinding(value: string): { feature_key: string; output: string } | null {
  const [feature_key, output] = value.split("\u0000");
  return feature_key && output ? { feature_key, output } : null;
}

// Every feature output a role may bind: only features whose source matches the role's source.
function bindingOptions(candidate: DecisionInputCandidate | undefined, source: "TARGET" | "REFERENCE"): { value: string; label: string }[] {
  return (candidate?.features ?? [])
    .filter((feature) => feature.source === source)
    .flatMap((feature) => feature.outputs.map((output) => ({
      value: bindingValue(feature, output),
      label: `${feature.key}${feature.outputs.length > 1 ? ` · ${output}` : ""} (${feature.indicator_id}${feature.reference ? ` on ${feature.reference.symbol} ${feature.reference.timeframe}` : ""})`,
    })));
}

function defaultBindings(strategy: StrategyDefinition | undefined, candidate: DecisionInputCandidate | undefined): Record<string, string> {
  return Object.fromEntries((strategy?.bindings ?? []).map((binding, index) => {
    const options = bindingOptions(candidate, binding.source);
    // Spread same-source roles over different features when possible, e.g. fast and slow.
    const sameSourceBefore = strategy?.bindings.slice(0, index).filter((item) => item.source === binding.source).length ?? 0;
    return [binding.role, options[Math.min(sameSourceBefore, options.length - 1)]?.value ?? ""];
  }));
}

function defaultParameters(strategy: StrategyDefinition | undefined): Record<string, string> {
  return Object.fromEntries((strategy?.parameters ?? []).map((spec) => [spec.name, String(spec.default)]));
}

function buildRequest(
  strategy: StrategyDefinition | undefined,
  featureSetId: string,
  bindings: Record<string, string>,
  parameters: Record<string, string>,
  bounds: Bounds,
): DecisionSetRequest {
  if (!featureSetId) throw new Error("Select a READY feature set.");
  if (!strategy) throw new Error("Select a strategy.");
  const requestBindings: DecisionSetRequest["bindings"] = {};
  for (const binding of strategy.bindings) {
    const parsed = parseBinding(bindings[binding.role] ?? "");
    if (!parsed) throw new Error(`Bind the ${binding.role} role to a ${binding.source === "TARGET" ? "target-series" : "reference-series"} feature.`);
    requestBindings[binding.role] = parsed;
  }
  const requestParameters: Record<string, number> = {};
  for (const spec of strategy.parameters) {
    const raw = (parameters[spec.name] ?? "").trim();
    const value = Number(raw);
    if (raw === "" || !Number.isFinite(value) || value < spec.min || value > spec.max) {
      throw new Error(`${spec.name} must be a number between ${spec.min} and ${spec.max}.`);
    }
    requestParameters[spec.name] = value;
  }
  const maxRows = Number(bounds.maxRows);
  const maxMegabytes = Number(bounds.maxOutputMegabytes);
  if (!Number.isSafeInteger(maxRows) || maxRows <= 0) throw new Error("Max decision rows must be a positive whole number.");
  if (!Number.isSafeInteger(maxMegabytes) || maxMegabytes <= 0) throw new Error("Max output size must be a positive whole number.");
  return {
    feature_set_id: featureSetId,
    strategy_id: strategy.id,
    strategy_version: strategy.version,
    parameters: requestParameters,
    bindings: requestBindings,
    quotas: { max_rows: maxRows, max_output_bytes: maxMegabytes * 1024 * 1024 },
  };
}

function formatValue(value: number | null): string {
  if (value === null) return "—";
  return Math.abs(value) >= 1000 ? value.toFixed(2) : value.toPrecision(7);
}

const phaseLabel: Record<DecisionProgressSnapshot["phase"], string> = {
  QUEUED: "Queued",
  VERIFYING_INPUT: "Re-verifying the feature set",
  EVALUATING: "Evaluating decisions",
  FINALIZING: "Writing and hashing decisions",
};

export default function StrategyStudio() {
  const [registry, setRegistry] = useState<StrategyRegistrySnapshot | null>(null);
  const [inputs, setInputs] = useState<DecisionInputCandidate[]>([]);
  const [decisionSets, setDecisionSets] = useState<DecisionSetCatalogEntry[]>([]);
  const [attempts, setAttempts] = useState<DecisionAttempt[]>([]);
  const [featureSetId, setFeatureSetId] = useState("");
  const [strategyId, setStrategyId] = useState("");
  const [bindings, setBindings] = useState<Record<string, string>>({});
  const [parameters, setParameters] = useState<Record<string, string>>({});
  const [bounds, setBounds] = useState<Bounds>(defaultBounds);
  const [preview, setPreview] = useState<DecisionPreviewSession | null>(null);
  const [activeJobId, setActiveJobId] = useState<string | null>(null);
  const [progress, setProgress] = useState<DecisionProgressSnapshot | null>(null);
  const [previewPending, setPreviewPending] = useState(false);
  const [busy, setBusy] = useState<"loading" | "previewing" | "confirming" | "cancelling" | null>("loading");
  const [selectedSet, setSelectedSet] = useState<DecisionSetCatalogEntry | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const awaitingPreview = useRef(false);

  const eligibleInputs = inputs.filter((item) => item.eligible);
  const candidate = inputs.find((item) => item.feature_set_id === featureSetId);
  const strategy = registry?.strategies.find((item) => item.id === strategyId);
  const locked = busy !== null || preview !== null || previewPending;

  const selectInput = useCallback((candidates: DecisionInputCandidate[], currentId: string, nextStrategy: StrategyDefinition | undefined) => {
    const current = candidates.find((item) => item.eligible && item.feature_set_id === currentId);
    const chosen = current ?? candidates.find((item) => item.eligible);
    setFeatureSetId(chosen?.feature_set_id ?? "");
    if (!current) setBindings(defaultBindings(nextStrategy, chosen));
  }, []);

  const refreshCatalog = useCallback(async () => {
    const [sets, history] = await Promise.all([
      invoke<DecisionSetCatalogEntry[]>("list_decision_sets"),
      invoke<DecisionAttempt[]>("list_decision_attempts"),
    ]);
    setDecisionSets(sets);
    setAttempts(history);
    return sets;
  }, []);

  useEffect(() => {
    let current = true;
    Promise.all([
      invoke<StrategyRegistrySnapshot>("get_strategy_registry"),
      invoke<DecisionInputCandidate[]>("list_decision_inputs"),
      invoke<DecisionSetCatalogEntry[]>("list_decision_sets"),
      invoke<DecisionAttempt[]>("list_decision_attempts"),
      invoke<ActiveDecisionJob | null>("get_active_decision_job"),
    ]).then(([nextRegistry, candidates, sets, history, active]) => {
      if (!current) return;
      const firstStrategy = nextRegistry.strategies[0];
      setRegistry(nextRegistry);
      setInputs(candidates);
      setDecisionSets(sets);
      setAttempts(history);
      setStrategyId(firstStrategy?.id ?? "");
      setParameters(defaultParameters(firstStrategy));
      selectInput(candidates, "", firstStrategy);
      if (active?.state === "PREPARED") {
        setPreview(active.session);
        setActiveJobId(active.session.jobId);
        setNotice("The validated decision preview was restored. Confirm or cancel it before starting another.");
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
  }, [selectInput]);

  useEffect(() => {
    if (!previewPending || !activeJobId) return;
    let current = true;
    let timer: number | undefined;
    async function poll() {
      try {
        const active = await invoke<ActiveDecisionJob | null>("get_active_decision_job");
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
          setNotice("The strategy job ended before a preview was prepared. Check the attempt history, then retry.");
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

  function changeFeatureSet(id: string) {
    setFeatureSetId(id);
    setBindings(defaultBindings(strategy, inputs.find((item) => item.feature_set_id === id)));
  }

  function changeStrategy(id: string) {
    const next = registry?.strategies.find((item) => item.id === id);
    setStrategyId(id);
    setParameters(defaultParameters(next));
    setBindings(defaultBindings(next, candidate));
  }

  async function createPreview(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setError(null);
    setNotice(null);
    let request: DecisionSetRequest;
    try {
      request = buildRequest(strategy, featureSetId, bindings, parameters, bounds);
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
      setPreview(await invoke<DecisionPreviewSession>("preview_decision_set", { jobId, request }));
    } catch (reason) {
      setActiveJobId(null);
      const text = message(reason);
      if (text.toLowerCase().includes("cancel")) setNotice("Strategy evaluation cancelled. No decision set was registered.");
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
      result = await invoke<string>("cancel_decision_job", { jobId: activeJobId });
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
    setNotice("Preview cancelled. No decision set was registered.");
    try {
      await refreshCatalog();
    } catch (reason) {
      setError(`The preview was cancelled, but the decision-set history did not refresh: ${message(reason)}. Use Refresh to retry.`);
    }
  }

  async function confirmPreview() {
    if (!preview) return;
    setError(null);
    setBusy("confirming");
    let result: DecisionCommitResult;
    try {
      result = await invoke<DecisionCommitResult>("confirm_decision_set", { jobId: preview.jobId });
    } catch (reason) {
      // The backend consumes the prepared preview before committing, so it cannot be retried.
      setPreview(null);
      setActiveJobId(null);
      setBusy(null);
      setError(`The decision set was not registered: ${message(reason)}`);
      try { await refreshCatalog(); } catch { /* keep the commit diagnostic */ }
      return;
    }
    setPreview(null);
    setActiveJobId(null);
    setBusy(null);
    setNotice(`${result.disposition === "REUSED" ? "Identical decision set reused" : "Decision set frozen and registered"}: ${result.decision_set_id} · ${result.status}.`);
    try {
      const sets = await refreshCatalog();
      setSelectedSet(sets.find((item) => item.decision_set_id === result.decision_set_id) ?? null);
    } catch (reason) {
      setError(`The decision set was committed, but the catalogue did not refresh: ${message(reason)}. Use Refresh to retry.`);
    }
  }

  async function refreshAll() {
    setError(null);
    setBusy("loading");
    try {
      const [candidates] = await Promise.all([invoke<DecisionInputCandidate[]>("list_decision_inputs"), refreshCatalog()]);
      setInputs(candidates);
      selectInput(candidates, featureSetId, strategy);
    } catch (reason) {
      setError(message(reason));
    } finally {
      setBusy(null);
    }
  }

  async function inspectSet(decisionSetId: string) {
    setError(null);
    try {
      const entry = await invoke<DecisionSetCatalogEntry | null>("inspect_decision_set", { decisionSetId });
      if (!entry) throw new Error("Decision set no longer exists in the registry.");
      setSelectedSet(entry);
    } catch (reason) {
      setError(message(reason));
    }
  }

  const unavailableReasons = inputs.filter((item) => !item.eligible).map((item) => <p className="form-hint" key={item.feature_set_id}><AlertTriangle size={13} /> <code title={item.feature_set_id}>{shortHash(item.feature_set_id)}</code>: {item.reason}</p>);

  return (
    <div className="page-content data-ground-page feature-studio-page">
      <div className="page-heading data-ground-heading">
        <div>
          <div className="eyebrow">RG-R03 · STRATEGY STUDIO</div>
          <h1>Strategy Studio</h1>
          <p className="page-intro">Turn one verified READY feature set into BUY, SELL, or SKIP research decisions with a versioned, deterministic rule.</p>
        </div>
        <div className="data-ground-heading-actions">
          <button className="button button-quiet" type="button" onClick={refreshAll} disabled={busy !== null}><RefreshCw size={15} aria-hidden="true" />Refresh</button>
        </div>
      </div>

      {error && <div className="data-alert data-alert-error" role="alert"><AlertTriangle size={16} /><span>{error}</span><button type="button" aria-label="Dismiss error" onClick={() => setError(null)}><X size={15} /></button></div>}
      {notice && <div className="data-alert data-alert-notice" role="status"><Check size={16} /><span>{notice}</span><button type="button" aria-label="Dismiss notice" onClick={() => setNotice(null)}><X size={15} /></button></div>}
      {registry && <p className="feature-notice"><CircleHelp size={13} aria-hidden="true" /> {registry.researchNotice}</p>}

      <form onSubmit={createPreview} noValidate>
        <section className="data-section" aria-labelledby="strategy-input-title">
          <div className="data-section-heading">
            <div className="step-badge">01</div>
            <div><h2 id="strategy-input-title">Feature set</h2><p>Only READY feature sets that re-verify down to their source datasets can be used. Decisions are made for the feature set's one target series.</p></div>
          </div>
          {busy === "loading" && !registry ? (
            <div className="empty-registry"><span className="loading-indicator" /> Verifying registered feature sets…</div>
          ) : eligibleInputs.length === 0 ? (
            <>
              <div className="empty-registry"><GitBranch size={18} /><span>No READY feature set is registered. Create and freeze one in Feature Studio first.</span></div>
              {unavailableReasons}
            </>
          ) : (
            <fieldset className="form-fieldset" disabled={locked}>
              <div className="form-grid">
                <label className="form-field"><span>Feature set</span>
                  <select value={featureSetId} onChange={(event) => changeFeatureSet(event.target.value)}>
                    {inputs.map((item) => <option key={item.feature_set_id} value={item.feature_set_id} disabled={!item.eligible}>{`${item.target ? `${item.target.symbol} ${item.target.timeframe}` : "Unknown target"} · ${shortHash(item.feature_set_id)}${item.eligible ? "" : ` · ${item.status} (unavailable)`}`}</option>)}
                  </select>
                </label>
              </div>
              {candidate?.target && <p className="form-hint">Target <strong>{candidate.target.symbol} {candidate.target.timeframe}</strong> · {candidate.row_count?.toLocaleString()} rows · features {candidate.features.map((feature) => feature.key).join(", ")}</p>}
              {unavailableReasons}
            </fieldset>
          )}
        </section>

        <section className="data-section" aria-labelledby="strategy-rule-title">
          <div className="data-section-heading">
            <div className="step-badge">02</div>
            <div><h2 id="strategy-rule-title">Strategy</h2><p>Native Rust rules with typed parameters. Each role must be bound to a feature from the required source; reference features only filter.</p></div>
          </div>
          <fieldset className="form-fieldset" disabled={locked || !registry || eligibleInputs.length === 0}>
            <div className="form-grid">
              <label className="form-field"><span>Strategy</span>
                <select value={strategyId} onChange={(event) => changeStrategy(event.target.value)}>
                  {registry?.strategies.map((item) => <option key={item.id} value={item.id}>{item.name} · v{item.version}</option>)}
                </select>
              </label>
              {strategy?.bindings.map((binding) => {
                const options = bindingOptions(candidate, binding.source);
                return (
                  <label className="form-field" key={binding.role}><span>{binding.role} <small>{binding.source === "TARGET" ? "target feature" : "reference feature"}</small></span>
                    <select value={bindings[binding.role] ?? ""} onChange={(event) => setBindings({ ...bindings, [binding.role]: event.target.value })}>
                      <option value="">{options.length === 0 ? `No ${binding.source === "TARGET" ? "target" : "reference"} feature in this set` : "Select a feature"}</option>
                      {options.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}
                    </select>
                  </label>
                );
              })}
              {strategy?.parameters.map((spec) => (
                <label className="form-field" key={spec.name}><span>{spec.name} <small>{spec.description}</small></span>
                  <input type="number" step="any" min={spec.min} max={spec.max} value={parameters[spec.name] ?? ""} onChange={(event) => setParameters({ ...parameters, [spec.name]: event.target.value })} />
                </label>
              ))}
            </div>
            {strategy && <p className="form-hint"><code>{strategy.decision_rule}</code> · {strategy.equality_semantics} · {strategy.warmup_rule}</p>}
          </fieldset>
        </section>

        <section className="data-section" aria-labelledby="strategy-contract-title">
          <div className="data-section-heading">
            <div className="step-badge">03</div>
            <div><h2 id="strategy-contract-title">Decision contract and bounds</h2><p>Bound to every decision-set identity{registry ? ` (decision semantics v${registry.decisionSemantics.version})` : ""}.</p></div>
          </div>
          {registry && <ul className="temporal-rules">
            {([
              ["Rows", registry.decisionSemantics.decision_rows],
              ["Causality", registry.decisionSemantics.causality],
              ["Context", registry.decisionSemantics.context],
              ["SKIP order", registry.decisionSemantics.skip_precedence],
              ["Equality", registry.decisionSemantics.equality],
              ["Conflict", registry.decisionSemantics.conflict],
            ] as const).map(([label, text]) => <li key={label}><strong>{label}</strong><span>{text}</span></li>)}
          </ul>}
          <fieldset className="form-fieldset" disabled={locked}>
            <div className="form-grid">
              <label className="form-field"><span>Max decision rows</span><input type="number" value={bounds.maxRows} onChange={(event) => setBounds({ ...bounds, maxRows: event.target.value })} /></label>
              <label className="form-field"><span>Max output size (MB)</span><input type="number" value={bounds.maxOutputMegabytes} onChange={(event) => setBounds({ ...bounds, maxOutputMegabytes: event.target.value })} /></label>
            </div>
          </fieldset>
          {!preview && (
            <div className="form-actions">
              {previewPending ? <>
                <span className="inline-progress" role="status"><span className="loading-indicator" /> {busy === "cancelling" ? "Waiting for the engine to stop…" : progress ? `${phaseLabel[progress.phase]} · ${progress.processed_rows.toLocaleString()} / ${progress.total_rows.toLocaleString()} rows` : "Starting strategy evaluation…"}</span>
                <button className="button button-quiet" type="button" onClick={cancelJob} disabled={busy === "cancelling"}><X size={15} />{busy === "cancelling" ? "Cancelling…" : "Cancel evaluation"}</button>
              </> : (
                <button className="button button-primary" type="submit" disabled={busy !== null || !registry || eligibleInputs.length === 0}><ShieldCheck size={16} />Validate and preview decisions</button>
              )}
            </div>
          )}
        </section>
      </form>

      {preview && <DecisionPreviewPanel preview={preview.preview} busy={busy === "confirming" || busy === "cancelling"} onConfirm={confirmPreview} onCancel={cancelJob} />}

      <section className="data-section registry-section" aria-labelledby="decision-catalog-title">
        <div className="data-section-heading">
          <div className="step-badge">04</div>
          <div><h2 id="decision-catalog-title">Decision-set catalogue</h2><p>Registered sets are immutable; every read rechecks the manifest, the output digest, and the consumed feature set down to its source datasets.</p></div>
        </div>
        {decisionSets.length === 0 ? <div className="empty-registry"><GitBranch size={18} /><span>No decision sets registered yet.</span></div> : (
          <div className="dataset-table-wrap">
            <table className="dataset-table">
              <thead><tr><th>Decision set</th><th>Status</th><th>Strategy</th><th>BUY / SELL / SKIP</th><th>Output SHA-256</th><th><span className="sr-only">Action</span></th></tr></thead>
              <tbody>{decisionSets.map((entry) => <tr key={entry.decision_set_id}>
                <td><strong><code title={entry.decision_set_id}>{shortHash(entry.decision_set_id)}</code></strong><small>{entry.manifest ? `${entry.manifest.lineage.target.symbol} ${entry.manifest.lineage.target.timeframe} · ${entry.manifest.created_at_utc}` : "Manifest unavailable"}</small></td>
                <td><span className={`status-pill status-${entry.status.toLowerCase()}`}>{entry.status}</span></td>
                <td>{entry.manifest ? `${entry.manifest.identity.strategy.strategy_id} v${entry.manifest.identity.strategy.strategy_version}` : "—"}</td>
                <td>{entry.manifest ? `${entry.manifest.summary.buy_rows} / ${entry.manifest.summary.sell_rows} / ${entry.manifest.summary.skip_rows}` : "—"}</td>
                <td><code title={entry.manifest?.output_sha256}>{entry.manifest ? shortHash(entry.manifest.output_sha256) : "—"}</code></td>
                <td><button className="inspect-button" type="button" onClick={() => inspectSet(entry.decision_set_id)}>Inspect <ChevronRight size={14} /></button></td>
              </tr>)}</tbody>
            </table>
          </div>
        )}
        {selectedSet && <DecisionSetDetails entry={selectedSet} onClose={() => setSelectedSet(null)} />}
      </section>

      <section className="data-section attempts-section" aria-labelledby="decision-attempts-title">
        <div className="data-section-heading"><div className="step-badge step-muted">05</div><div><h2 id="decision-attempts-title">Strategy attempt history</h2><p>Failed and cancelled evaluations remain visible for reproducibility.</p></div></div>
        {attempts.length === 0 ? <div className="empty-registry compact-empty">No strategy attempts recorded.</div> : <div className="attempt-list">{attempts.slice(0, 8).map((attempt) => <div className="attempt-row" key={attempt.attempt_id}><span className={`attempt-status attempt-${attempt.status.toLowerCase()}`}>{attempt.status}</span><strong>{attempt.decision_set_id ? shortHash(attempt.decision_set_id) : "No decision set"}</strong><time>{attempt.completed_at_utc}</time>{attempt.diagnostic_message && <small title={attempt.diagnostic_message}>{attempt.diagnostic_code ?? "Diagnostic"}</small>}</div>)}</div>}
      </section>
    </div>
  );
}

function DecisionPreviewPanel({ preview, busy, onConfirm, onCancel }: { preview: DecisionSetPreview; busy: boolean; onConfirm: () => void; onCancel: () => void }) {
  const manifest = preview.manifest;
  const summary = manifest.summary;
  const roles = manifest.identity.strategy.bindings;
  return <section className="data-section preview-panel" aria-labelledby="decision-preview-title">
    <div className="preview-title-row"><div><div className="eyebrow">DECISION PREVIEW · NOT YET REGISTERED</div><h2 id="decision-preview-title">{manifest.lineage.target.symbol} {manifest.lineage.target.timeframe} · {manifest.identity.strategy.strategy_id} v{manifest.identity.strategy.strategy_version}</h2></div><span className={`status-pill status-${manifest.status.toLowerCase()}`}>{manifest.status}</span></div>
    <div className="preview-stats"><Stat label="BUY rows" value={summary.buy_rows.toLocaleString()} /><Stat label="SELL rows" value={summary.sell_rows.toLocaleString()} /><Stat label="SKIP rows" value={summary.skip_rows.toLocaleString()} /></div>
    <div className="audit-summary" role="status">
      <ShieldCheck size={17} />
      <span>One decision per target bar, read from the verified feature values shown below. SKIP reasons: {Object.entries(summary.skip_reasons).map(([reason, count]) => `${reason} ${count}`).join(" · ") || "none"}. {manifest.lineage.context_series.length > 0 ? `Context only: ${manifest.lineage.context_series.map((series) => `${series.symbol} ${series.timeframe}`).join(", ")}.` : ""}</span>
    </div>
    <div className="series-table-wrap"><table className="series-table feature-values"><caption>Most recent {preview.sample_rows.length} decisions</caption><thead><tr><th>Decision (UTC)</th><th>Decision</th><th>Reason</th>{roles.map((binding) => <th key={binding.role}>{binding.role} ({binding.feature_key})</th>)}</tr></thead><tbody>
      {preview.sample_rows.map((row) => <tr key={row.decision_time_utc}><td>{row.decision_time_utc}</td><td><strong>{row.decision}</strong></td><td>{row.reason_code.replaceAll("_", " ")}{row.reason_role ? ` · ${row.reason_role}` : ""}</td>{row.inputs.map((input) => <td key={input.role}>{input.value !== null ? formatValue(input.value) : <span className="row-status">{input.status.replaceAll("_", " ")}</span>}</td>)}</tr>)}
    </tbody></table></div>
    <div className="hash-row"><span>Decision set ID</span><code title={manifest.decision_set_id}>{shortHash(manifest.decision_set_id)}</code><span>Feature set</span><code title={manifest.lineage.feature_set_id}>{shortHash(manifest.lineage.feature_set_id)}</code><span>Manifest SHA-256</span><code title={preview.manifest_sha256}>{shortHash(preview.manifest_sha256)}</code><span>Output SHA-256</span><code title={manifest.output_sha256}>{shortHash(manifest.output_sha256)}</code></div>
    <div className="preview-actions"><button className="button button-quiet" type="button" onClick={onCancel} disabled={busy}><X size={15} />Cancel preview</button><button className="button button-primary" type="button" onClick={onConfirm} disabled={busy}><FileCheck2 size={16} />{busy ? "Freezing decision set…" : "Freeze and register decision set"}</button></div>
  </section>;
}

function DecisionSetDetails({ entry, onClose }: { entry: DecisionSetCatalogEntry; onClose: () => void }) {
  const manifest = entry.manifest;
  return <section className="dataset-details" aria-label="Decision set integrity and lineage">
    <div className="details-heading"><div><div className="eyebrow">DECISION SET INSPECTION</div><h3><code>{shortHash(entry.decision_set_id)}</code></h3></div><button type="button" className="icon-button" aria-label="Close decision set inspection" onClick={onClose}><X size={15} /></button></div>
    <div className="integrity-list"><Integrity value={entry.integrity.manifest_valid} label="Manifest and identity" /><Integrity value={entry.integrity.output_valid} label="Output SHA-256" /><Integrity value={entry.integrity.lineage_valid} label="Feature-set lineage" /></div>
    {entry.diagnostic && <p className="integrity-diagnostic" role="alert">{entry.diagnostic}</p>}
    <div className="details-meta"><span>Decision set ID <code>{entry.decision_set_id}</code></span><span>Feature set <code>{entry.feature_set_id}</code></span><span>Manifest SHA-256 <code>{entry.manifest_sha256}</code></span>{manifest && <><span>Output SHA-256 <code>{manifest.output_sha256}</code></span><span>Parameters <code>{Object.entries(manifest.identity.strategy.parameters).map(([name, value]) => `${name}=${value}`).join(" ")}</code></span><span><Clock3 size={12} /> Created <code>{manifest.created_at_utc}</code></span></>}</div>
    {manifest && <div className="series-table-wrap detail-series"><table className="series-table"><thead><tr><th>Role</th><th>Source</th><th>Feature</th><th>Output</th></tr></thead><tbody>{manifest.identity.strategy.bindings.map((binding) => <tr key={binding.role}><td>{binding.role}</td><td>{binding.source}</td><td>{binding.feature_key}</td><td>{binding.output}</td></tr>)}</tbody></table></div>}
  </section>;
}

function Integrity({ value, label }: { value: boolean; label: string }) {
  return <span className={value ? "integrity-valid" : "integrity-invalid"}>{value ? <Check size={13} /> : <AlertTriangle size={13} />}{label}: {value ? "verified" : "mismatch"}</span>;
}

function Stat({ label, value }: { label: string; value: string }) {
  return <div className="preview-stat"><span>{label}</span><strong>{value}</strong></div>;
}
