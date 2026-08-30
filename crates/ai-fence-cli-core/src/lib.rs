pub mod agent_launcher;
pub mod auth;
pub mod config;
pub mod proxy;

#[cfg(test)]
mod boundary_tests {
    #[test]
    fn launcher_core_does_not_depend_on_private_backend() {
        let manifest = include_str!("../Cargo.toml");
        assert!(!manifest.contains(concat!("ai-fence-", "backend")));

        let sources = [
            include_str!("agent_launcher.rs"),
            include_str!("auth.rs"),
            include_str!("config.rs"),
            include_str!("proxy.rs"),
        ];
        for source in sources {
            for private_path in [
                concat!("ai_fence_", "backend"),
                "crate::adapter",
                "crate::canonical",
                "crate::pipeline",
                "crate::redaction",
                "crate::routes",
                "crate::state",
            ] {
                assert!(
                    !source.contains(private_path),
                    "launcher source imports private path {private_path}"
                );
            }
        }
    }
}
