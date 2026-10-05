# Gates: pluggable agent sandbox providers

OWNS: crates/cuma-sandbox/**, crates/cuma-core/src/ports.rs, crates/cuma-config/src/**, crates/cuma-protocol-acp/src/**, crates/cuma-workspace/src/confine.rs, crates/cuma-cli/**, plugins/sandbox/**, docs/**, Cargo.toml, Cargo.lock, README.md, CLAUDE.md, IMPLEMENTATION_PLAN.md

Scope: any coding agent runs under a sandbox the operator chooses — native, container, microVM, Wasm, Kubernetes or remote — through built-in providers for the eight requested sandboxes and a plugin protocol for anything else, with a documented plan.

- [x] G0: this ledger states outcomes that can fail
  CHECK: node /Users/t798157/.config/devin/skills/unlazy/scripts/gate-lint.mjs .unlazy/sandbox-providers/GATES.md
  EXPECT: LINT OK
  EVIDENCE: automatic-evidence=v1; definition-sha256=d6f45044be599d9d89f9fdd01ca830a1c3f5c5484b50f0f538b56632011eda03; exit=0; EXPECT=matched; output-sha256=ef6d636f45415ce92a3ae7c80e7c529b2b5ddb03a8753fd459e5a9d6b939c9e4; output-bytes=177; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G1: configuration accepts every sandbox kind, rejects unknown keys, and agents name their sandbox, state and env
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-config every_sandbox_kind an_unknown_key_in_a_sandbox an_agent_naming_an_undefined_sandbox agent_state_and_env
  EXPECT: tests verified: cuma-config
  EVIDENCE: automatic-evidence=v1; definition-sha256=4c5f6b0318a703cab1e8eb38d7c6d5f6eba824564f3049e546ce35fe9375108e; exit=0; EXPECT=matched; output-sha256=bf27026814213959aa343e26ebbfa48e781a1517ce80b358b06f375d85b33a4b; output-bytes=84; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G2: every provider kind, the registry and the workspace sync have passing tests
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox native:: docker:: microsandbox:: arcbox:: kubernetes:: e2b:: opensandbox:: wasmer:: command:: plugin:: sync:: registry:: bridge::
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=2d696ab0e6352589904c2be5d2d9d09eb16c95ea5c8c09c079e9ff6ad45142ed; exit=0; EXPECT=matched; output-sha256=90709524b15bca3121ca8feaad87ad6626d6d9ff440a39c69623b205fd610d1a; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G3: the ACP adapter finishes every launch it opens and aborts abandoned ones
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-protocol-acp a_launch_is_finished_after_the_turn a_launch_is_aborted_when_the_turn_is_abandoned
  EXPECT: tests verified: cuma-protocol-acp
  EVIDENCE: automatic-evidence=v1; definition-sha256=12e1b82a2497f7788d715ee8cc7fdf3de792a9192eb52c0336beb5ef9bdb18de; exit=0; EXPECT=matched; output-sha256=629ba6fc3d660a8fcceefb769cf8ba17e81aa84024ccb801513afc9b6a1770f4; output-bytes=96; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G4: the cuma binary lists every provider with its capabilities and refuses unknown or broken ones
  CHECK: node .unlazy/sandbox-providers/checks/cli.mjs
  EXPECT: cli sandbox verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=8fee361b8d55b081b100392ec5a957f40af46c0b22f438a5699d645ee384ed00; exit=0; EXPECT=matched; output-sha256=ca842c0ae713363ec222963913308324ba65fcbe583e089adc80dcc1c4dbb1f8; output-bytes=41; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G5: a real container engine runs an ACP agent through the docker provider and leaves no container behind
  CHECK: node .unlazy/sandbox-providers/checks/live-docker.mjs
  EXPECT: live docker verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=98c5616ffd2e221e1fc5954b2ed67aa134445a95d87a4bede30605eec2374af1; exit=0; EXPECT=matched; output-sha256=242351dbc5c631a3b4a953c6365c72b8ff8dc395eb93c73796c0fbcdb235ada9; output-bytes=21; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G6: the plan and operator docs cover all eight sandboxes, every kind, the ADR and the reference plugins
  CHECK: node .unlazy/sandbox-providers/checks/docs.mjs
  EXPECT: docs coverage verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=da0179f29e03cf482aeb369ae1c1872b9c10ca643a1de0665bd16a28ac949f35; exit=0; EXPECT=matched; output-sha256=7f2d085d97739d435cf76bb84b86f2e1820481eb7b541c3ff75d66b8b8615715; output-bytes=70; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G7: the workspace is formatted
  CHECK: node .unlazy/sandbox-providers/checks/workspace.mjs fmt
  EXPECT: formatting verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=9e75e7ef4146207a6b5d12cedac05fe096df16b0403da2bab9c6d62ea4300304; exit=0; EXPECT=matched; output-sha256=2057daf36564229061fe8d3004df5bc8ab0aa714eb8a45a9e45720bc3b6d5792; output-bytes=20; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G8: clippy is clean with warnings denied, with and without the otel feature
  CHECK: node .unlazy/sandbox-providers/checks/workspace.mjs clippy
  EXPECT: clippy verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=7d4971891877d5df219c153ac2faa9d30b6e164fcbd21fc9e8eebc30e14bccbc; exit=0; EXPECT=matched; output-sha256=376fb4de3f2e0afe667fb4e70f9d80214ce7dcbfe9a9b7ac71aeec768c5c1682; output-bytes=16; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G9: the whole test suite passes and has grown beyond the measured baseline
  CHECK: node .unlazy/sandbox-providers/checks/workspace.mjs tests
  EXPECT: test suite verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=eb0f248e376909959500336227f173d02a659adab1d15f833677e80017824919; exit=0; EXPECT=matched; output-sha256=a81275314d4eb260d03a06ca8c517e34e5be0fb944b38d211316b4fca786ee48; output-bytes=52; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G10: a real microVM runs an ACP agent through the microsandbox provider and leaves no VM behind
  CHECK: node .unlazy/sandbox-providers/checks/live-microsandbox.mjs
  EXPECT: live microsandbox verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=80249afcf20ba1529a8eb0b0521c20674ac5b76c8203132bd18401111380447d; exit=0; EXPECT=matched; output-sha256=3314fea1ad4493f3fcbcfc78fa4410a381ccef28c1f99b73bc87d596f1f6bcba; output-bytes=27; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [ ] G11: providers whose tools or infrastructure are not available here have been exercised live (ArcBox, agent-sandbox on a cluster, OpenSandbox server, CubeSandbox, Firecracker)
  EVIDENCE: pending

- [x] G12: a real WASIX sandbox runs an ACP agent through the wasmer provider with the workspace mapped
  CHECK: node .unlazy/sandbox-providers/checks/live-wasmer.mjs
  EXPECT: live wasmer verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=0da8920a9fe347278a64da2602d6c649f51021296d32b8f89f3e192a650318f9; exit=0; EXPECT=matched; output-sha256=6a71c91b13af31f8cf3aed99e73fdf2d829db7babe00c8018e1960559207d837; output-bytes=21; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G13: a real agentOS VM runs an ACP agent through the reference plugin and writes the workspace as the user
  CHECK: node .unlazy/sandbox-providers/checks/live-agentos.mjs
  EXPECT: live agentos verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=69bde0088beebdd6eb92f9da12dad9cc5a9208b8f5a0eacdb9f94afaa2a3f250; exit=0; EXPECT=matched; output-sha256=44c90f224b565d46fe0de1aed7d7e53994ec18befb1df1b34a7f02a748d47945; output-bytes=22; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries
