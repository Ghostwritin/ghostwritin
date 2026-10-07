//! The binary without a network: argument errors and a missing model key.

use std::process::Command;

fn ghostwritin(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ghostwritin"))
        .args(args)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("the binary runs")
}

#[test]
fn without_a_model_key_it_says_so_and_fails() {
    let dir = std::env::temp_dir().join(format!("ghostwritin-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let draft = dir.join("draft.md");
    std::fs::write(&draft, "A draft with 18% growth.\n").unwrap();
    let out = ghostwritin(&["rewrite", draft.to_str().unwrap(), "--voice", "casual"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no model key"), "{stderr}");
    assert!(out.stdout.is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unknown_voice_is_refused() {
    let out = ghostwritin(&["rewrite", "-", "--voice", "pirate"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("expected casual, professional, academic or my_voice")
    );
}

#[test]
fn help_lists_the_commands() {
    let out = ghostwritin(&["--help"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("rewrite") && stdout.contains("voice"));
}
