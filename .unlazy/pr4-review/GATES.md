# Gates: every review finding on PR #4 analysed, fixed or answered

Scope: the 20 inline review comments on italoag/cuma#4 (Copilot, llamapreview, Devin Review, Amazon Q). Each valid finding gets a fix proven by a named test; each comment gets a reply saying what was done; the pipeline runs green on the pushed head.

- [x] G0: this ledger states outcomes that can fail
  CHECK: node /Users/t798157/.config/devin/skills/unlazy/scripts/gate-lint.mjs .unlazy/pr4-review/GATES.md
  EXPECT: LINT OK
  EVIDENCE: automatic-evidence=v1; definition-sha256=3094bee385a07f2c487c389416f2ab1151ae56c6558311d154e7cc1722990dbc; exit=0; EXPECT=matched; output-sha256=48630b7361dd44ee870917b12c3d19b9d7bdea738aaca16bb04d4cab83b772d2; output-bytes=8; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G1: an archive from a copied sandbox that would write outside its unpack directory (absolute name, `..`, a member below a planted link) is refused before extraction
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox sync::tests::an_archive_reaching_outside_where_it_is_unpacked_is_refused
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=77ddb7e8cd731fa000a23e2018a6664d9489481226cf0403fba4b634d0cbcea3; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G2: names that are not variable names never reach a sourced env file (CUMA's and the Firecracker plugin's), and `-e NAME=value` shows only the name in errors
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox a_name_that_is_not_a_variable_name_never_reaches_the_shell a_failure_shows_variable_names_but_never_their_values && node .unlazy/pr4-review/checks/firecracker-env.mjs
  EXPECT: firecracker env names verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=b687e7e65e0d5ace8ed1ed892906ebae5392e2c6fe7ebdabc128b61984deef0a; exit=0; EXPECT=matched; output-sha256=6d01a48efd5c57336abd5a336615d87d2108acb8ab5183a358c2d293a3554a3b; output-bytes=117; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G3: a program that writes before reading a large input does not stall a launch step
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox process::tests::a_program_that_writes_before_reading_a_large_input_does_not_stall
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=d02478b51eba9212fac2b13a4353bcf73ece781abfc9c31a371ce20c17819d87; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G4: input that cannot be delivered ends the e2b and OpenSandbox relays with an error instead of leaving the agent waiting
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox e2b::tests::an_input_that_cannot_be_delivered_ends_the_relay opensandbox::tests::an_input_that_cannot_be_delivered_ends_the_relay
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=1d464a2341e5bc10d54531c7e97244592ec0a8430ebbaf13034f7ecc73309b1f; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G5: a container that cannot be removed fails the launch's finish; one already gone does not
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox docker::tests::a_container_that_cannot_be_removed_fails_the_finish
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=aabc0250d2d1b046bbcabe1353a408fe4e035bb6486765366ab6780bb9547784; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G6: with no native runtime working, the native provider claims no isolation
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox native::tests::with_no_runtime_the_native_provider_claims_no_isolation
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=7bd345deeaa9d7b7ef139585335000a8a77aee20f5c623dc3427e3f21b504747; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G7: a plugin's malformed open reply never leaks its session, a copying plugin without a session is refused, and a failed merge leaves the plugin's copy unreleased
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox plugin::tests::an_open_reply_with_an_empty_prefix_aborts_the_session plugin::tests::a_copying_plugin_that_returns_no_session_is_refused plugin::tests::a_failed_merge_leaves_the_plugins_copy_unreleased
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=ac5dce16040f51b3e11bdb138af0043b4d4cdf512886959a11636fad805e55f7; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G8: the Kubernetes probe creates a sandbox, runs `true` inside it and deletes it
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox kubernetes::tests::the_probe_runs_true_inside_a_sandbox_and_deletes_it
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=d16009f4b32f47f4123b0d06106215f379e4d6fa69f4c5441fc5f50a640092c5; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G9: with the workspace mounted, OpenSandbox also mounts the agent's state writable and its command paths read-only
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox opensandbox::tests::a_mounted_workspace_brings_the_state_and_the_command_paths
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=4b308a4910bca2b02e00fc304deaca6e0435ab3fd5bca1efec77d524474f3509; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G10: a failed turn whose copied work cannot be collected reports both failures
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-protocol-acp a_failed_turn_whose_work_cannot_be_collected_reports_both
  EXPECT: tests verified: cuma-protocol-acp
  EVIDENCE: automatic-evidence=v1; definition-sha256=1236d6b84eae0b6e528255e5cafcc836f610c06f0404fa7eb13edd9d53c51399; exit=0; EXPECT=matched; output-sha256=327dbb3490b2e432cebac4f1f88b4f0b9ed767fc6dcb0378303ff23403e3a4a0; output-bytes=96; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G11: a plugin agent is probed before a required network allowlist refuses it, so a plugin that enforces one registers
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-cli a_plugin_is_probed_before_an_allowlist_refuses_it
  EXPECT: tests verified: cuma-cli
  EVIDENCE: automatic-evidence=v1; definition-sha256=7bcb400704c5879429d0120e8fe782c3a4eb19bcb44391866a9316ca62c2eb36; exit=0; EXPECT=matched; output-sha256=5cd5e8635f30d38e08de99c5e6d7f0f1afa3b960f4b2b6a8c854dded99cbbfdc; output-bytes=78; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G12: a workspace below its repository's root is copied whole
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox sync::tests::a_workspace_below_the_repository_root_is_copied_whole
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=10d47134e18a068916d7e4c7194b5ddff57eeaa630e20eab6194d40f68a90fb7; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G13: a merge that cannot write every file writes none of them
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox sync::tests::a_merge_that_cannot_write_everything_writes_nothing
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=e4869d33e95ad501511ee358af7b7834697a45f12967a6b9b938404ee0cf6b31; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G14: the Sandboxes workflow lints clean and triggers on the CLI's launch wiring and native confinement
  CHECK: node .unlazy/pr4-review/checks/workflow.mjs
  EXPECT: workflow triggers verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=dc53288ecb7e05f59b5cd420e2d96ce1d8742806fad298e7efd4c6dca667d818; exit=0; EXPECT=matched; output-sha256=c67032c23c9883eae6d1d28d755eae11ae00d517c185da6c2d6c31fe106eb29a; output-bytes=65; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G15: the workspace is formatted
  CHECK: node .unlazy/sandbox-providers/checks/workspace.mjs fmt
  EXPECT: formatting verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=9e75e7ef4146207a6b5d12cedac05fe096df16b0403da2bab9c6d62ea4300304; exit=0; EXPECT=matched; output-sha256=2057daf36564229061fe8d3004df5bc8ab0aa714eb8a45a9e45720bc3b6d5792; output-bytes=20; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G16: clippy is clean, with and without the otel feature
  CHECK: node .unlazy/sandbox-providers/checks/workspace.mjs clippy
  EXPECT: clippy verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=7d4971891877d5df219c153ac2faa9d30b6e164fcbd21fc9e8eebc30e14bccbc; exit=0; EXPECT=matched; output-sha256=376fb4de3f2e0afe667fb4e70f9d80214ce7dcbfe9a9b7ac71aeec768c5c1682; output-bytes=16; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G17: the whole suite passes
  CHECK: node .unlazy/sandbox-providers/checks/workspace.mjs tests
  EXPECT: test suite verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=eb0f248e376909959500336227f173d02a659adab1d15f833677e80017824919; exit=0; EXPECT=matched; output-sha256=829f191911d76403d4e85fd74fd69535130730e6772409bb0cee71b084e02f29; output-bytes=52; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G18: every inline review comment on PR #4 has a reply from italoag
  CHECK: node .unlazy/pr4-review/checks/replies.mjs
  EXPECT: every review comment answered
  EVIDENCE: automatic-evidence=v1; definition-sha256=c2a4b69708a1c894f1068be465c2c0a2ba86a93d452b8129ce4f1a03a60fbacb; exit=0; EXPECT=matched; output-sha256=d607b1949277311c43841815905886f5e32b6139011fe6d6a7e4e73615e4e682; output-bytes=63; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G20: a sandbox's archive is read by CUMA itself, so the names it judges are the names it writes (a newline included): absolute names, `..`, members below a link, links and hard links reaching outside are refused, into a directory it creates
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox sync::tests::an_archive_reaching_outside_where_it_is_unpacked_is_refused sync::tests::a_name_with_a_newline_is_judged_whole sync::tests::long_names_and_pax_paths_are_read_as_tar_writes_them sync::tests::an_archive_the_system_tar_writes_round_trips
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=0a468728c156f66318d8a163d936c7b5277605c56943de7e9cd26c4b2055bf2c; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G21: a link the sandbox created or changed that points outside the workspace is not applied
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox sync::tests::a_link_pointing_outside_the_workspace_is_not_applied
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=997ba64db7ad5de4ad881307d04ebdf67c2176fbab0657eaf74abda85d590fba; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G22: a merge never writes through, over, or removes a file it did not create beside its targets
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox sync::tests::a_merge_never_touches_a_file_it_did_not_create
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=c1ba9f2a0740c928ee17547f5fe0e0e6ecb9e2f5ea7182efd09c9b56fa77da75; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G23: a command path inside the agent's state is not mounted read-only over that state
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-sandbox tests::readable_directories_leave_out_what_the_workspace_and_state_mount
  EXPECT: tests verified: cuma-sandbox
  EVIDENCE: automatic-evidence=v1; definition-sha256=cba3e1f4992861b9835c6bb30ca8c212947c851c4b08a1c94148398fe9b8d0ab; exit=0; EXPECT=matched; output-sha256=aba5d1aad28e3139be7c79b94cb43610192f9f8ef83dec2f318c4bce42acf40e; output-bytes=86; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G24: the explanation for having no agents probes a plugin before blaming it
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-cli the_explanation_probes_a_plugin_before_blaming_it
  EXPECT: tests verified: cuma-cli
  EVIDENCE: automatic-evidence=v1; definition-sha256=0c28c4465be296229efea0465dd86d06b4ec2cd2541204a6a0a0a0f914319418; exit=0; EXPECT=matched; output-sha256=5cd5e8635f30d38e08de99c5e6d7f0f1afa3b960f4b2b6a8c854dded99cbbfdc; output-bytes=78; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G25: reading a global configuration from its old location is reported once logging is set up, not lost before it
  CHECK: node .unlazy/sandbox-providers/checks/tests.mjs cuma-config a_global_file_left_at_the_old_location_is_still_read && node .unlazy/pr4-review/checks/deferred-warnings.mjs
  EXPECT: deferred warnings verified
  EVIDENCE: automatic-evidence=v1; definition-sha256=6ac3f489a4b36b5414f5142abc365f9f8e4a7831e337c3a56f31add747ac2c51; exit=0; EXPECT=matched; output-sha256=7acd5a1565c59bd7779702a2c9b17cf0816898a8f4d572706f20f995462a41b0; output-bytes=188; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G26: every inline review comment on PR #5 has a reply from italoag
  CHECK: node .unlazy/pr4-review/checks/replies.mjs 5
  EXPECT: every review comment answered
  EVIDENCE: automatic-evidence=v1; definition-sha256=c1b870b107f200263bbc944b4e8183d0a4dc322e1ad021e8525e233dbab64c62; exit=0; EXPECT=matched; output-sha256=a094c59069dda83a3e61b1f90f905b50280e5d7b8b22e102d568c2b3d35267fe; output-bytes=62; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries

- [x] G19: every workflow run on the head of PR #5 (#4 was merged before its review was addressed; #5 carries the fixes) concluded successfully
  CHECK: node .unlazy/pr4-review/checks/pipeline.mjs 5
  EXPECT: pipeline green
  EVIDENCE: automatic-evidence=v1; definition-sha256=4f9ec429c08e8bda18a8129bf2cf7c79b3eb91d1a38edb3b4bbf700dc8f863ab; exit=0; EXPECT=matched; output-sha256=e8b88239bd739a78822a2ca74f554be449ed6f5cef5edbc07a4ee033cc948519; output-bytes=64; shell=/bin/sh; cwd=/Users/t798157/Projects/italo/cuma; path=b76acaf321a9/80 entries
