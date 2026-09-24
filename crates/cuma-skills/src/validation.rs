//! Skill security validation.

use cuma_core::ports::{SkillManifest, TrustLevel};
use serde::{Deserialize, Serialize};

/// Permission patterns that never install without an explicit operator decision.
///
/// These are not "suspicious"; they are the specific things that turn a skill
/// from "helps with Rust" into "has your machine".
const DANGEROUS_PERMISSIONS: &[&str] = &[
    "shell:unrestricted",
    "filesystem:write:/",
    "filesystem:write:~",
    "network:unrestricted",
    "env:read:*",
    "process:spawn:*",
    "credentials",
    "keychain",
];

/// Substrings in a permission that indicate an attempt to escape its scope.
const ESCAPE_PATTERNS: &[&str] = &["..", "$(", "`", "\n", "\r", "\0"];

/// The result of validating a skill before installation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    /// Whether the skill may be installed at all.
    pub permitted: bool,
    /// The trust level validation concluded, which may be lower than claimed.
    pub trust: TrustLevel,
    /// Reasons the skill was refused.
    pub blockers: Vec<String>,
    /// Concerns that did not block installation but should be shown.
    pub warnings: Vec<String>,
}

impl ValidationReport {
    /// Render the report for a human to read before approving.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Verdict: {} (trust: {:?})\n",
            if self.permitted {
                "may install"
            } else {
                "REFUSED"
            },
            self.trust
        ));

        if !self.blockers.is_empty() {
            out.push_str("Blockers:\n");
            for blocker in &self.blockers {
                out.push_str(&format!("  - {blocker}\n"));
            }
        }

        if !self.warnings.is_empty() {
            out.push_str("Warnings:\n");
            for warning in &self.warnings {
                out.push_str(&format!("  - {warning}\n"));
            }
        }

        out
    }
}

/// Validate a manifest.
///
/// The claimed trust level in the manifest is *evidence*, not a decision:
/// anyone can write `trust = "trusted"` in a file they publish. This function
/// derives trust from what can actually be verified and takes the lower of the
/// two.
pub fn validate(manifest: &SkillManifest) -> ValidationReport {
    validate_with(manifest, &crate::integrity::Evidence::default())
}

/// Assess a skill whose files were fetched and examined.
///
/// Registries label what they list with a provisional trust level for
/// display; that label is theirs, not the skill's, and must not cap what the
/// evidence shows. Evidence alone decides here — which still leaves a
/// generated skill `Untrusted` and an unsigned, undigested one `Community`.
pub fn assess_fetched(
    manifest: &SkillManifest,
    evidence: &crate::integrity::Evidence,
) -> ValidationReport {
    let mut unlabelled = manifest.clone();
    unlabelled.trust = TrustLevel::Trusted;
    validate_with(&unlabelled, evidence)
}

/// Validate a manifest against what its files were shown to be.
///
/// Trust comes from evidence alone: a signature that verifies against a key
/// the operator configured makes a skill `Trusted`; contents matching the
/// digest a TLS registry published make it `Verified`. A manifest's own
/// `checksum` and `signature` fields are claims, and claims earn nothing.
pub fn validate_with(
    manifest: &SkillManifest,
    evidence: &crate::integrity::Evidence,
) -> ValidationReport {
    use crate::integrity::SignatureCheck;

    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    // --- structural sanity ------------------------------------------------
    if manifest.name.trim().is_empty() {
        blockers.push("the manifest has no name".to_owned());
    }

    if manifest.version.trim().is_empty() {
        warnings.push("the manifest declares no version".to_owned());
    }

    if manifest.source.trim().is_empty() {
        blockers.push("the manifest declares no source".to_owned());
    }

    // --- permissions ------------------------------------------------------
    for permission in &manifest.requested_permissions {
        let normalized = permission.trim().to_ascii_lowercase();

        if DANGEROUS_PERMISSIONS
            .iter()
            .any(|d| normalized.starts_with(d))
        {
            blockers.push(format!(
                "requests the unrestricted permission {permission:?}"
            ));
        }

        if ESCAPE_PATTERNS.iter().any(|p| permission.contains(p)) {
            blockers.push(format!(
                "permission {permission:?} contains a scope-escape pattern"
            ));
        }
    }

    if manifest.requested_permissions.len() > 12 {
        warnings.push(format!(
            "requests {} permissions, which is a lot for one skill",
            manifest.requested_permissions.len()
        ));
    }

    // --- integrity --------------------------------------------------------
    let digest_matches = evidence.digest_matches();
    if digest_matches == Some(false) {
        blockers.push(format!(
            "its contents ({}) do not match the digest the registry published ({})",
            evidence.digest.as_deref().unwrap_or("?"),
            evidence.published_digest.as_deref().unwrap_or("?")
        ));
    }

    let signed_by_trusted_key = match &evidence.signature {
        Some(SignatureCheck::Valid { .. }) => true,
        Some(SignatureCheck::Invalid { reason }) => {
            blockers.push(format!("its signature is invalid: {reason}"));
            false
        }
        Some(SignatureCheck::UnknownKey { key }) => {
            warnings.push(format!(
                "signed by {key:?}, which is not in skills.trusted_keys, so the signature proves nothing"
            ));
            false
        }
        Some(SignatureCheck::Absent) | None => {
            warnings.push("no signature, so its origin cannot be verified".to_owned());
            false
        }
    };

    if evidence.published_digest.is_none() {
        warnings
            .push("no digest is published for it, so its integrity cannot be verified".to_owned());
    }

    // --- origin -----------------------------------------------------------
    let source = manifest.source.trim().to_ascii_lowercase();

    // A `generated:` skill was written by the harness itself and never crossed
    // a network, so "was it fetched over TLS" is the wrong question for it.
    // It is local in origin and, separately, the least trustworthy thing in
    // the system — see the trust derivation below.
    let origin_is_generated = source.starts_with("generated:");
    let origin_is_local =
        source.starts_with("builtin:") || source.starts_with("file:") || origin_is_generated;
    let origin_is_secure =
        origin_is_local || source.starts_with("https://") || source.starts_with("git+https://");

    if !origin_is_secure {
        blockers.push(format!(
            "source {:?} is neither local nor fetched over TLS",
            manifest.source
        ));
    }

    // --- derive the trust level ------------------------------------------
    // Trust is earned by verifiable evidence, in this order.
    let derived = if origin_is_generated {
        // Checked before everything else: a skill the harness wrote for itself
        // is never evidence of its own trustworthiness, however it is signed
        // or wherever it claims to come from.
        TrustLevel::Untrusted
    } else if source.starts_with("builtin:") || signed_by_trusted_key {
        TrustLevel::Trusted
    } else if digest_matches == Some(true) && !origin_is_local {
        TrustLevel::Verified
    } else if origin_is_secure {
        TrustLevel::Community
    } else {
        TrustLevel::Untrusted
    };

    // `TrustLevel` orders most-trusted first, so `max` picks the *less*
    // trusted of claimed and derived. A manifest can never talk itself up.
    let trust = derived.max(manifest.trust);

    ValidationReport {
        permitted: blockers.is_empty(),
        trust,
        blockers,
        warnings,
    }
}

