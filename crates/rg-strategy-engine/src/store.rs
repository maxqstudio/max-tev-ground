//! Immutable decision-set store: staged evaluation, journaled promotion, crash recovery, and
//! verification of every catalogue read, including the consumed feature set's current state.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::Duration,
};

use chrono::Utc;
use rg_feature_studio::{
    store::{FeatureSetCatalogEntry, FeatureStore},
    FeatureSetStatus, FeatureSource, SeriesSelector,
};
use rg_persistence::{
    clear_pending_decision_set_promotion, complete_pending_decision_set_promotion,
    get_decision_set_metadata, list_decision_attempts, list_decision_set_metadata_after,
    list_pending_decision_set_promotions, open_database, save_decision_attempt,
    save_pending_decision_set_promotion, DecisionAttemptRecord, DecisionSetMetadata,
    PendingDecisionSetPromotion,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    engine::{self, DecisionProgress, PreviewDecision, ProgressPhase},
    spec::{
        hex, is_sha256, validate_request, DecisionLineage, DecisionSemantics, DecisionSetIdentity,
        DecisionSetManifest, DecisionSetRequest, DecisionSetStatus, FeatureSetInputIdentity,
        ValidatedRequest, DECISION_OUTPUT_FILE, DECISION_SET_SCHEMA_VERSION, RESEARCH_NOTICE,
        STRATEGY_ENGINE_ID, STRATEGY_ENGINE_VERSION,
    },
    StrategyError,
};

const CATALOG_PAGE_SIZE: u64 = 32;
const ATTEMPT_HISTORY_LIMIT: u64 = 50;
const MANIFEST_FILE: &str = "manifest.json";
/// Private, digest-verified copy of the consumed feature output. Removed before promotion.
const INPUT_COPY_FILE: &str = "input-features.parquet";

