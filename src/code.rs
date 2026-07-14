use alloy_primitives::{Address, B256, Bytes};
use evm_fork_cache::cache::{CodeMismatch, EvmCache};

use crate::OracleError;

/// How an oracle bytecode entry should be installed into the fork cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleCodeInstallMode {
    /// Seed a canonical bytecode claim and verify it against on-chain code hash.
    VerifyCanonical,
    /// Deliberately etch local simulation bytecode. This is never verified.
    EtchSimulation,
}

/// One bytecode install request for an oracle-related contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleCodeSeed {
    /// Contract address whose code should be installed.
    pub address: Address,
    /// Runtime bytecode.
    pub code: Bytes,
    /// Installation mode.
    pub mode: OracleCodeInstallMode,
}

impl OracleCodeSeed {
    /// Create a canonical seed that must verify against chain code hash.
    pub fn verify(address: Address, code: Bytes) -> Self {
        Self {
            address,
            code,
            mode: OracleCodeInstallMode::VerifyCanonical,
        }
    }

    /// Create an explicit local etch for simulation-only bytecode.
    pub fn etch(address: Address, code: Bytes) -> Self {
        Self {
            address,
            code,
            mode: OracleCodeInstallMode::EtchSimulation,
        }
    }
}

/// Registry of oracle bytecode claims to install before discovery or simulation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OracleCodeRegistry {
    seeds: Vec<OracleCodeSeed>,
}

impl OracleCodeRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a canonical runtime bytecode claim that must verify once.
    pub fn seed(mut self, address: Address, code: Bytes) -> Self {
        self.seeds.push(OracleCodeSeed::verify(address, code));
        self
    }

    /// Add multiple canonical runtime bytecode claims that must verify once.
    pub fn seed_many<I>(mut self, seeds: I) -> Self
    where
        I: IntoIterator<Item = (Address, Bytes)>,
    {
        self.seeds.extend(
            seeds
                .into_iter()
                .map(|(address, code)| OracleCodeSeed::verify(address, code)),
        );
        self
    }

    /// Add an explicit simulation-only code etch.
    pub fn etch(mut self, address: Address, code: Bytes) -> Self {
        self.seeds.push(OracleCodeSeed::etch(address, code));
        self
    }

    /// Add multiple explicit simulation-only code etches.
    pub fn etch_many<I>(mut self, etches: I) -> Self
    where
        I: IntoIterator<Item = (Address, Bytes)>,
    {
        self.seeds.extend(
            etches
                .into_iter()
                .map(|(address, code)| OracleCodeSeed::etch(address, code)),
        );
        self
    }

    /// Append an already-built seed.
    pub fn with_seed(mut self, seed: OracleCodeSeed) -> Self {
        self.seeds.push(seed);
        self
    }

    /// Return true when there is no code work to apply.
    pub fn is_empty(&self) -> bool {
        self.seeds.is_empty()
    }

    /// Return registered seed entries.
    pub fn seeds(&self) -> &[OracleCodeSeed] {
        &self.seeds
    }

    /// Install code entries into the cache and verify canonical seeds.
    pub fn apply_to_cache(&self, cache: &mut EvmCache) -> OracleCodeWarmupReport {
        let mut report = OracleCodeWarmupReport::default();
        let mut has_canonical_seed = false;

        for seed in &self.seeds {
            match seed.mode {
                OracleCodeInstallMode::VerifyCanonical => {
                    has_canonical_seed = true;
                    match cache.seed_account_code(seed.address, seed.code.clone()) {
                        Ok(code_hash) => report.seeded.push(OracleCodeInstall {
                            address: seed.address,
                            code_hash,
                        }),
                        Err(error) => report.install_errors.push(OracleCodeInstallError {
                            address: seed.address,
                            mode: seed.mode,
                            reason: error.to_string(),
                        }),
                    }
                }
                OracleCodeInstallMode::EtchSimulation => {
                    match cache.etch_account_code(seed.address, seed.code.clone()) {
                        Ok(code_hash) => report.etched.push(OracleCodeInstall {
                            address: seed.address,
                            code_hash,
                        }),
                        Err(error) => report.install_errors.push(OracleCodeInstallError {
                            address: seed.address,
                            mode: seed.mode,
                            reason: error.to_string(),
                        }),
                    }
                }
            }
        }

        if has_canonical_seed {
            match cache.verify_code_seeds() {
                Ok(verification) => {
                    report.verified = verification.verified;
                    report.mismatched = verification
                        .mismatched
                        .into_iter()
                        .map(Into::into)
                        .collect();
                    report.not_deployed = verification.not_deployed;
                    report.codeless = verification.codeless;
                    report.unverifiable = verification
                        .unverifiable
                        .into_iter()
                        .map(|(address, reason)| OracleCodeUnverifiable { address, reason })
                        .collect();
                }
                Err(error) => report.verify_error = Some(error.to_string()),
            }
        }

        report
    }

    /// Install code entries, verify canonical seeds, and enforce `policy`.
    pub fn apply_to_cache_with_policy(
        &self,
        cache: &mut EvmCache,
        policy: OracleCodeWarmupPolicy,
    ) -> Result<OracleCodeWarmupReport, OracleError> {
        let report = self.apply_to_cache(cache);
        report.enforce_policy(policy)?;
        Ok(report)
    }
}

