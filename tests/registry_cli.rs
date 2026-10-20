//! Invalid registry alternatives fail before operational state or hook dispatch.

use std::process::{Command, Stdio};

use symposium::hook::{HookAgent, HookEvent, failure_response};

#[test]
fn invalid_registry_sources_preserve_configuration_and_deny_permission_hooks() {
    const CANARY: &str = "REGISTRY_SOURCE_PRIVATE_CANARY";
    for source in [
        format!("git = 'https://github.com/example/{CANARY}'\npath = 'local'\n"),
        String::new(),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let original = format!(
            "auto-sync = false\nauto-update = 'off'\n\
             [defaults]\nsymposium-recommendations = false\nuser-plugins = false\n\
             [[registry]]\nname = 'invalid'\n{source}"
        );
        let config = home.join("config.toml");
        std::fs::write(&config, &original).unwrap();

        let command = || {
            let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-agents"));
            command
                .current_dir(temp.path())
                .env("SYMPOSIUM_HOME", &home)
                .env("HOME", temp.path())
                .env("USERPROFILE", temp.path())
                .env("CARGO_NET_OFFLINE", "true")
                .env_remove("SYMPOSIUM_ENABLE_AGENT_TESTING")
                .env_remove("SYMPOSIUM_TEST_AGENT")
                .stdin(Stdio::null());
            command
        };
        let status = command().arg("status").output().unwrap();
        assert!(!status.status.success());
        assert!(status.stdout.is_empty());
        assert!(String::from_utf8_lossy(&status.stderr).contains("registry"));
        assert!(!String::from_utf8_lossy(&status.stderr).contains(CANARY));

        for agent in [
            HookAgent::Claude,
            HookAgent::Codex,
            HookAgent::Copilot,
            HookAgent::Kiro,
            HookAgent::Antigravity,
        ] {
            let output = command()
                .args(["hook", agent.as_str(), "pre-tool-use"])
                .output()
                .unwrap();
            let expected = failure_response(agent, HookEvent::PreToolUse).unwrap();
            assert_eq!(output.status.code(), Some(expected.exit_code.into()));
            assert_eq!(output.stdout, expected.stdout);
            assert!(!String::from_utf8_lossy(&output.stderr).contains(CANARY));
        }
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
        assert!(!home.join("state.toml").exists());
    }
}
