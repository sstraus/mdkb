---
id: agent2-intune-pkg-postinstall-trust-60007
title: "Agent2 pkg postinstall fails: add-trusted-cert -60007"
entry_type: problem
source_type: auto_extracted
status: active
tags: [agent2, intune, mdm, macos, keychain, trust-settings, postinstall]
created_at: 1789724770
updated_at: 1789724770
---

SYMPTOM (2026-09-18, Boss's Mac, macOS 26 / Darwin 25.6): Intune installed Agent2Platform 1.74.0 pkg at 09:33:08; PackageKit 'Install Failed Code=112', no receipt, Lansweeper provisioning script loops 'package_not_installed', LaunchAgent never bootstrapped, capture extension never activated. Intune nonetheless reports ComplianceState Installed (bundle-id detection, IgnoreVersioning). ROOT CAUSE: postinstall -> agent2 capture-provision -> provision_macos_capture_trust -> 'security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain' (crates/core/src/ca_trust.rs:1191). authd: 'Fatal: interaction not allowed (session has no ui access)', 'Failed to authorize right com.apple.trust-settings.admin by client /usr/libexec/trustd ... (-60007)'. trustd demands user interaction for admin trust settings even for root; an installer postinstall has no UI session, so this is deterministic on macOS 15+/26, not a flake. Non-zero exit is mapped to TrustErrorKind::AccessDenied -> 'capture trust provision failed: AccessDenied' -> fail_state provider_crash -> exit 1. The 71-minute postinstall duration was NOT a hang: pmset log shows clamshell sleep at 09:33:43 with dark wakes only (09:49, 10:06, 10:13, 10:30, 10:41) until full wake 10:43:56; Intune killed installer (exit 15) at 10:41:24 during a dark wake, 68 min after start. SECONDARY: verify-agent2-daemon calls lipo/otool/swift, which are Xcode shims (each spawned xcodebuild as root); a customer Mac without CLT/Xcode cannot run the verifier. FOLLOW-UP: root trust cannot be installed from a pkg postinstall on current macOS; needs an MDM certificate payload or a user-session step.
