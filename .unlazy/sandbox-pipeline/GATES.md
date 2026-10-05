# Gates: a pipeline that brings up every sandbox and runs its live test

Scope: a GitHub Actions pipeline, triggered by pull requests that touch the sandbox code and by hand, that installs or builds each sandbox, runs CUMA's live test against it, and publishes the images it needs to ghcr.io/italoag/cuma-*; self-hosted jobs cover what hosted runners cannot run.

- [x] G0: this ledger states outcomes that can fail
  CHECK: node /Users/t798157/.config/devin/skills/unlazy/scripts/gate-lint.mjs .unlazy/sandbox-pipeline/GATES.md
  EXPECT: LINT OK
  EVIDENCE: automatic-evidence=v1; definition-sha256=17927ed6d7c642870bf6c476a71e6cf65922d9fbc6075d5a7ba69d6a7051aa14; exit=0; EXPECT=matched; output-sha256=209283512a69eba47c77cd64cf528d09a2c2803ddfd57a14b1350c58540947f4; output-bytes=293; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G1: the pipeline's workflows pass actionlint
  CHECK: actionlint .github/workflows/sandboxes.yml .github/workflows/sandbox-images.yml && echo "workflows lint clean"
  EXPECT: workflows lint clean
  EVIDENCE: automatic-evidence=v1; definition-sha256=6a08e13cf793cc3fbdc090a07037c6a9c33a85622eeb71fc76a6a8759f3683dc; exit=0; EXPECT=matched; output-sha256=6e6e6c2d88dd8cb07973700fc0d860095ea3067c55f9c9c24ac115dbace3e419; output-bytes=21; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G2: every sandbox kind has a live test and a pipeline job that runs it, on the agreed triggers, with self-hosted jobs gated by repository variables
  CHECK: node .unlazy/sandbox-pipeline/checks/coverage.mjs
  EXPECT: pipeline coverage verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=6c989f1445216340f83b1be50bcfc8dbad4c55f876e36b18bcbfc1d0fe671794; exit=0; EXPECT=matched; output-sha256=e8fbff3866c383b68d7338f72ebdf829b820397b4258550c3ef4fc9e4da3d7e7; output-bytes=27; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G3: every live test compiles and skips cleanly when its environment is absent
  CHECK: node .unlazy/sandbox-pipeline/checks/skips.mjs
  EXPECT: live tests skip cleanly
  EVIDENCE: automatic-evidence=v1; definition-sha256=87e3ccbc6b91747d1cdcfcbc37bd74f63913f7994afa4b39ed08a37a30fc2fda; exit=0; EXPECT=matched; output-sha256=c61535eb9a6c5237bfbfbfd170a147cd2f4c45853aef194ed4b3c064274fca05; output-bytes=24; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G4: the Firecracker plugin's relay discards boot output, delivers the agent's JSON-RPC both ways, and ends when stdin ends, against a stand-in VM
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox the_firecracker_relay_hides_the_boot_and_carries_a_whole_turn
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=54dd464e27b6bcceb807af64b0bd024fe132b2a60d94c50951bac283d607ec81; exit=0; EXPECT=matched; output-sha256=b809ae4c6c00aea0739c592bb2c78ee0b4a1cd6cd8f68ae98a4bcd7fc5b8e20d; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G5: the image definitions and the mirror list name only pinned, existing sources and publish under ghcr.io/italoag/cuma-*
  CHECK: node .unlazy/sandbox-pipeline/checks/images.mjs
  EXPECT: images verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=34fa599c778503fcf5937828e3c57a82bed9a41fdbca86ae9a4046b79ce8c9b8; exit=0; EXPECT=matched; output-sha256=61fa7cba14ec9d492f5f417c18059186e40a6a77484927f583c151c1b70378d9; output-bytes=16; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G6: the providers verifiable on this machine still pass live (microsandbox, wasmer, agentOS)
  CHECK: node .unlazy/sandbox-providers/checks/live-microsandbox.mjs && node .unlazy/sandbox-providers/checks/live-wasmer.mjs && node .unlazy/sandbox-providers/checks/live-agentos.mjs
  EXPECT: live agentos verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=e469614c51744809bfb7a98cabc8744fb4b05c3c267b13932abaf51b1ff62e2c; exit=0; EXPECT=matched; output-sha256=f012af5d9c3c6e88b0cfa9329086f030a04fb2415ed90e0cf46e67adc7252112; output-bytes=70; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [x] G7: the workspace is formatted, clippy is clean and the whole suite passes
  CHECK: node .unlazy/sandbox-providers/checks/workspace.mjs fmt && node .unlazy/sandbox-providers/checks/workspace.mjs clippy && node .unlazy/sandbox-providers/checks/workspace.mjs tests
  EXPECT: test suite verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=7ce99e6144a9d92f56200fbde1ede2478f1d4276eadbe2151dbec0732bd8ef6c; exit=0; EXPECT=matched; output-sha256=953d26034a68f90e179a58f432d97711b3a1a40dafb677beb42e252d9cde0f95; output-bytes=88; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=ac2163596ad4/79 entries

- [ ] G8: the hosted jobs of the pipeline have run green on GitHub Actions (docker, microsandbox, wasmer, agentOS, kubernetes, opensandbox, firecracker)
  EVIDENCE: pending

- [ ] G9: the self-hosted jobs have run green on registered runners (arcbox, cubesandbox)
  EVIDENCE: pending
