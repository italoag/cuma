#![no_main]

use arbitrary::Arbitrary;
use cuma_config::SecurityConfig;
use cuma_core::ports::{SkillManifest, TrustLevel};
use cuma_core::{CapabilitySet, SkillId};
use cuma_skills::integrity::{Evidence, SignatureCheck, TrustedKeys, check_signature};
use cuma_skills::package::{check_id, parse_manifest};
use cuma_skills::validate_with;
use cuma_workspace::CommandGuard;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct SecurityInput {
    command: String,
    allow_destructive: bool,
    allowlist: Vec<String>,
    skill_id: String,
    skill_name: String,
    skill_version: String,
    skill_source: String,
    permissions: Vec<String>,
    trust_idx: u8,
    sig_kind: u8,
    digest: String,
    sig_line: String,
    toml_text: String,
}

const TRUST_LEVELS: [TrustLevel; 4] = [
    TrustLevel::Trusted,
    TrustLevel::Verified,
    TrustLevel::Community,
    TrustLevel::Untrusted,
];

fuzz_target!(|input: SecurityInput| {
    let config = SecurityConfig {
        allow_destructive_operations: input.allow_destructive,
        command_allowlist: input.allowlist.into_iter().take(8).collect(),
        ..SecurityConfig::default()
    };
    let guard = CommandGuard::new(&config);
    let verdict = guard.screen(&input.command);
    if input.command.trim().is_empty() {
        assert!(
            !verdict.is_allowed(),
            "empty command must always be refused"
        );
    }
    if !input.allow_destructive {
        assert!(!guard.screen("git reset --hard HEAD").is_allowed());
        assert!(!guard.screen("rm -rf /").is_allowed());
    }

    let _ = parse_manifest(&input.toml_text);
    if check_id(&input.skill_id).is_ok() {
        assert!(!input.skill_id.is_empty());
        assert!(input.skill_id.len() <= 64);
        assert!(!input.skill_id.starts_with('.'));
        assert!(!input.skill_id.contains(".."));
        assert!(!input.skill_id.contains('/'));
    }

    let _ = check_signature(&input.digest, &input.sig_line, &TrustedKeys::default());

    let claimed_trust = TRUST_LEVELS[usize::from(input.trust_idx) % TRUST_LEVELS.len()];
    let manifest = SkillManifest {
        id: SkillId::new("fuzz-skill"),
        name: input.skill_name,
        description: String::new(),
        version: input.skill_version,
        source: input.skill_source.clone(),
        capabilities: CapabilitySet::new(),
        requested_permissions: input.permissions.into_iter().take(16).collect(),
        checksum: None,
        signature: None,
        trust: claimed_trust,
    };

    let sig = match input.sig_kind % 4 {
        0 => Some(SignatureCheck::Absent),
        1 => Some(SignatureCheck::Valid {
            key: "acme".to_owned(),
        }),
        2 => Some(SignatureCheck::UnknownKey {
            key: "stranger".to_owned(),
        }),
        _ => Some(SignatureCheck::Invalid {
            reason: "tampered".to_owned(),
        }),
    };
    let evidence = Evidence {
        digest: Some(input.digest.clone()),
        published_digest: Some(input.digest),
        signature: sig,
    };

    let report = validate_with(&manifest, &evidence);
    assert!(
        report.trust >= claimed_trust,
        "manifest trust claim must never be upgraded: claimed={claimed_trust:?}, got={:?}",
        report.trust
    );
    if input
        .skill_source
        .trim()
        .to_ascii_lowercase()
        .starts_with("generated:")
    {
        assert_eq!(
            report.trust,
            TrustLevel::Untrusted,
            "generated skills must always remain Untrusted"
        );
    }
});