/// Whether `trust` clears the configured auto-install bar.
pub fn may_auto_install(trust: TrustLevel, policy: cuma_config::SkillAutoInstall) -> bool {
    match policy {
        cuma_config::SkillAutoInstall::Never => false,
        cuma_config::SkillAutoInstall::TrustedOnly => trust == TrustLevel::Trusted,
        cuma_config::SkillAutoInstall::Verified => {
            matches!(trust, TrustLevel::Trusted | TrustLevel::Verified)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use cuma_core::{CapabilitySet, SkillId};

    fn manifest(source: &str) -> SkillManifest {
        SkillManifest {
            id: SkillId::new("rust-debug"),
            name: "Rust debugging".into(),
            description: "Helps debug Rust".into(),
            version: "1.0.0".into(),
            source: source.into(),
            capabilities: CapabilitySet::new(),
            requested_permissions: vec!["filesystem:read:./src".into()],
            checksum: None,
            signature: None,
            trust: TrustLevel::Community,
        }
    }

    #[test]
    fn a_builtin_skill_is_trusted() {
        let mut m = manifest("builtin:rust-debug");
        m.trust = TrustLevel::Trusted;

        let report = validate(&m);
        assert!(report.permitted);
        assert_eq!(report.trust, TrustLevel::Trusted);
    }

    #[test]
    fn a_builtin_source_alone_does_not_override_a_lower_claim() {
        // Derived trust is `Trusted`, but the manifest says `Community`.
        // The lower of the two wins, so a builtin that marks itself
        // provisional stays provisional.
        let report = validate(&manifest("builtin:rust-debug"));
        assert_eq!(report.trust, TrustLevel::Community);
    }

    #[test]
    fn claimed_checksums_and_signatures_earn_nothing() {
        // The fields a manifest carries are claims. Before this was fixed,
        // having both was enough to be rated Verified.
        let mut m = manifest("https://registry.example/skills/rust-debug");
        m.checksum = Some("sha256:abc".into());
        m.signature = Some("ed25519:def".into());
        m.trust = TrustLevel::Trusted;

        let report = validate(&m);
        assert!(report.permitted);
        assert_eq!(report.trust, TrustLevel::Community);
    }

    fn evidence(
        digest: &str,
        published: Option<&str>,
        signature: crate::integrity::SignatureCheck,
    ) -> crate::integrity::Evidence {
        crate::integrity::Evidence {
            digest: Some(digest.into()),
            published_digest: published.map(Into::into),
            signature: Some(signature),
        }
    }

    #[test]
    fn contents_matching_a_published_digest_are_verified() {
        let mut m = manifest("https://registry.example/skills/rust-debug");
        m.trust = TrustLevel::Trusted;
        let report = validate_with(
            &m,
            &evidence(
                "sha256:aa",
                Some("sha256:aa"),
                crate::integrity::SignatureCheck::Absent,
            ),
        );
        assert!(report.permitted);
        assert_eq!(report.trust, TrustLevel::Verified);
    }

    #[test]
    fn contents_not_matching_the_published_digest_are_refused() {
        let report = validate_with(
            &manifest("https://registry.example/skills/rust-debug"),
            &evidence(
                "sha256:aa",
                Some("sha256:bb"),
                crate::integrity::SignatureCheck::Absent,
            ),
        );
        assert!(!report.permitted);
        assert!(report.blockers[0].contains("do not match"));
    }

    #[test]
    fn a_signature_by_a_configured_key_makes_a_skill_trusted() {
        let mut m = manifest("git+https://github.com/acme/skills#rust-debug");
        m.trust = TrustLevel::Trusted;
        let report = validate_with(
            &m,
            &evidence(
                "sha256:aa",
                None,
                crate::integrity::SignatureCheck::Valid { key: "acme".into() },
            ),
        );
        assert_eq!(report.trust, TrustLevel::Trusted);
    }

    #[test]
    fn an_invalid_signature_is_a_blocker_not_a_downgrade() {
        let report = validate_with(
            &manifest("https://registry.example/skills/rust-debug"),
            &evidence(
                "sha256:aa",
                None,
                crate::integrity::SignatureCheck::Invalid {
                    reason: "tampered".into(),
                },
            ),
        );
        assert!(!report.permitted);
    }

    #[test]
    fn a_generated_skill_stays_untrusted_whatever_its_evidence() {
        let mut m = manifest("generated:rust-debug");
        m.trust = TrustLevel::Trusted;
        let report = validate_with(
            &m,
            &evidence(
                "sha256:aa",
                Some("sha256:aa"),
                crate::integrity::SignatureCheck::Valid { key: "acme".into() },
            ),
        );
        assert_eq!(report.trust, TrustLevel::Untrusted);
    }

    #[test]
    fn an_unsigned_skill_over_tls_is_only_community_trust() {
        let report = validate(&manifest("https://registry.example/skills/x"));
        assert!(report.permitted);
        assert_eq!(report.trust, TrustLevel::Community);
        assert!(report.warnings.iter().any(|w| w.contains("signature")));
    }

    #[test]
    fn a_manifest_cannot_talk_itself_up() {
        let mut m = manifest("https://registry.example/skills/x");
        m.trust = TrustLevel::Trusted; // claimed, not earned

        let report = validate(&m);
        assert_eq!(
            report.trust,
            TrustLevel::Community,
            "trust must be derived from evidence, not from the manifest's own claim"
        );
    }

    #[test]
    fn a_manifest_that_declares_itself_untrusted_is_believed() {
        let mut m = manifest("builtin:x");
        m.trust = TrustLevel::Untrusted;

        // Downwards, a claim is always honoured.
        assert_eq!(validate(&m).trust, TrustLevel::Untrusted);
    }

    #[test]
    fn a_cleartext_source_is_refused() {
        let report = validate(&manifest("http://registry.example/skills/x"));
        assert!(!report.permitted);
        assert!(report.blockers.iter().any(|b| b.contains("TLS")));
    }

    #[test]
    fn unrestricted_permissions_are_refused() {
        for dangerous in [
            "shell:unrestricted",
            "filesystem:write:/",
            "network:unrestricted",
            "credentials",
            "keychain",
        ] {
            let mut m = manifest("builtin:x");
            m.requested_permissions = vec![dangerous.into()];

            let report = validate(&m);
            assert!(!report.permitted, "{dangerous} should have been refused");
        }
    }

    #[test]
    fn a_permission_attempting_to_escape_its_scope_is_refused() {
        for escape in [
            "filesystem:read:./src/../../../etc",
            "shell:run:$(whoami)",
            "shell:run:`id`",
            "filesystem:read:a\nb",
        ] {
            let mut m = manifest("builtin:x");
            m.requested_permissions = vec![escape.into()];

            assert!(
                !validate(&m).permitted,
                "{escape:?} should have been refused"
            );
        }
    }

    #[test]
    fn an_ordinary_scoped_permission_is_fine() {
        let mut m = manifest("builtin:x");
        m.requested_permissions = vec!["filesystem:read:./src".into(), "shell:run:cargo".into()];
        assert!(validate(&m).permitted);
    }

    #[test]
    fn a_manifest_missing_its_essentials_is_refused() {
        let mut m = manifest("builtin:x");
        m.name = "  ".into();
        assert!(!validate(&m).permitted);

        let mut m = manifest("");
        m.source = String::new();
        assert!(!validate(&m).permitted);
    }

    #[test]
    fn an_unusual_number_of_permissions_warns_without_blocking() {
        let mut m = manifest("builtin:x");
        m.requested_permissions = (0..20)
            .map(|i| format!("filesystem:read:./dir{i}"))
            .collect();

        let report = validate(&m);
        assert!(report.permitted);
        assert!(report.warnings.iter().any(|w| w.contains("a lot")));
    }

    #[test]
    fn the_default_policy_auto_installs_only_trusted_skills() {
        use cuma_config::SkillAutoInstall::TrustedOnly;

        assert!(may_auto_install(TrustLevel::Trusted, TrustedOnly));
        assert!(!may_auto_install(TrustLevel::Verified, TrustedOnly));
        assert!(!may_auto_install(TrustLevel::Community, TrustedOnly));
        assert!(!may_auto_install(TrustLevel::Untrusted, TrustedOnly));
    }

    #[test]
    fn the_never_policy_auto_installs_nothing_at_all() {
        use cuma_config::SkillAutoInstall::Never;
        for trust in [
            TrustLevel::Trusted,
            TrustLevel::Verified,
            TrustLevel::Community,
            TrustLevel::Untrusted,
        ] {
            assert!(!may_auto_install(trust, Never));
        }
    }

    #[test]
    fn the_verified_policy_admits_trusted_and_verified_but_no_further() {
        use cuma_config::SkillAutoInstall::Verified as Policy;

        assert!(may_auto_install(TrustLevel::Trusted, Policy));
        assert!(may_auto_install(TrustLevel::Verified, Policy));
        assert!(!may_auto_install(TrustLevel::Community, Policy));
    }

    #[test]
    fn a_generated_skill_is_local_in_origin_but_never_trusted() {
        // It never crossed a network, so TLS is the wrong question — and it is
        // still the least trustworthy thing in the system.
        let mut m = manifest("generated:anthropic");
        m.trust = TrustLevel::Trusted; // claimed, and ignored

        let report = validate(&m);
        assert!(report.permitted, "blockers: {:?}", report.blockers);
        assert_eq!(report.trust, TrustLevel::Untrusted);
    }

    #[test]
    fn a_generated_skill_stays_untrusted_even_when_signed() {
        let mut m = manifest("generated:anthropic");
        m.checksum = Some("sha256:abc".into());
        m.signature = Some("ed25519:def".into());

        assert_eq!(validate(&m).trust, TrustLevel::Untrusted);
    }

    #[test]
    fn a_generated_skill_never_clears_any_auto_install_bar() {
        for policy in [
            cuma_config::SkillAutoInstall::Never,
            cuma_config::SkillAutoInstall::TrustedOnly,
            cuma_config::SkillAutoInstall::Verified,
        ] {
            assert!(
                !may_auto_install(validate(&manifest("generated:x")).trust, policy),
                "a generated skill must always need explicit approval"
            );
        }
    }

    #[test]
    fn a_report_explains_itself_to_a_human() {
        let report = validate(&manifest("http://registry.example/x"));
        let text = report.render();
        assert!(text.contains("REFUSED"));
        assert!(text.contains("Blockers:"));
    }
}
