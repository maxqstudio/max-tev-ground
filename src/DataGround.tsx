import { useCallback, useEffect, useState, type FormEvent } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  AlertTriangle,
  Check,
  ChevronRight,
  CircleHelp,
  Database,
  FileCheck2,
  FileUp,
  RefreshCw,
  ShieldCheck,
  X,
} from "lucide-react";
import type {
  DatasetCatalogEntry,
  DatasetStatus,
  FieldMapping,
  ImportAttempt,
  ImportCommitResult,
  ImportPreview,
  ImportPreviewSession,
  ImportRequest,
  MarketRole,
  SelectedSource,
  SourceInspection,
} from "./data-ground-types";

const SAFE_INTEGER_LIMIT = Number.MAX_SAFE_INTEGER;

interface FormState {
  mapping: FieldMapping;
  timestampSemantics: ImportRequest["timestamp_semantics"];
  utcOffsetMinutes: string;
  defaultSymbol: string;
  defaultTimeframe: string;
  defaultMarketRole: MarketRole;
  marketRoles: string;
  sourceId: string;
  broker: string;
  server: string;
  delimiter: ImportRequest["delimiter"];
  calendar: "UNKNOWN" | "CONTINUOUS" | "WEEKENDS_UTC" | "CUSTOM";
  closedIntervals: string;
  maxSourceBytes: string;
  maxRecords: string;
}

const emptyMapping: FieldMapping = {
  timestamp: "",
  open: "",
  high: "",
  low: "",
  close: "",
  volume: "",
  symbol: null,
  timeframe: null,
};

function initialForm(): FormState {
  return {
    mapping: emptyMapping,
    timestampSemantics: "BAR_OPENS_AT",
    utcOffsetMinutes: "0",
    defaultSymbol: "",
    defaultTimeframe: "H1",
    defaultMarketRole: "TARGET",
    marketRoles: "",
    sourceId: "local-file-import",
    broker: "",
    server: "",
    delimiter: "COMMA",
    calendar: "UNKNOWN",
    closedIntervals: "",
    maxSourceBytes: String(5 * 1024 * 1024 * 1024),
    maxRecords: "50000000",
  };
}

function guessHeader(headers: string[], names: string[]): string {
  const candidates = new Map(headers.map((header) => [header.toLowerCase().replaceAll(/[^a-z0-9]/g, ""), header]));
  for (const name of names) {
    const match = candidates.get(name.toLowerCase().replaceAll(/[^a-z0-9]/g, ""));
    if (match) return match;
  }
  return "";
}

function suggestedForm(inspection: SourceInspection): FormState {
  const headers = inspection.headers;
  const mapping: FieldMapping = {
    timestamp: guessHeader(headers, ["timestamp", "time", "datetime", "date"]),
    open: guessHeader(headers, ["open", "o"]),
    high: guessHeader(headers, ["high", "h"]),
    low: guessHeader(headers, ["low", "l"]),
    close: guessHeader(headers, ["close", "c"]),
    volume: guessHeader(headers, ["volume", "tickvolume", "vol"]),
    symbol: guessHeader(headers, ["symbol", "market", "instrument", "ticker"]) || null,
    timeframe: guessHeader(headers, ["timeframe", "period", "tf"]) || null,
  };
  const sample = inspection.sample_rows[0] ?? {};
  return {
    ...initialForm(),
    mapping,
    defaultSymbol: mapping.symbol ? sample[mapping.symbol] ?? "" : "",
    defaultTimeframe: mapping.timeframe ? sample[mapping.timeframe] ?? "H1" : "H1",
  };
}

function parseMarketRoles(value: string): Record<string, MarketRole> {
  const roles: Record<string, MarketRole> = {};
  for (const [index, rawLine] of value.split(/\r?\n/).entries()) {
    const line = rawLine.trim();
    if (!line) continue;
    const separator = line.lastIndexOf("=");
    if (separator < 1) throw new Error(`Market role line ${index + 1} must use SYMBOL=TARGET or SYMBOL=REFERENCE.`);
    const symbol = line.slice(0, separator).trim();
    const role = line.slice(separator + 1).trim().toUpperCase();
    if (!symbol || (role !== "TARGET" && role !== "REFERENCE")) {
      throw new Error(`Market role line ${index + 1} must use SYMBOL=TARGET or SYMBOL=REFERENCE.`);
    }
    roles[symbol] = role;
  }
  return roles;
}