#[derive(Debug)]
pub struct PreparedDecisionSet {
    pub manifest: DecisionSetManifest,
    pub manifest_json: String,
    pub manifest_sha256: String,
    pub preview: DecisionSetPreview,
    pub staging_directory: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct DecisionSetPreview {
    pub decision_set_id: String,
    pub manifest_sha256: String,
    pub manifest: DecisionSetManifest,
    pub sample_rows: Vec<PreviewDecision>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionCommitDisposition {
    Materialized,
    Reused,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DecisionCommitResult {
    pub decision_set_id: String,
    pub status: DecisionSetStatus,
    pub disposition: DecisionCommitDisposition,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DecisionSetIntegrity {
    pub manifest_valid: bool,
    pub output_valid: bool,
    pub lineage_valid: bool,
}

impl DecisionSetIntegrity {
    fn is_valid(&self) -> bool {
        self.manifest_valid && self.output_valid && self.lineage_valid
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DecisionSetCatalogEntry {
    pub decision_set_id: String,
    pub identity_sha256: String,
    pub manifest_sha256: String,
    pub feature_set_id: String,
    pub status: DecisionSetStatus,
    pub manifest: Option<DecisionSetManifest>,
    pub integrity: DecisionSetIntegrity,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CandidateFeature {
    pub key: String,
    pub indicator_id: String,
    pub source: &'static str,
    pub reference: Option<SeriesSelector>,
    pub outputs: Vec<String>,
}

/// A registered feature set as offered to Strategy Studio. Only `eligible` entries may be used.
#[derive(Clone, Debug, Serialize)]
pub struct DecisionInputCandidate {
    pub feature_set_id: String,
    pub status: FeatureSetStatus,
    pub eligible: bool,
    pub reason: Option<String>,
    pub target: Option<SeriesSelector>,
    pub row_count: Option<u64>,
    pub created_at_utc: Option<String>,
    pub features: Vec<CandidateFeature>,
}

pub struct StrategyStore {
    features: Arc<FeatureStore>,
    staging_directory: PathBuf,
    decision_sets_directory: PathBuf,
    connection: Mutex<rusqlite::Connection>,
    operation_lock: Mutex<()>,
}

/// Feature sets re-verified during one store operation, so each is checked at most once.
type FeatureCheck = BTreeMap<String, Option<FeatureSetCatalogEntry>>;

impl StrategyStore {
    /// Opens the decision registry beside an already opened feature store and finishes any
    /// interrupted promotion. Recovery fails closed and preserves evidence on any mismatch.
    pub fn open(
        root: impl AsRef<Path>,
        features: Arc<FeatureStore>,
    ) -> Result<Self, StrategyError> {
        fs::create_dir_all(root.as_ref())?;
        let root = fs::canonicalize(root.as_ref())?;
        let staging_directory = ensure_store_directory(&root, ".decision-staging")?;
        let decision_sets_directory = ensure_store_directory(&root, "decision-sets")?;
        let database_path = root.join("research.sqlite3");
        match fs::symlink_metadata(&database_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            _ => return Err(StrategyError::UnsafeStorePath),
        }
        let store = Self {
            features,
            staging_directory,
            decision_sets_directory,
            connection: Mutex::new(open_database(database_path)?),
            operation_lock: Mutex::new(()),
        };
        store.recover_pending_promotions()?;
        Ok(store)
    }

    pub fn features(&self) -> &Arc<FeatureStore> {
        &self.features
    }

    /// Lists registered feature sets with their re-verified status. Non-READY sets are returned
    /// as ineligible so the UI can explain, not hide, why they cannot be used.
    pub fn list_input_candidates(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Vec<DecisionInputCandidate>, StrategyError> {
        let entries = self.features.list_feature_sets(cancel)?;
        Ok(entries
            .into_iter()
            .map(|entry| {
                let reason = match entry.status {
                    FeatureSetStatus::Ready => None,
                    FeatureSetStatus::Blocked => Some(
                        "Feature set is BLOCKED: at least one feature never became READY".into(),
                    ),
                    FeatureSetStatus::Quarantined => {
                        Some(entry.diagnostic.clone().unwrap_or_else(|| {
                            "Feature set failed integrity or lineage verification".into()
                        }))
                    }
                };
                let manifest = entry.manifest.as_ref();
                DecisionInputCandidate {
                    feature_set_id: entry.feature_set_id.clone(),
                    status: entry.status.clone(),
                    eligible: reason.is_none() && manifest.is_some(),
                    reason,
                    target: manifest.map(|m| m.identity.target.clone()),
                    row_count: manifest.map(|m| m.row_count),
                    created_at_utc: manifest.map(|m| m.created_at_utc.clone()),
                    features: manifest
                        .map(|m| {
                            m.identity
                                .features
                                .iter()
                                .map(|feature| CandidateFeature {
                                    key: feature.key.clone(),
                                    indicator_id: feature.indicator_id.clone(),
                                    source: match feature.source {
                                        FeatureSource::Target => "TARGET",
                                        FeatureSource::Reference { .. } => "REFERENCE",
                                    },
                                    reference: match &feature.source {
                                        FeatureSource::Target => None,
                                        FeatureSource::Reference { series, .. } => {
                                            Some(series.clone())
                                        }
                                    },
                                    outputs: feature.outputs.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            })
            .collect())
    }

    /// Validates the request, re-verifies the feature set, and evaluates decisions into a private
    /// staging directory. Nothing is registered until [`StrategyStore::commit_prepared`].
    pub fn prepare(
        &self,
        request: &DecisionSetRequest,
        cancel: &AtomicBool,
        progress: &DecisionProgress,
    ) -> Result<PreparedDecisionSet, StrategyError> {
        validate_feature_set_id(&request.feature_set_id)?;
        progress.set_phase(ProgressPhase::VerifyingInput, 0);
        let (entry, output_path) = self.consumable(&request.feature_set_id)?;
        let feature_manifest =
            entry
                .manifest
                .as_ref()
                .ok_or_else(|| StrategyError::FeatureSetIntegrity {
                    feature_set_id: request.feature_set_id.clone(),
                    reason: "verified manifest is unavailable".into(),
                })?;
        let validated = validate_request(request, feature_manifest)?;
        if feature_manifest.row_count > validated.quotas.max_rows {
            return Err(StrategyError::ResourceQuota(format!(
                "feature set has {} rows; the decision-row quota is {}",
                feature_manifest.row_count, validated.quotas.max_rows
            )));
        }
        let stage = self.staging_directory.join(Uuid::now_v7().to_string());
        fs::create_dir(&stage)?;
        let result = prepare_in_stage(&validated, &entry, &output_path, &stage, cancel, progress);
        if result.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }

    /// The feature store's downstream boundary, with an unknown feature set reported as missing.
    fn consumable(
        &self,
        feature_set_id: &str,
    ) -> Result<(FeatureSetCatalogEntry, PathBuf), StrategyError> {
        match self.features.consumable_feature_set(feature_set_id) {
            Err(rg_feature_studio::FeatureError::FeatureSetNotUsable { status, .. })
                if status == "NOT_REGISTERED" =>
            {
                Err(StrategyError::FeatureSetMissing(feature_set_id.into()))
            }
            other => other.map_err(Into::into),
        }
    }

    pub fn commit_prepared(
        &self,
        prepared: PreparedDecisionSet,
    ) -> Result<DecisionCommitResult, StrategyError> {
        let _operation = self.operation_guard()?;
        let decision_set_id = prepared.manifest.decision_set_id.clone();
        let result = self.commit_locked(&prepared);
        let pending = self.has_pending_promotion(&decision_set_id).unwrap_or(true);
        if let Err(error) = &result {
            if !pending {
                let status = if matches!(error, StrategyError::Cancelled) {
                    "CANCELLED"
                } else {
                    "FAILED"
                };
                let _ = self.save_attempt(
                    status,
                    Some(&decision_set_id),
                    Some(error.code()),
                    Some(&error.to_string()),
                );
            }
        }
        if !pending && prepared.staging_directory.exists() {
            let _ = fs::remove_dir_all(&prepared.staging_directory);
        }
        result
    }

    fn commit_locked(
        &self,
        prepared: &PreparedDecisionSet,
    ) -> Result<DecisionCommitResult, StrategyError> {
        let stage = verify_prepared(prepared, &self.staging_directory)?;
        let manifest = &prepared.manifest;
        let source = &manifest.identity.feature_set;
        let unchanged = self
            .consumable(&source.feature_set_id)
            .is_ok_and(|(entry, _)| feature_set_matches(&entry, source));
        if !unchanged {
            return Err(StrategyError::FeatureSetIntegrity {
                feature_set_id: source.feature_set_id.clone(),
                reason: "feature set changed or lost READY verification after preview".into(),
            });
        }
        let existing = {
            let connection = self.connection_guard()?;
            get_decision_set_metadata(&connection, &manifest.decision_set_id)?
        };
        if let Some(existing) = existing {
            let entry =
                self.verify_metadata(&existing, &mut FeatureCheck::new(), &AtomicBool::new(false))?;
            if entry.status == DecisionSetStatus::Quarantined {
                return Err(StrategyError::ExistingDecisionSetCorrupt(
                    manifest.decision_set_id.clone(),
                ));
            }
            if existing.output_sha256 != manifest.output_sha256 {
                return Err(StrategyError::NonDeterministicReplay(
                    manifest.decision_set_id.clone(),
                ));
            }
            self.save_attempt(
                "REUSED",
                Some(&manifest.decision_set_id),
                Some("IDENTICAL_IDENTITY"),
                Some("Existing decision set reused after a bit-identical replay"),
            )?;
            let _ = fs::remove_dir_all(&stage);
            return Ok(DecisionCommitResult {
                decision_set_id: manifest.decision_set_id.clone(),
                status: entry.status,
                disposition: DecisionCommitDisposition::Reused,
            });
        }
        let destination = self.decision_sets_directory.join(&manifest.decision_set_id);
        if path_exists(&destination)? {
            return Err(StrategyError::OrphanDirectoryExists);
        }
        let staging_name = stage
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| StrategyError::InvalidPrepared("staging name is invalid".into()))?
            .to_owned();
        let now = timestamp();
        let pending = PendingDecisionSetPromotion {
            staging_name,
            decision_set: metadata_from_prepared(prepared),
            attempt: DecisionAttemptRecord {
                attempt_id: Uuid::now_v7().to_string(),
                started_at_utc: now.clone(),
                completed_at_utc: now,
                status: "MATERIALIZED".into(),
                decision_set_id: Some(manifest.decision_set_id.clone()),
                diagnostic_code: None,
                diagnostic_message: None,
            },
        };
        {
            let connection = self.connection_guard()?;
            save_pending_decision_set_promotion(&connection, &pending)?;
        }
        if let Err(error) = promote_with_bounded_retry(&stage, &destination) {
            if !path_exists(&destination)? {
                let connection = self.connection_guard()?;
                clear_pending_decision_set_promotion(&connection, &manifest.decision_set_id)?;
            }
            return Err(error);
        }
        let mut connection = self.connection_guard()?;
        complete_pending_decision_set_promotion(&mut connection, &manifest.decision_set_id)?;
        Ok(DecisionCommitResult {
            decision_set_id: manifest.decision_set_id.clone(),
            status: manifest.status.clone(),
            disposition: DecisionCommitDisposition::Materialized,
        })
    }

    /// Removes a preview that the Owner did not confirm and records the cancellation.
    pub fn discard_prepared(&self, prepared: PreparedDecisionSet) -> Result<(), StrategyError> {
        let _operation = self.operation_guard()?;
        verify_prepared(&prepared, &self.staging_directory)?;
        fs::remove_dir_all(&prepared.staging_directory)?;
        self.save_attempt(
            "CANCELLED",
            Some(&prepared.manifest.decision_set_id),
            Some("OWNER_CANCELLED_PREVIEW"),
            Some("Decision preview was cancelled before explicit confirmation"),
        )
    }

    pub fn record_failure(
        &self,
        status: &str,
        diagnostic_code: &str,
        diagnostic_message: &str,
    ) -> Result<(), StrategyError> {
        if !matches!(status, "FAILED" | "CANCELLED") {
            return Err(StrategyError::InvalidRequest(
                "only failed or cancelled attempts can be recorded as failures".into(),
            ));
        }
        let _operation = self.operation_guard()?;
        self.save_attempt(
            status,
            None,
            Some(diagnostic_code),
            Some(diagnostic_message),
        )
    }

    pub fn list_attempts(&self) -> Result<Vec<DecisionAttemptRecord>, StrategyError> {
        let connection = self.connection_guard()?;
        Ok(list_decision_attempts(&connection, ATTEMPT_HISTORY_LIMIT)?)
    }

    /// Lists every registered decision set after re-verifying its manifest, output digest, and
    /// the current state of the feature set it consumed. Each feature set is checked once.
    pub fn list_decision_sets(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Vec<DecisionSetCatalogEntry>, StrategyError> {
        let mut check = FeatureCheck::new();
        let mut entries = Vec::new();
        let mut cursor = String::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(StrategyError::Cancelled);
            }
            let page = {
                let connection = self.connection_guard()?;
                list_decision_set_metadata_after(&connection, &cursor, CATALOG_PAGE_SIZE)?
            };
            if page.is_empty() {
                return Ok(entries);
            }
            for metadata in page {
                cursor.clone_from(&metadata.decision_set_id);
                entries.push(self.verify_metadata(&metadata, &mut check, cancel)?);
            }
        }
    }

    /// Re-verifies one registered decision set, including its consumed feature set.
    pub fn inspect_decision_set(
        &self,
        decision_set_id: &str,
    ) -> Result<Option<DecisionSetCatalogEntry>, StrategyError> {
        validate_decision_set_id(decision_set_id)?;
        let metadata = {
            let connection = self.connection_guard()?;
            get_decision_set_metadata(&connection, decision_set_id)?
        };
        metadata
            .map(|metadata| {
                self.verify_metadata(&metadata, &mut FeatureCheck::new(), &AtomicBool::new(false))
            })
            .transpose()
    }

    /// Downstream-use boundary: returns a decision set's output path only when it is registered
    /// and READY after re-verification. QUARANTINED and unknown sets fail closed. Callers must
    /// still compare the file digest with the manifest around every read.
    pub fn output_path(&self, decision_set_id: &str) -> Result<PathBuf, StrategyError> {
        let not_usable = |status: &str| StrategyError::DecisionSetNotUsable {
            decision_set_id: decision_set_id.into(),
            status: status.into(),
        };
        match self.inspect_decision_set(decision_set_id)? {
            Some(entry) if entry.status == DecisionSetStatus::Ready => Ok(self
                .decision_sets_directory
                .join(decision_set_id)
                .join(DECISION_OUTPUT_FILE)),
            Some(entry) => Err(not_usable(entry.status.as_str())),
            None => Err(not_usable("NOT_REGISTERED")),
        }
    }

    fn verify_metadata(
        &self,
        metadata: &DecisionSetMetadata,
        check: &mut FeatureCheck,
        cancel: &AtomicBool,
    ) -> Result<DecisionSetCatalogEntry, StrategyError> {
        let manifest = serde_json::from_str::<DecisionSetManifest>(&metadata.manifest_json).ok();
        let directory = self.decision_sets_directory.join(&metadata.decision_set_id);
        let id_valid = validate_decision_set_id(&metadata.decision_set_id).is_ok();
        let identity_valid = manifest.as_ref().is_some_and(|manifest| {
            manifest.identity_is_valid()
                && manifest.decision_set_id == metadata.decision_set_id
                && manifest.identity_sha256 == metadata.identity_sha256
                && manifest.output_sha256 == metadata.output_sha256
                && manifest.status.as_str() == metadata.status
                && manifest.identity.feature_set.feature_set_id == metadata.feature_set_id
                && manifest.identity.feature_set.manifest_sha256
                    == metadata.feature_set_manifest_sha256
                && manifest
                    .canonical_json()
                    .is_ok_and(|json| json == metadata.manifest_json)
        });
        let manifest_file_valid = id_valid
            && directory_is_regular(&directory)
            && regular_file_bytes(&directory.join(MANIFEST_FILE))?
                .is_some_and(|bytes| bytes == metadata.manifest_json.as_bytes());
        let output_valid = id_valid
            && directory_is_regular(&directory)
            && is_regular_file(&directory.join(DECISION_OUTPUT_FILE))?
            && engine::sha256_file(&directory.join(DECISION_OUTPUT_FILE), cancel)?
                == metadata.output_sha256;
        let lineage_problem = match &manifest {
            Some(manifest) => self.lineage_problem(manifest, check)?,
            None => {
                Some("decision-set manifest is unreadable, so lineage cannot be checked".into())
            }
        };
        let integrity = DecisionSetIntegrity {
            manifest_valid: metadata.manifest_integrity_valid
                && identity_valid
                && manifest_file_valid,
            output_valid,
            lineage_valid: lineage_problem.is_none(),
        };
        let valid = integrity.is_valid();
        let diagnostic = if !integrity.manifest_valid || !integrity.output_valid {
            Some("manifest or decision output digest does not match the immutable registry".into())
        } else {
            lineage_problem
        };
        Ok(DecisionSetCatalogEntry {
            decision_set_id: metadata.decision_set_id.clone(),
            identity_sha256: metadata.identity_sha256.clone(),
            manifest_sha256: metadata.manifest_sha256.clone(),
            feature_set_id: metadata.feature_set_id.clone(),
            status: match (&manifest, valid) {
                (Some(manifest), true) => manifest.status.clone(),
                _ => DecisionSetStatus::Quarantined,
            },
            manifest,
            diagnostic,
            integrity,
        })
    }

    /// Re-verifies the consumed feature set through the feature store (and thereby its source
    /// datasets). Returns why lineage is untrustworthy, or `None` when it is READY and unchanged.
    fn lineage_problem(
        &self,
        manifest: &DecisionSetManifest,
        check: &mut FeatureCheck,
    ) -> Result<Option<String>, StrategyError> {
        let source = &manifest.identity.feature_set;
        if !check.contains_key(&source.feature_set_id) {
            let entry = self.features.inspect_feature_set(&source.feature_set_id)?;
            check.insert(source.feature_set_id.clone(), entry);
        }
        Ok(match &check[&source.feature_set_id] {
            None => Some(format!(
                "feature set {} is no longer registered",
                source.feature_set_id
            )),
            Some(entry) if entry.status != FeatureSetStatus::Ready => Some(format!(
                "feature set {} is {}, not READY{}",
                source.feature_set_id,
                entry.status.as_str(),
                entry
                    .diagnostic
                    .as_deref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            )),
            Some(entry) if !feature_set_matches(entry, source) => Some(format!(
                "feature set {} no longer matches the recorded manifest or output digest",
                source.feature_set_id
            )),
            Some(_) => None,
        })
    }

    fn recover_pending_promotions(&self) -> Result<(), StrategyError> {
        let pending = {
            let connection = self.connection_guard()?;
            list_pending_decision_set_promotions(&connection)?
        };
        for item in pending {
            let id = item.decision_set.decision_set_id.clone();
            validate_decision_set_id(&id)?;
            let blocked = |reason: &str| StrategyError::RecoveryBlocked {
                decision_set_id: id.clone(),
                reason: reason.into(),
            };
            if Uuid::parse_str(&item.staging_name)
                .ok()
                .is_none_or(|uuid| uuid.to_string() != item.staging_name)
            {
                return Err(blocked("journal contains an invalid staging identity"));
            }
            let stage = self.staging_directory.join(&item.staging_name);
            let destination = self.decision_sets_directory.join(&id);
            match (path_exists(&stage)?, path_exists(&destination)?) {
                (true, true) => {
                    return Err(blocked(
                        "both staging and promoted directories exist; evidence was preserved",
                    ))
                }
                (false, true) => {
                    if !snapshot_matches(&destination, &item.decision_set)? {
                        return Err(blocked(
                            "promoted decision set failed journal integrity checks; evidence was preserved",
                        ));
                    }
                }
                (true, false) => {
                    if !snapshot_matches(&stage, &item.decision_set)? {
                        return Err(blocked(
                            "staged decision set failed journal integrity checks; evidence was preserved",
                        ));
                    }
                    promote_with_bounded_retry(&stage, &destination)?;
                    if !snapshot_matches(&destination, &item.decision_set)? {
                        return Err(blocked(
                            "promoted decision set failed post-move integrity checks; evidence was preserved",
                        ));
                    }
                }
                (false, false) => {
                    return Err(blocked(
                        "neither journaled staging nor promoted decision set exists; journal was preserved",
                    ))
                }
            }
            let mut connection = self.connection_guard()?;
            complete_pending_decision_set_promotion(&mut connection, &id)?;
        }
        Ok(())
    }

    fn has_pending_promotion(&self, decision_set_id: &str) -> Result<bool, StrategyError> {
        let connection = self.connection_guard()?;
        Ok(list_pending_decision_set_promotions(&connection)?
            .iter()
            .any(|item| item.decision_set.decision_set_id == decision_set_id))
    }

    fn save_attempt(
        &self,
        status: &str,
        decision_set_id: Option<&str>,
        diagnostic_code: Option<&str>,
        diagnostic_message: Option<&str>,
    ) -> Result<(), StrategyError> {
        let now = timestamp();
        let connection = self.connection_guard()?;
        save_decision_attempt(
            &connection,
            &DecisionAttemptRecord {
                attempt_id: Uuid::now_v7().to_string(),
                started_at_utc: now.clone(),
                completed_at_utc: now,
                status: status.into(),
                decision_set_id: decision_set_id.map(str::to_owned),
                diagnostic_code: diagnostic_code.map(str::to_owned),
                diagnostic_message: diagnostic_message.map(str::to_owned),
            },
        )?;
        Ok(())
    }

    fn connection_guard(&self) -> Result<MutexGuard<'_, rusqlite::Connection>, StrategyError> {
        self.connection.lock().map_err(|_| StrategyError::Poisoned)
    }

    fn operation_guard(&self) -> Result<MutexGuard<'_, ()>, StrategyError> {
        self.operation_lock
            .lock()
            .map_err(|_| StrategyError::Poisoned)
    }

    #[doc(hidden)]
    pub fn staging_directory_for_tests(&self) -> &Path {
        &self.staging_directory
    }
}

fn prepare_in_stage(
    validated: &ValidatedRequest,
    entry: &FeatureSetCatalogEntry,
    feature_output: &Path,
    stage: &Path,
    cancel: &AtomicBool,
    progress: &DecisionProgress,
) -> Result<PreparedDecisionSet, StrategyError> {
    let feature_manifest = entry
        .manifest
        .as_ref()
        .expect("a consumable feature set has a verified manifest");
    let input_copy = stage.join(INPUT_COPY_FILE);
    if !engine::copy_verified(
        feature_output,
        &input_copy,
        &feature_manifest.output_sha256,
        cancel,
    )? {
        return Err(StrategyError::FeatureSetIntegrity {
            feature_set_id: entry.feature_set_id.clone(),
            reason: "feature output bytes changed after verification".into(),
        });
    }
    let materialized = engine::materialize(
        validated,
        feature_manifest,
        &input_copy,
        &stage.join(DECISION_OUTPUT_FILE),
        cancel,
        progress,
    )?;
    fs::remove_file(&input_copy)?;
    let identity = DecisionSetIdentity {
        schema_version: DECISION_SET_SCHEMA_VERSION,
        engine_id: STRATEGY_ENGINE_ID.into(),
        engine_version: STRATEGY_ENGINE_VERSION.into(),
        decision_semantics: DecisionSemantics::current(),
        strategy: validated.strategy.clone(),
        feature_set: FeatureSetInputIdentity {
            feature_set_id: entry.feature_set_id.clone(),
            identity_sha256: feature_manifest.identity_sha256.clone(),
            manifest_sha256: entry.manifest_sha256.clone(),
            output_sha256: feature_manifest.output_sha256.clone(),
            output_fingerprint: feature_manifest.output_fingerprint.clone(),
            row_count: feature_manifest.row_count,
            target: feature_manifest.identity.target.clone(),
        },
    };
    let identity_sha256 = identity.sha256()?;
    let manifest = DecisionSetManifest {
        schema_version: DECISION_SET_SCHEMA_VERSION,
        decision_set_id: format!("dc_{identity_sha256}"),
        identity_sha256,
        identity,
        created_at_utc: timestamp(),
        output_file: DECISION_OUTPUT_FILE.into(),
        output_sha256: materialized.output_sha256,
        output_fingerprint: materialized.output_fingerprint,
        row_count: materialized.row_count,
        columns: materialized.columns,
        summary: materialized.summary,
        lineage: DecisionLineage {
            feature_set_id: entry.feature_set_id.clone(),
            target: feature_manifest.identity.target.clone(),
            context_series: validated.context_series.clone(),
            datasets: feature_manifest.lineage.clone(),
        },
        status: DecisionSetStatus::Ready,
        research_notice: RESEARCH_NOTICE.into(),
    };
    let manifest_json = manifest.canonical_json()?;
    let manifest_sha256 = hex(Sha256::digest(manifest_json.as_bytes()));
    fs::write(stage.join(MANIFEST_FILE), manifest_json.as_bytes())?;
    Ok(PreparedDecisionSet {
        preview: DecisionSetPreview {
            decision_set_id: manifest.decision_set_id.clone(),
            manifest_sha256: manifest_sha256.clone(),
            manifest: manifest.clone(),
            sample_rows: materialized.preview_rows,
        },
        manifest,
        manifest_json,
        manifest_sha256,
        staging_directory: stage.to_path_buf(),
    })
}

/// Whether a re-verified feature set is the exact content a decision set was computed from.
fn feature_set_matches(entry: &FeatureSetCatalogEntry, source: &FeatureSetInputIdentity) -> bool {
    entry.feature_set_id == source.feature_set_id
        && entry.status == FeatureSetStatus::Ready
        && entry.manifest_sha256 == source.manifest_sha256
        && entry.manifest.as_ref().is_some_and(|manifest| {
            manifest.identity_sha256 == source.identity_sha256
                && manifest.output_sha256 == source.output_sha256
                && manifest.output_fingerprint == source.output_fingerprint
                && manifest.row_count == source.row_count
                && manifest.identity.target == source.target
        })
}

fn metadata_from_prepared(prepared: &PreparedDecisionSet) -> DecisionSetMetadata {
    let manifest = &prepared.manifest;
    DecisionSetMetadata {
        decision_set_id: manifest.decision_set_id.clone(),
        identity_sha256: manifest.identity_sha256.clone(),
        feature_set_id: manifest.identity.feature_set.feature_set_id.clone(),
        feature_set_manifest_sha256: manifest.identity.feature_set.manifest_sha256.clone(),
        manifest_json: prepared.manifest_json.clone(),
        manifest_sha256: prepared.manifest_sha256.clone(),
        output_sha256: manifest.output_sha256.clone(),
        created_at_utc: manifest.created_at_utc.clone(),
        status: manifest.status.as_str().into(),
        manifest_integrity_valid: true,
    }
}

/// Checks that a prepared set is internally consistent and that its staged files still match.
/// Returns the canonical staging path.
fn verify_prepared(
    prepared: &PreparedDecisionSet,
    staging_root: &Path,
) -> Result<PathBuf, StrategyError> {
    let manifest = &prepared.manifest;
    if !manifest.identity_is_valid()
        || prepared.manifest_json != manifest.canonical_json()?
        || hex(Sha256::digest(prepared.manifest_json.as_bytes())) != prepared.manifest_sha256
        || prepared.preview.decision_set_id != manifest.decision_set_id
        || prepared.preview.manifest_sha256 != prepared.manifest_sha256
        || prepared.preview.manifest != *manifest
    {
        return Err(StrategyError::InvalidPrepared(
            "manifest, preview, or immutable identity is inconsistent".into(),
        ));
    }
    let canonical_root = fs::canonicalize(staging_root)?;
    let stage = fs::canonicalize(&prepared.staging_directory)?;
    let stage_name_valid = stage
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| {
            Uuid::parse_str(name)
                .ok()
                .map(|uuid| uuid.to_string() == name)
        })
        .unwrap_or(false);
    if stage.parent() != Some(canonical_root.as_path())
        || !directory_is_regular(&stage)
        || !stage_name_valid
        || path_exists(&stage.join(INPUT_COPY_FILE))?
    {
        return Err(StrategyError::InvalidPrepared(
            "staging path is outside the private decision staging directory".into(),
        ));
    }
    let never = AtomicBool::new(false);
    let manifest_matches = regular_file_bytes(&stage.join(MANIFEST_FILE))?
        .is_some_and(|bytes| bytes == prepared.manifest_json.as_bytes());
    let output_matches = is_regular_file(&stage.join(DECISION_OUTPUT_FILE))?
        && engine::sha256_file(&stage.join(DECISION_OUTPUT_FILE), &never)?
            == manifest.output_sha256;
    if !manifest_matches || !output_matches {
        return Err(StrategyError::InvalidPrepared(
            "staged manifest or decision output digest does not match".into(),
        ));
    }
    Ok(stage)
}

fn snapshot_matches(
    directory: &Path,
    metadata: &DecisionSetMetadata,
) -> Result<bool, StrategyError> {
    if !directory_is_regular(directory)
        || !metadata.manifest_integrity_valid
        || validate_decision_set_id(&metadata.decision_set_id).is_err()
        || !is_sha256(&metadata.output_sha256)
    {
        return Ok(false);
    }
    let Some(bytes) = regular_file_bytes(&directory.join(MANIFEST_FILE))? else {
        return Ok(false);
    };
    if bytes != metadata.manifest_json.as_bytes() {
        return Ok(false);
    }
    let Ok(manifest) = serde_json::from_slice::<DecisionSetManifest>(&bytes) else {
        return Ok(false);
    };
    if !manifest.identity_is_valid()
        || manifest.decision_set_id != metadata.decision_set_id
        || manifest.output_sha256 != metadata.output_sha256
        || manifest.canonical_json().ok().as_deref() != Some(metadata.manifest_json.as_str())
    {
        return Ok(false);
    }
    Ok(is_regular_file(&directory.join(DECISION_OUTPUT_FILE))?
        && engine::sha256_file(
            &directory.join(DECISION_OUTPUT_FILE),
            &AtomicBool::new(false),
        )? == metadata.output_sha256)
}

fn ensure_store_directory(root: &Path, name: &str) -> Result<PathBuf, StrategyError> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(StrategyError::UnsafeStorePath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
        Err(error) => return Err(error.into()),
    }
    let canonical = fs::canonicalize(&path)?;
    if canonical.parent() != Some(root) {
        return Err(StrategyError::UnsafeStorePath);
    }
    Ok(canonical)
}

fn promote_with_bounded_retry(source: &Path, destination: &Path) -> Result<(), StrategyError> {
    let mut last_error = None;
    for attempt in 0..3u64 {
        if path_exists(destination)? {
            return Err(StrategyError::OrphanDirectoryExists);
        }
        match fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                ) && attempt < 2 =>
            {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(25 * (attempt + 1)));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(last_error
        .unwrap_or_else(|| std::io::Error::other("bounded promotion retry exhausted"))
        .into())
}

fn path_exists(path: &Path) -> Result<bool, StrategyError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn directory_is_regular(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}

fn is_regular_file(path: &Path) -> Result<bool, StrategyError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn regular_file_bytes(path: &Path) -> Result<Option<Vec<u8>>, StrategyError> {
    if !is_regular_file(path)? {
        return Ok(None);
    }
    Ok(Some(fs::read(path)?))
}

fn validate_feature_set_id(value: &str) -> Result<(), StrategyError> {
    if value.len() != 67 || !value.starts_with("fs_") || !is_sha256(&value[3..]) {
        return Err(StrategyError::InvalidRequest(format!(
            "{value:?} is not a registered feature-set identity (fs_ followed by a lowercase SHA-256)"
        )));
    }
    Ok(())
}

fn validate_decision_set_id(value: &str) -> Result<(), StrategyError> {
    if value.len() != 67 || !value.starts_with("dc_") || !is_sha256(&value[3..]) {
        return Err(StrategyError::InvalidDecisionSetId);
    }
    Ok(())
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