/// Failure policy for oracle bytecode warmup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OracleCodeWarmupPolicy {
    /// Fail when installing a seed or etch returns an error.
    pub fail_on_install_error: bool,
    /// Fail when canonical seeded bytecode contradicts on-chain code hash.
    pub fail_on_mismatch: bool,
    /// Fail when a canonical seed points at an address that is not deployed.
    pub fail_on_not_deployed: bool,
    /// Fail when a canonical seed points at an account with empty code.
    pub fail_on_codeless: bool,
    /// Fail when canonical verification cannot run or cannot settle.
    pub fail_on_unverifiable: bool,
    /// Fail when the whole verification pass errors.
    pub fail_on_verify_error: bool,
}

impl OracleCodeWarmupPolicy {
    /// Strict policy for production canonical bytecode claims.
    pub fn strict() -> Self {
        Self {
            fail_on_install_error: true,
            fail_on_mismatch: true,
            fail_on_not_deployed: true,
            fail_on_codeless: true,
            fail_on_unverifiable: true,
            fail_on_verify_error: true,
        }
    }

    /// Allow transient verification uncertainty, but fail on contradictions.
    pub fn allow_unverifiable() -> Self {
        Self {
            fail_on_unverifiable: false,
            fail_on_verify_error: false,
            ..Self::strict()
        }
    }

    /// Best-effort policy that only records report fields.
    pub fn best_effort() -> Self {
        Self {
            fail_on_install_error: false,
            fail_on_mismatch: false,
            fail_on_not_deployed: false,
            fail_on_codeless: false,
            fail_on_unverifiable: false,
            fail_on_verify_error: false,
        }
    }
}

impl Default for OracleCodeWarmupPolicy {
    fn default() -> Self {
        Self::allow_unverifiable()
    }
}

/// One successfully installed bytecode entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleCodeInstall {
    /// Contract address.
    pub address: Address,
    /// Installed bytecode hash.
    pub code_hash: B256,
}

/// Code install failure for one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleCodeInstallError {
    /// Contract address.
    pub address: Address,
    /// Requested installation mode.
    pub mode: OracleCodeInstallMode,
    /// Error message.
    pub reason: String,
}

/// Code-hash mismatch for a canonical seed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleCodeMismatch {
    /// Contract address.
    pub address: Address,
    /// Expected hash from the seeded bytecode.
    pub expected: B256,
    /// Actual on-chain code hash.
    pub actual: B256,
}

impl From<CodeMismatch> for OracleCodeMismatch {
    fn from(value: CodeMismatch) -> Self {
        Self {
            address: value.address,
            expected: value.expected,
            actual: value.actual,
        }
    }
}

/// Canonical code claim that could not be verified due to transport or provider limitations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleCodeUnverifiable {
    /// Contract address.
    pub address: Address,
    /// Reason verification did not settle.
    pub reason: String,
}