function buildRequest(form: FormState): ImportRequest {
  const maxSourceBytes = Number(form.maxSourceBytes);
  const maxRecords = Number(form.maxRecords);
  const utcOffsetMinutes = Number(form.utcOffsetMinutes);
  if (!Number.isSafeInteger(maxSourceBytes) || maxSourceBytes <= 0 || !Number.isSafeInteger(maxRecords) || maxRecords <= 0) {
    throw new Error("Resource quotas must be positive safe integers.");
  }
  if (!Number.isInteger(utcOffsetMinutes) || utcOffsetMinutes < -840 || utcOffsetMinutes > 840) {
    throw new Error("Naive timestamp offset must be whole minutes between -840 and 840.");
  }
  if (!form.defaultSymbol.trim() || !form.defaultTimeframe.trim() || !form.broker.trim() || !form.server.trim()) {
    throw new Error("Default symbol, timeframe, broker, and server are required for source provenance.");
  }
  let calendar: ImportRequest["calendar"];
  switch (form.calendar) {
    case "UNKNOWN":
      calendar = { kind: "UNKNOWN" };
      break;
    case "CONTINUOUS":
      calendar = { kind: "CONTINUOUS" };
      break;
    case "WEEKENDS_UTC":
      calendar = { kind: "WEEKENDS_UTC" };
      break;
    case "CUSTOM": {
      const intervals = form.closedIntervals.split(/\r?\n/).flatMap((rawLine, index) => {
        const line = rawLine.trim();
        if (!line) return [];
        const values = line.split(",").map((value) => value.trim());
        if (values.length !== 2 || !values[0] || !values[1]) {
          throw new Error(`Closed interval line ${index + 1} must contain start,end timestamps.`);
        }
        return [{ start_utc: values[0], end_utc: values[1] }];
      });
      if (intervals.length === 0) throw new Error("Add at least one custom closed interval, or choose another calendar assumption.");
      calendar = { kind: "CUSTOM_CLOSED_INTERVALS", intervals };
      break;
    }
  }
  return {
    mapping: form.mapping,
    timestamp_semantics: form.timestampSemantics,
    naive_timestamp_offset_minutes: utcOffsetMinutes,
    default_symbol: form.defaultSymbol.trim(),
    default_timeframe: form.defaultTimeframe.trim(),
    default_market_role: form.defaultMarketRole,
    market_roles: parseMarketRoles(form.marketRoles),
    source_id: form.sourceId.trim() || "local-file-import",
    broker: form.broker.trim(),
    server: form.server.trim(),
    delimiter: form.delimiter,
    calendar,
    quotas: { max_source_bytes: maxSourceBytes, max_records: maxRecords },
  };
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let size = bytes / 1024;
  let unit = units[0];
  for (let index = 0; size >= 1024 && index < units.length - 1; index += 1) {
    size /= 1024;
    unit = units[index + 1];
  }
  return `${size.toLocaleString(undefined, { maximumFractionDigits: 1 })} ${unit}`;
}

function shortHash(value: string): string {
  return value.length > 22 ? `${value.slice(0, 12)}…${value.slice(-8)}` : value;
}

