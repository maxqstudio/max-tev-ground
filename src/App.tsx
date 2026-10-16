import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Activity,
  BrainCircuit,
  ChartNoAxesCombined,
  Database,
  FlaskConical,
  GitBranch,
  ListFilter,
  SlidersHorizontal,
} from "lucide-react";
import type { FoundationSnapshot } from "./types";

const navigation = [
  { id: "overview", label: "Overview", icon: Activity, phase: "RG-R00" },
  { id: "data-ground", label: "Data Ground", icon: Database, phase: "RG-R01" },
  { id: "feature-studio", label: "Feature Studio", icon: SlidersHorizontal, phase: "RG-R02" },
  { id: "strategy-studio", label: "Strategy Studio", icon: GitBranch, phase: "RG-R03" },
  { id: "model-lab", label: "Model Lab", icon: BrainCircuit, phase: "RG-R04" },
  { id: "policy-studio", label: "Policy Studio", icon: ListFilter, phase: "RG-R05" },
  { id: "experiments", label: "Experiments", icon: FlaskConical, phase: "RG-R06" },
  { id: "results", label: "Results", icon: ChartNoAxesCombined, phase: "RG-R07" },
] as const;

const deliverableLabels: Record<string, string> = {
  foundation: "Rust foundation",
  research_contracts: "Research contracts",
  desktop_shell: "Desktop shell",
  persistence: "SQLite persistence",
  governance: "Roadmap governance",
  acceptance_evidence: "Acceptance evidence",
};

function statusLabel(value: string): string {
  return value.replaceAll("_", " ");
}

export default function App() {
  const [activePage, setActivePage] = useState("overview");
  const [snapshot, setSnapshot] = useState<FoundationSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const currentPage = navigation.find((item) => item.id === activePage) ?? navigation[0];

  useEffect(() => {
    let current = true;
    invoke<FoundationSnapshot>("get_foundation_status")
      .then((value) => {
        if (current) setSnapshot(value);
      })
      .catch((reason: unknown) => {
        if (current) setError(reason instanceof Error ? reason.message : String(reason));
      });
    return () => {
      current = false;
    };
  }, []);

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark" aria-hidden="true">M</div>
          <div>
            <div className="brand-name">MAX</div>
            <div className="brand-caption">RESEARCH GROUND</div>
          </div>
        </div>

        <div className="workspace-tag">
          <span className="status-dot" aria-hidden="true" />
          <span>LOCAL WORKSPACE</span>
        </div>

        <nav className="primary-nav" aria-label="Main navigation">
          {navigation.map((item, index) => {
            const Icon = item.icon;
            const planned = item.id !== "overview";
            const itemClass = "nav-item" + (activePage === item.id ? " is-active" : "");
            return (
              <button
                className={itemClass}
                key={item.id}
                type="button"
                aria-current={activePage === item.id ? "page" : undefined}
                onClick={() => setActivePage(item.id)}
              >
                <Icon size={17} strokeWidth={1.8} aria-hidden="true" />
                <span className="nav-label">{item.label}</span>
                {planned ? (
                  <span className="nav-phase">{item.phase}</span>
                ) : (
                  <span className="nav-index">{String(index + 1).padStart(2, "0")}</span>
                )}
              </button>
            );
          })}
        </nav>

        <div className="sidebar-bottom">
          <div className="research-only">
            <span className="research-icon" aria-hidden="true">R</span>
            <span>Research only</span>
          </div>
          <div className="sidebar-phase">
            <span>CURRENT SCOPE</span>
            <strong>RG-R00 <i>Foundation</i></strong>
          </div>
        </div>
      </aside>

      <main className="main-panel">
        <header className="topbar">
          <div className="breadcrumbs">
            <span>Workspace</span>
            <span className="breadcrumb-divider">/</span>
            <strong>{currentPage.label}</strong>
          </div>
          <div className="topbar-state">
            <span className="state-dot" aria-hidden="true" />
            {error ? "Bridge unavailable" : snapshot ? "Local governance" : "Connecting"}
          </div>
        </header>

        {activePage === "overview" ? (
          <Overview snapshot={snapshot} error={error} />
        ) : (
          <PlannedPage
            label={currentPage.label}
            phase={currentPage.phase}
            icon={currentPage.icon}
          />
        )}
      </main>
    </div>
  );
}

