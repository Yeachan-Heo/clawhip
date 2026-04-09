use std::fs;
use std::path::Path;

const DOCS_WITHOUT_LEGACY_SURFACES: &[&str] = &[
    "SKILL.md",
    "README.md",
    "docs/canonical-contract-cleanup.md",
    "docs/native-event-contract.md",
    "docs/live-verification.md",
    "docs/event-contract-v1.md",
    "integrations/omx/README.md",
    "skills/omc/SKILL.md",
    "skills/omx/SKILL.md",
];

const FORBIDDEN_SURFACES: &[&str] = &[
    "clawhip omx",
    "clawhip omc",
    "/api/omx/hook",
    "skills/omx",
    "skills/omc",
    "integrations/omx",
];

const REQUIRED_SHARED_EVENTS: &[&str] = &[
    "SessionStart",
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "Stop",
];

fn read_repo_file(relative: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(relative)).expect("read doc fixture")
}

#[test]
fn provider_native_docs_drop_legacy_public_surfaces() {
    for relative in DOCS_WITHOUT_LEGACY_SURFACES {
        let contents = read_repo_file(relative).to_lowercase();
        for forbidden in FORBIDDEN_SURFACES {
            assert!(
                !contents.contains(&forbidden.to_lowercase()),
                "{relative} still references forbidden legacy surface: {forbidden}"
            );
        }
    }
}

#[test]
fn provider_native_contract_docs_list_all_shared_events() {
    for relative in [
        "SKILL.md",
        "README.md",
        "docs/native-event-contract.md",
        "docs/live-verification.md",
        "docs/event-contract-v1.md",
    ] {
        let contents = read_repo_file(relative);
        for event in REQUIRED_SHARED_EVENTS {
            assert!(
                contents.contains(event),
                "{relative} is missing shared event {event}"
            );
        }
        assert!(
            contents.contains("clawhip native hook"),
            "{relative} should reference the generic provider-native ingress"
        );
    }
}

#[test]
fn root_skill_guides_provider_native_install_and_routing() {
    let contents = read_repo_file("SKILL.md");
    for needle in [
        "clawhip hooks install --all --scope global --force",
        ".codex/hooks.json",
        ".claude/settings.json",
        ".clawhip/project.json",
        "repo_name",
        "claude-code",
    ] {
        assert!(
            contents.contains(needle),
            "SKILL.md should mention {needle}"
        );
    }
}