function newJobId(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function displayStatus(status: DatasetStatus): string {
  return status.replaceAll("_", " ");
}

export default function DataGround() {
  const [datasets, setDatasets] = useState<DatasetCatalogEntry[]>([]);
  const [attempts, setAttempts] = useState<ImportAttempt[]>([]);
  const [selectedSource, setSelectedSource] = useState<SelectedSource | null>(null);
  const [inspection, setInspection] = useState<SourceInspection | null>(null);
  const [form, setForm] = useState<FormState>(initialForm);
  const [preview, setPreview] = useState<ImportPreviewSession | null>(null);
  const [selectedDataset, setSelectedDataset] = useState<DatasetCatalogEntry | null>(null);
  const [busy, setBusy] = useState<"loading" | "selecting" | "inspecting" | "previewing" | "confirming" | "cancelling" | null>("loading");
  const [activeJobId, setActiveJobId] = useState<string | null>(null);
  const [previewPending, setPreviewPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const refreshRegistry = useCallback(async () => {
    const [nextDatasets, nextAttempts] = await Promise.all([
      invoke<DatasetCatalogEntry[]>("list_datasets"),
      invoke<ImportAttempt[]>("list_import_attempts"),
    ]);
    setDatasets(nextDatasets);
    setAttempts(nextAttempts);
    return nextDatasets;
  }, []);

  useEffect(() => {
    let current = true;
    Promise.all([
      invoke<DatasetCatalogEntry[]>("list_datasets"),
      invoke<ImportAttempt[]>("list_import_attempts"),
    ]).then(([nextDatasets, nextAttempts]) => {
      if (!current) return;
      setDatasets(nextDatasets);
      setAttempts(nextAttempts);
      setBusy(null);
    }).catch((reason: unknown) => {
      if (!current) return;
      setError(reason instanceof Error ? reason.message : String(reason));
      setBusy(null);
    });
    return () => { current = false; };
  }, []);

  function updateForm<K extends keyof FormState>(key: K, value: FormState[K]) {
    setForm((current) => ({ ...current, [key]: value }));
  }

  function updateMapping<K extends keyof FieldMapping>(key: K, value: FieldMapping[K]) {
    setForm((current) => ({ ...current, mapping: { ...current.mapping, [key]: value } }));
  }

  async function changeDelimiter(value: string) {
    const delimiter = value as FormState["delimiter"];
    setForm((current) => ({ ...current, delimiter }));
    if (!selectedSource || !selectedSource.fileName.toLowerCase().endsWith(".csv")) return;

    setError(null);
    setNotice(null);
    setBusy("inspecting");
    try {
      const inspected = await invoke<{ selectionId: string; inspection: SourceInspection }>("inspect_selected_source", {
        selectionId: selectedSource.selectionId,
        maxSourceBytes: SAFE_INTEGER_LIMIT,
        delimiter,
      });
      setInspection(inspected.inspection);
      const suggested = suggestedForm(inspected.inspection);
      setForm((current) => ({ ...current, delimiter, mapping: suggested.mapping }));
      setPreview(null);
      setNotice("CSV headers re-inspected with the selected delimiter; suggested column mappings were refreshed.");
    } catch (reason) {
      setForm((current) => ({ ...current, delimiter: form.delimiter }));
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(null);
    }
  }

  async function chooseSource() {
    setError(null);
    setNotice(null);
    setBusy("selecting");
    try {
      const selected = await invoke<SelectedSource | null>("select_data_source");
      if (!selected) return;
      const inspected = await invoke<{ selectionId: string; inspection: SourceInspection }>("inspect_selected_source", {
        selectionId: selected.selectionId,
        maxSourceBytes: SAFE_INTEGER_LIMIT,
        delimiter: form.delimiter,
      });
      setSelectedSource(selected);
      setInspection(inspected.inspection);
      setForm({ ...suggestedForm(inspected.inspection), delimiter: form.delimiter });
      setPreview(null);
      setActiveJobId(null);
      setPreviewPending(false);
      setSelectedDataset(null);
      setNotice(`${selected.fileName} inspected. Check the field mapping and time contract before preview.`);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(null);
    }
  }

  async function createPreview(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!selectedSource) return;
    setError(null);
    setNotice(null);
    let request: ImportRequest;
    try {
      request = buildRequest(form);
      const requiredFields = [request.mapping.timestamp, request.mapping.open, request.mapping.high, request.mapping.low, request.mapping.close, request.mapping.volume];
      if (requiredFields.some((field) => !field)) throw new Error("Map timestamp, open, high, low, close, and volume before preview.");
      const duplicate = new Set(requiredFields).size !== requiredFields.length;
      if (duplicate) throw new Error("Each timestamp/OHLC/volume field must map to a different source column.");
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
      return;
    }

    const jobId = newJobId();
    setActiveJobId(jobId);
    setPreviewPending(true);
    setBusy("previewing");
    try {
      const result = await invoke<ImportPreviewSession>("preview_data_import", {
        jobId,
        selectionId: selectedSource.selectionId,
        request,
      });
      setPreview(result);
      setNotice(null);
    } catch (reason) {
      setActiveJobId(null);
      const message = reason instanceof Error ? reason.message : String(reason);
      if (message.toLowerCase().includes("cancel")) setNotice("Import cancelled. No dataset was added to the registry.");
      else setError(message);
      try { await refreshRegistry(); } catch { /* retain the import diagnostic */ }
    } finally {
      setPreviewPending(false);
      setBusy(null);
    }
  }

  async function cancelPreview() {
    if (!activeJobId) return;
    setError(null);
    setBusy("cancelling");
    try {
      const result = await invoke<string>("cancel_data_import", { jobId: activeJobId });
      if (result === "CANCELLATION_REQUESTED") {
        setNotice("Cancellation requested. The importer will stop at its next safe checkpoint.");
      } else {
        setPreview(null);
        setActiveJobId(null);
        setNotice("Preview cancelled. No dataset was added to the registry.");
        await refreshRegistry();
      }
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(null);
    }
  }

  async function confirmPreview() {
    if (!preview) return;
    setError(null);
    setBusy("confirming");
    try {
      const result = await invoke<ImportCommitResult>("confirm_data_import", { jobId: preview.jobId });
      const nextDatasets = await refreshRegistry();
      setPreview(null);
      setActiveJobId(null);
      setSelectedSource(null);
      setInspection(null);
      setSelectedDataset(nextDatasets.find((item) => item.dataset_id === result.dataset_id) ?? null);
      setNotice(`${result.disposition === "REUSED" ? "Existing dataset reused" : "Dataset frozen and registered"}: ${result.dataset_id} · ${displayStatus(result.status)}.`);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
      try { await refreshRegistry(); } catch { /* retain the commit diagnostic */ }
    } finally {
      setBusy(null);
    }
  }

  async function inspectDataset(datasetId: string) {
    setError(null);
    try {
      const entry = await invoke<DatasetCatalogEntry | null>("inspect_dataset", { datasetId });
      if (!entry) throw new Error("Dataset no longer exists in the registry.");
      setSelectedDataset(entry);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    }
  }

  return (
    <div className="page-content data-ground-page">
      <div className="page-heading data-ground-heading">
        <div>
          <div className="eyebrow">RG-R01 · DATA GROUND</div>
          <h1>Data Ground</h1>
          <p className="page-intro">Import and freeze local market history with an explicit time contract and integrity audit.</p>
        </div>
        <div className="data-ground-heading-actions">
          <SelectField label="CSV delimiter" value={form.delimiter} onChange={changeDelimiter} disabled={busy !== null || Boolean(preview)} options={[["COMMA", "Comma"], ["SEMICOLON", "Semicolon"], ["TAB", "Tab"]]} />
          <button className="button button-primary" type="button" onClick={chooseSource} disabled={busy !== null || Boolean(preview)}>
            <FileUp size={16} aria-hidden="true" />
            {busy === "selecting" ? "Opening file picker…" : "Choose CSV or Parquet"}
          </button>
        </div>
      </div>

      {error && <div className="data-alert data-alert-error" role="alert"><AlertTriangle size={16} /><span>{error}</span><button type="button" aria-label="Dismiss error" onClick={() => setError(null)}><X size={15} /></button></div>}
      {notice && <div className="data-alert data-alert-notice" role="status"><Check size={16} /><span>{notice}</span><button type="button" aria-label="Dismiss notice" onClick={() => setNotice(null)}><X size={15} /></button></div>}

      <section className="data-section import-section" aria-labelledby="import-title">
        <div className="data-section-heading">
          <div className="step-badge">01</div>
          <div><h2 id="import-title">Source and import contract</h2><p>Preview validates the full source. Registry is unchanged until you confirm.</p></div>
        </div>
        {selectedSource && inspection ? (
          <>
            <div className="source-summary">
              <div className="file-mark"><FileCheck2 size={19} /></div>
              <div className="source-main"><strong>{selectedSource.fileName}</strong><span>{inspection.extension.toUpperCase()} · {formatBytes(inspection.size_bytes)} · {inspection.headers.length} columns detected</span></div>
              <button type="button" className="button button-quiet" onClick={chooseSource} disabled={busy !== null || Boolean(preview)}>Choose another</button>
            </div>

            <form onSubmit={createPreview}>
              <fieldset className="form-fieldset" disabled={busy !== null || Boolean(preview)}>
                <div className="data-form-section">
                  <h3>Column mapping</h3>
                  <div className="mapping-grid">
                    <FieldSelect label="Timestamp" value={form.mapping.timestamp} headers={inspection.headers} onChange={(value) => updateMapping("timestamp", value)} required />
                    <FieldSelect label="Open" value={form.mapping.open} headers={inspection.headers} onChange={(value) => updateMapping("open", value)} required />
                    <FieldSelect label="High" value={form.mapping.high} headers={inspection.headers} onChange={(value) => updateMapping("high", value)} required />
                    <FieldSelect label="Low" value={form.mapping.low} headers={inspection.headers} onChange={(value) => updateMapping("low", value)} required />
                    <FieldSelect label="Close" value={form.mapping.close} headers={inspection.headers} onChange={(value) => updateMapping("close", value)} required />
                    <FieldSelect label="Volume" value={form.mapping.volume} headers={inspection.headers} onChange={(value) => updateMapping("volume", value)} required />
                    <FieldSelect label="Symbol (optional)" value={form.mapping.symbol ?? ""} headers={inspection.headers} onChange={(value) => updateMapping("symbol", value || null)} optional />
                    <FieldSelect label="Timeframe (optional)" value={form.mapping.timeframe ?? ""} headers={inspection.headers} onChange={(value) => updateMapping("timeframe", value || null)} optional />
                  </div>
                </div>

                <div className="data-form-section">
                  <h3>Market and source identity</h3>
                  <div className="form-grid">
                    <TextField label="Default symbol" value={form.defaultSymbol} onChange={(value) => updateForm("defaultSymbol", value)} placeholder="e.g. EURUSD.m" required />
                    <TextField label="Default timeframe" value={form.defaultTimeframe} onChange={(value) => updateForm("defaultTimeframe", value.toUpperCase())} placeholder="M1, M5, M15, M30, H1, H4, D1, W1" required />
                    <SelectField label="Default market role" value={form.defaultMarketRole} onChange={(value) => updateForm("defaultMarketRole", value as MarketRole)} options={[["TARGET", "Target market"], ["REFERENCE", "Reference market"]]} />
                    <TextField label="Source ID" value={form.sourceId} onChange={(value) => updateForm("sourceId", value)} placeholder="Stable import source label" required />
                    <TextField label="Broker" value={form.broker} onChange={(value) => updateForm("broker", value)} placeholder="Broker or provider" required />
                    <TextField label="Server" value={form.server} onChange={(value) => updateForm("server", value)} placeholder="Broker server / feed" required />
                  </div>
                  <label className="form-field role-map-field"><span>Role overrides <small>One symbol per line: SYMBOL=TARGET or SYMBOL=REFERENCE</small></span><textarea value={form.marketRoles} onChange={(event) => updateForm("marketRoles", event.target.value)} placeholder={"GBPUSD=REFERENCE\nDXY=REFERENCE"} rows={2} /></label>
                </div>

                <div className="data-form-section">
                  <h3>Time, calendar and resource bounds</h3>
                  <div className="form-grid">
                    <SelectField label="Source timestamp represents" value={form.timestampSemantics} onChange={(value) => updateForm("timestampSemantics", value as FormState["timestampSemantics"])} options={[["BAR_OPENS_AT", "Bar open time"], ["BAR_CLOSES_AT", "Bar close time"]]} />
                    <TextField label="Offset for timestamps without zone (minutes)" value={form.utcOffsetMinutes} onChange={(value) => updateForm("utcOffsetMinutes", value)} placeholder="0" type="number" />
                    <SelectField label="Trading calendar assumption" value={form.calendar} onChange={(value) => updateForm("calendar", value as FormState["calendar"])} options={[["UNKNOWN", "Unknown · gaps block dataset"], ["CONTINUOUS", "Continuous market"], ["WEEKENDS_UTC", "Closed weekends (UTC)"], ["CUSTOM", "Custom closed intervals"]]} />
                    <TextField label="Max source bytes" value={form.maxSourceBytes} onChange={(value) => updateForm("maxSourceBytes", value)} type="number" />
                    <TextField label="Max records" value={form.maxRecords} onChange={(value) => updateForm("maxRecords", value)} type="number" />
                  </div>
                  {form.calendar === "CUSTOM" && <label className="form-field role-map-field"><span>Custom closed intervals (UTC) <small>One per line: ISO-8601 start,end; these intervals are recorded in the frozen import manifest.</small></span><textarea aria-label="Custom closed intervals (UTC)" value={form.closedIntervals} onChange={(event) => updateForm("closedIntervals", event.target.value)} placeholder={"2024-12-25T00:00:00Z,2024-12-26T00:00:00Z"} rows={3} /></label>}
                  <p className="form-hint"><CircleHelp size={13} /> Timestamp availability is no earlier than the canonical bar close. Missing bars are audited, never synthesized.</p>
                </div>
              </fieldset>

              {!preview && (
                <div className="form-actions">
                  {previewPending ? <><span className="inline-progress"><span className="loading-indicator" /> {busy === "cancelling" ? "Waiting for the importer to stop…" : "Validating source and building preview…"}</span><button className="button button-quiet" type="button" onClick={cancelPreview} disabled={busy !== null && busy !== "previewing"}><X size={15} />{busy === "cancelling" ? "Cancelling…" : "Cancel import"}</button></> : (
                    <button className="button button-primary" type="submit" disabled={busy !== null}><ShieldCheck size={16} />{busy === "loading" ? "Loading registry…" : "Validate and preview"}</button>
                  )}
                </div>
              )}
            </form>
          </>
        ) : (
          <div className="empty-source">
            <div className="empty-source-icon"><FileUp size={20} /></div>
            <div><strong>{busy === "loading" ? "Loading dataset registry…" : "No source selected"}</strong><p>Choose a local CSV or Parquet file to inspect its columns and configure the import contract.</p></div>
            <button className="button button-secondary" type="button" onClick={chooseSource} disabled={busy !== null}><FileUp size={15} />Choose file</button>
          </div>
        )}
      </section>

      {preview && <PreviewPanel preview={preview.preview} busy={busy === "confirming" || busy === "cancelling"} onConfirm={confirmPreview} onCancel={cancelPreview} />}

      <section className="data-section registry-section" aria-labelledby="registry-title">
        <div className="data-section-heading registry-heading">
          <div className="step-badge">02</div>
          <div><h2 id="registry-title">Dataset registry</h2><p>Registered snapshots are immutable; every read rechecks manifest and file hashes.</p></div>
          <button className="icon-button" type="button" aria-label="Refresh dataset registry" onClick={() => { setBusy("loading"); refreshRegistry().catch((reason: unknown) => setError(String(reason))).finally(() => setBusy(null)); }} disabled={busy !== null}><RefreshCw size={15} /></button>
        </div>
        {datasets.length === 0 ? (
          <div className="empty-registry"><Database size={19} /><span>{busy === "loading" ? "Reading local registry…" : "No datasets registered yet."}</span></div>
        ) : (
          <div className="dataset-table-wrap">
            <table className="dataset-table">
              <thead><tr><th>Dataset</th><th>Status</th><th>Series</th><th>Bars</th><th>Fingerprint</th><th><span className="sr-only">Action</span></th></tr></thead>
              <tbody>{datasets.map((entry) => <tr key={entry.dataset_id}>
                <td><strong>{entry.manifest?.provenance.source_file_name ?? entry.dataset_id}</strong><small>{entry.manifest?.created_at_utc ?? "Created time unavailable"}</small></td>
                <td><StatusBadge status={entry.status} /></td>
                <td>{entry.manifest?.series.length ?? "—"}</td>
                <td>{entry.manifest?.bar_count.toLocaleString() ?? "—"}</td>
                <td><code title={entry.fingerprint}>{shortHash(entry.fingerprint)}</code></td>
                <td><button className="inspect-button" type="button" onClick={() => inspectDataset(entry.dataset_id)}>Inspect <ChevronRight size={14} /></button></td>
              </tr>)}</tbody>
            </table>
          </div>
        )}
        {selectedDataset && <DatasetDetails entry={selectedDataset} onClose={() => setSelectedDataset(null)} />}
      </section>

      <section className="data-section attempts-section" aria-labelledby="attempts-title">
        <div className="data-section-heading"><div className="step-badge step-muted">03</div><div><h2 id="attempts-title">Import history</h2><p>Failed and cancelled attempts remain visible for reproducibility.</p></div></div>
        {attempts.length === 0 ? <div className="empty-registry compact-empty">No import attempts recorded.</div> : <div className="attempt-list">{attempts.slice(0, 8).map((attempt) => <div className="attempt-row" key={attempt.attempt_id}><span className={`attempt-status attempt-${attempt.status.toLowerCase()}`}>{attempt.status.replaceAll("_", " ")}</span><strong>{attempt.source_file_name}</strong><time>{attempt.completed_at_utc}</time>{attempt.diagnostic_message && <small title={attempt.diagnostic_message}>{attempt.diagnostic_code ?? "Diagnostic"}</small>}</div>)}</div>}
      </section>
    </div>
  );
}

function FieldSelect({ label, value, headers, onChange, required = false, optional = false }: { label: string; value: string; headers: string[]; onChange: (value: string) => void; required?: boolean; optional?: boolean }) {
  return <label className="form-field"><span>{label}</span><select value={value} onChange={(event) => onChange(event.target.value)} required={required}><option value="">{optional ? "Not mapped" : "Select a source column"}</option>{headers.map((header) => <option key={header} value={header}>{header}</option>)}</select></label>;
}

function TextField({ label, value, onChange, placeholder, type = "text", required = false }: { label: string; value: string; onChange: (value: string) => void; placeholder?: string; type?: string; required?: boolean }) {
  return <label className="form-field"><span>{label}</span><input type={type} value={value} placeholder={placeholder} onChange={(event) => onChange(event.target.value)} required={required} /></label>;
}

function SelectField({ label, value, onChange, options, disabled = false }: { label: string; value: string; onChange: (value: string) => void; options: Array<[string, string]>; disabled?: boolean }) {
  return <label className="form-field"><span>{label}</span><select value={value} onChange={(event) => onChange(event.target.value)} disabled={disabled}>{options.map(([optionValue, optionLabel]) => <option key={optionValue} value={optionValue}>{optionLabel}</option>)}</select></label>;
}

function PreviewPanel({ preview, busy, onConfirm, onCancel }: { preview: ImportPreview; busy: boolean; onConfirm: () => void; onCancel: () => void }) {
  const audit = preview.quality_audit;
  const gapCount = audit.missing_bars + audit.closed_session_bars + audit.unclassified_gap_bars;
  return <section className="data-section preview-panel" aria-labelledby="preview-title">
    <div className="preview-title-row"><div><div className="eyebrow">IMPORT PREVIEW</div><h2 id="preview-title">Audit complete · {preview.source_file_name}</h2></div><StatusBadge status={preview.status} /></div>
    <div className="preview-stats"><Stat label="Canonical bars" value={preview.bar_count.toLocaleString()} /><Stat label="Market/timeframe series" value={String(preview.series_count)} /><Stat label="Observed gap bars" value={gapCount.toLocaleString()} /></div>
    <div className="audit-summary" role="status">
      {preview.status === "READY" ? <ShieldCheck size={17} /> : <AlertTriangle size={17} />}
      <span>{preview.status === "READY" ? "Quality checks passed for the selected calendar assumption." : "This snapshot has audit findings and will be registered as BLOCKED. It cannot be used as trusted input until corrected."}</span>
    </div>
    <div className="series-table-wrap"><table className="series-table"><thead><tr><th>Market</th><th>Role</th><th>Timeframe</th><th>Bars</th><th>First open UTC</th><th>Last open UTC</th></tr></thead><tbody>{preview.series.map((series) => <tr key={`${series.symbol}-${series.timeframe}`}><td>{series.symbol}</td><td>{series.market_role}</td><td>{series.timeframe}</td><td>{series.bar_count.toLocaleString()}</td><td>{series.first_open_utc}</td><td>{series.last_open_utc}</td></tr>)}</tbody></table></div>
    <div className="audit-counts"><span><b>{audit.missing_bars}</b> missing</span><span><b>{audit.closed_session_bars}</b> closed-session</span><span><b>{audit.unclassified_gap_bars}</b> unclassified</span><span><b>{audit.duplicate_bars}</b> duplicate</span></div>
    {audit.gaps.length > 0 && <div className="gap-list"><strong>Gap samples</strong>{audit.gaps.slice(0, 5).map((gap, index) => <div key={`${gap.symbol}-${gap.timeframe}-${gap.first_missing_open_utc}-${index}`}><span>{gap.symbol} {gap.timeframe}</span><small>{gap.first_missing_open_utc} → {gap.next_observed_open_utc} · {gap.expected_missing_bars} expected</small></div>)}</div>}
    <div className="hash-row"><span>Dataset fingerprint</span><code title={preview.fingerprint}>{shortHash(preview.fingerprint)}</code><span>Source SHA-256</span><code title={preview.source_sha256}>{shortHash(preview.source_sha256)}</code><span>Raw snapshot SHA-256</span><code title={preview.raw_sha256}>{shortHash(preview.raw_sha256)}</code><span>Canonical Parquet SHA-256</span><code title={preview.parquet_sha256}>{shortHash(preview.parquet_sha256)}</code><span>Manifest SHA-256</span><code title={preview.manifest_sha256}>{shortHash(preview.manifest_sha256)}</code></div>
    <div className="preview-actions"><button className="button button-quiet" type="button" onClick={onCancel} disabled={busy}><X size={15} />Cancel preview</button><button className="button button-primary" type="button" onClick={onConfirm} disabled={busy}><FileCheck2 size={16} />{busy ? "Freezing snapshot…" : preview.status === "READY" ? "Freeze and register dataset" : "Register as BLOCKED"}</button></div>
  </section>;
}

function DatasetDetails({ entry, onClose }: { entry: DatasetCatalogEntry; onClose: () => void }) {
  const manifest = entry.manifest;
  return <section className="dataset-details" aria-label="Dataset integrity and details">
    <div className="details-heading"><div><div className="eyebrow">DATASET INSPECTION</div><h3>{manifest?.provenance.source_file_name ?? entry.dataset_id}</h3></div><button type="button" className="icon-button" aria-label="Close dataset inspection" onClick={onClose}><X size={15} /></button></div>
    <div className="integrity-list"><Integrity value={entry.integrity.manifest_valid} label="Manifest SHA-256" /><Integrity value={entry.integrity.raw_source_valid} label="Raw source SHA-256" /><Integrity value={entry.integrity.canonical_parquet_valid} label="Canonical Parquet SHA-256" /></div>
    {entry.diagnostic && <p className="integrity-diagnostic" role="alert">{entry.diagnostic}</p>}
    <div className="details-meta"><span>Dataset ID <code>{entry.dataset_id}</code></span><span>Fingerprint <code>{entry.fingerprint}</code></span><span>Source <code>{manifest?.provenance.source_id ?? "Unavailable"} · {manifest?.provenance.broker ?? ""} / {manifest?.provenance.server ?? ""}</code></span><span>Manifest SHA-256 <code>{entry.manifest_sha256}</code></span><span>Raw source SHA-256 <code>{manifest?.provenance.raw_sha256 ?? "Unavailable"}</code></span><span>Canonical Parquet SHA-256 <code>{manifest?.canonical_bars_sha256 ?? "Unavailable"}</code></span></div>
    {manifest && <div className="series-table-wrap detail-series"><table className="series-table"><thead><tr><th>Market</th><th>Role</th><th>Timeframe</th><th>Bars</th><th>Missing</th><th>Closed</th><th>Unclassified</th></tr></thead><tbody>{manifest.series.map((series) => <tr key={`${series.symbol}-${series.timeframe}`}><td>{series.symbol}</td><td>{series.market_role}</td><td>{series.timeframe}</td><td>{series.bar_count.toLocaleString()}</td><td>{series.missing_bars}</td><td>{series.closed_session_bars}</td><td>{series.unclassified_gap_bars}</td></tr>)}</tbody></table></div>}
  </section>;
}

function Integrity({ value, label }: { value: boolean; label: string }) {
  return <span className={value ? "integrity-valid" : "integrity-invalid"}>{value ? <Check size={13} /> : <AlertTriangle size={13} />}{label}: {value ? "verified" : "mismatch"}</span>;
}

function StatusBadge({ status }: { status: DatasetStatus }) {
  return <span className={`status-pill status-${status.toLowerCase()}`}>{displayStatus(status)}</span>;
}

function Stat({ label, value }: { label: string; value: string }) {
  return <div className="preview-stat"><span>{label}</span><strong>{value}</strong></div>;
}