function Overview({
  snapshot,
  error,
}: {
  snapshot: FoundationSnapshot | null;
  error: string | null;
}) {
  return (
    <div className="page-content">
      <div className="page-heading">
        <div>
          <div className="eyebrow">RESEARCH WORKSPACE</div>
          <h1>Overview</h1>
          <p className="page-intro">A reproducible foundation for independent trading research.</p>
        </div>
        <div className="phase-chip"><span>{snapshot ? statusLabel(snapshot.phase_lifecycle) : error ? "UNAVAILABLE" : "PHASE"}</span>{snapshot?.current_phase ?? "RG-R00"}</div>
      </div>

      {error ? (
        <section className="connection-state" role="alert">
          <div className="connection-mark">!</div>
          <div>
            <strong>Desktop bridge unavailable</strong>
            <p>The governance snapshot could not be loaded. No project status is being simulated.</p>
            <code>{error}</code>
          </div>
        </section>
      ) : snapshot ? (
        <>
          <section className="status-panel" aria-label="Foundation status">
            <div className="status-panel-heading">
              <div>
                <div className="eyebrow">FOUNDATION STATUS</div>
                <h2>{snapshot.project}</h2>
              </div>
              <span className="status-pill status-active">{statusLabel(snapshot.phase_lifecycle)}</span>
            </div>
            <div className="status-facts">
              <Fact label="Current phase" value={snapshot.current_phase} />
              <Fact label="Builder status" value={statusLabel(snapshot.builder_status)} />
              <Fact label="Automated gates" value={statusLabel(snapshot.automated_status)} />
              <Fact label="Owner UI review" value={statusLabel(snapshot.owner_ui_status)} />
              <Fact label="Owner acceptance" value={statusLabel(snapshot.owner_acceptance)} />
              <Fact label="CI" value={statusLabel(snapshot.ci_status)} />
            </div>
            <div className="status-footnote">
              RG-R01 remains locked until the Owner accepts this checkpoint.
              <span>{snapshot.next_phase_unlocked ? "UNLOCKED" : "LOCKED"}</span>
            </div>
          </section>

          <section className="deliverables-section">
            <div className="section-heading">
              <div>
                <div className="eyebrow">RG-R00</div>
                <h2>Foundation components</h2>
              </div>
              <span className="section-count">{Object.keys(snapshot.deliverables).length} tracked</span>
            </div>
            <div className="deliverable-list">
              {Object.entries(snapshot.deliverables).map(([key, status]) => {
                const statusClass = "status-" + status.toLowerCase().replaceAll("_", "-");
                return (
                  <div className="deliverable-row" key={key}>
                    <span className="deliverable-name">{deliverableLabels[key] ?? key}</span>
                    <span className={"status-pill " + statusClass}>{statusLabel(status)}</span>
                  </div>
                );
              })}
            </div>
          </section>

          <div className="foundation-note">
            <span className="note-mark" aria-hidden="true">i</span>
            <p>
              No market data or trading performance is loaded in this phase. Research results
              require separate, reproducible evidence.
            </p>
          </div>
        </>
      ) : (
        <section className="loading-panel" aria-live="polite">
          <span className="loading-indicator" aria-hidden="true" />
          <span>Loading governance snapshot…</span>
        </section>
      )}
    </div>
  );
}

function Fact({ label, value }: { label: string; value: string }) {
  return (
    <div className="status-fact">
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

function PlannedPage({
  label,
  phase,
  icon: Icon,
}: {
  label: string;
  phase: string;
  icon: typeof Activity;
}) {
  return (
    <div className="page-content planned-content">
      <div className="page-heading">
        <div>
          <div className="eyebrow">RESEARCH MODULE</div>
          <h1>{label}</h1>
          <p className="page-intro">This module is reserved for a later roadmap phase.</p>
        </div>
        <span className="status-pill status-planned">PLANNED</span>
      </div>
      <section className="planned-panel">
        <div className="planned-icon"><Icon size={22} strokeWidth={1.7} aria-hidden="true" /></div>
        <div>
          <div className="eyebrow">{phase}</div>
          <h2>{label} is not implemented yet</h2>
          <p>
            The workspace keeps this destination visible for orientation. It will become
            functional only when its phase is authorized and its acceptance gates are met.
          </p>
        </div>
      </section>
    </div>
  );
}