//! Verifying a skill: the cost paid on every install and update.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use criterion::{Criterion, criterion_group, criterion_main};
use cuma_skills::integrity::{TrustedKeys, check_signature, content_digest, public_key, sign};
use std::hint::black_box;

fn integrity(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("skill.toml"),
        "id = 'bench'\nname = 'Bench'\n",
    )
    .unwrap();
    for i in 0..100 {
        let sub = dir.path().join(format!("docs/part-{}", i % 10));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(format!("{i}.md")), "x".repeat(4096)).unwrap();
    }

    c.bench_function("skills/digest-100-files-400KiB", |b| {
        b.iter(|| black_box(content_digest(dir.path()).unwrap()));
    });

    let key = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
    let keys = TrustedKeys::from_config(&std::collections::BTreeMap::from([(
        "k".to_owned(),
        public_key(&key),
    )]))
    .unwrap();
    let digest = content_digest(dir.path()).unwrap();
    let line = sign(&digest, "k", &key);
    c.bench_function("skills/verify-signature", |b| {
        b.iter(|| black_box(check_signature(&digest, &line, &keys)));
    });
}

criterion_group!(benches, integrity);
criterion_main!(benches);