/// Report for code seeding, verification, and explicit etching.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OracleCodeWarmupReport {
    /// Canonical code claims installed before verification.
    pub seeded: Vec<OracleCodeInstall>,
    /// Explicit local etches installed.
    pub etched: Vec<OracleCodeInstall>,
    /// Entries that could not be installed.
    pub install_errors: Vec<OracleCodeInstallError>,
    /// Canonical claims verified against chain code hash.
    pub verified: Vec<Address>,
    /// Canonical claims contradicted by chain code hash.
    pub mismatched: Vec<OracleCodeMismatch>,
    /// Addresses not deployed at the pinned block.
    pub not_deployed: Vec<Address>,
    /// Addresses with empty code at the pinned block.
    pub codeless: Vec<Address>,
    /// Canonical claims that could not be verified but remain pending.
    pub unverifiable: Vec<OracleCodeUnverifiable>,
    /// Whole-verification error, if verification itself could not run.
    pub verify_error: Option<String>,
}

impl OracleCodeWarmupReport {
    /// Return true when no code work was requested or performed.
    pub fn is_empty(&self) -> bool {
        self.seeded.is_empty()
            && self.etched.is_empty()
            && self.install_errors.is_empty()
            && self.verified.is_empty()
            && self.mismatched.is_empty()
            && self.not_deployed.is_empty()
            && self.codeless.is_empty()
            && self.unverifiable.is_empty()
            && self.verify_error.is_none()
    }

    /// Enforce a failure policy against this report.
    pub fn enforce_policy(&self, policy: OracleCodeWarmupPolicy) -> Result<(), OracleError> {
        let violation = OracleCodePolicyViolation {
            install_errors: if policy.fail_on_install_error {
                self.install_errors.len()
            } else {
                0
            },
            mismatched: if policy.fail_on_mismatch {
                self.mismatched.len()
            } else {
                0
            },
            not_deployed: if policy.fail_on_not_deployed {
                self.not_deployed.len()
            } else {
                0
            },
            codeless: if policy.fail_on_codeless {
                self.codeless.len()
            } else {
                0
            },
            unverifiable: if policy.fail_on_unverifiable {
                self.unverifiable.len()
            } else {
                0
            },
            verify_error: if policy.fail_on_verify_error {
                self.verify_error.clone()
            } else {
                None
            },
        };
        if violation.is_empty() {
            Ok(())
        } else {
            Err(OracleError::CodePolicy(Box::new(violation)))
        }
    }
}

/// Summary of the code warmup report entries that violated an enforced
/// [`OracleCodeWarmupPolicy`], carried by
/// [`OracleError::CodePolicy`](crate::OracleError::CodePolicy).
///
/// Each count covers only the categories the policy actually enforced; the
/// full per-entry detail remains on the [`OracleCodeWarmupReport`] returned by
/// the `build_report` paths.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OracleCodePolicyViolation {
    /// Enforced install errors.
    pub install_errors: usize,
    /// Enforced code hash mismatches.
    pub mismatched: usize,
    /// Enforced not-deployed addresses.
    pub not_deployed: usize,
    /// Enforced codeless addresses.
    pub codeless: usize,
    /// Enforced unverifiable claims.
    pub unverifiable: usize,
    /// Enforced verification transport error, when one occurred.
    pub verify_error: Option<String>,
}

impl OracleCodePolicyViolation {
    fn is_empty(&self) -> bool {
        self.install_errors == 0
            && self.mismatched == 0
            && self.not_deployed == 0
            && self.codeless == 0
            && self.unverifiable == 0
            && self.verify_error.is_none()
    }
}

impl std::fmt::Display for OracleCodePolicyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut failures = Vec::new();
        if self.install_errors > 0 {
            failures.push(format!("{} install error(s)", self.install_errors));
        }
        if self.mismatched > 0 {
            failures.push(format!("{} code hash mismatch(es)", self.mismatched));
        }
        if self.not_deployed > 0 {
            failures.push(format!("{} not deployed address(es)", self.not_deployed));
        }
        if self.codeless > 0 {
            failures.push(format!("{} codeless address(es)", self.codeless));
        }
        if self.unverifiable > 0 {
            failures.push(format!("{} unverifiable claim(s)", self.unverifiable));
        }
        if let Some(error) = &self.verify_error {
            failures.push(format!("verification error: {error}"));
        }
        write!(
            f,
            "oracle code warmup failed policy: {}",
            failures.join(", ")
        )
    }
}
